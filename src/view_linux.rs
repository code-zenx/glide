//! The sending end of View Screen on Linux: this machine's screen as H.264
//! and its sound as Opus (raw PCM without `opusenc`), for the Mac to show
//! fullscreen.
//!
//! - **Hyprland**: frames come from wlr-screencopy (`capture_wayland.rs`). A
//!   box with no screen plugged in gets a headless monitor for as long as the
//!   Mac watches, so there is something to draw on and to capture.
//! - **X11**: `ximagesrc` captures the whole root window.
//!
//! H.264, not HEVC: VA-API HEVC encoding crashed Sunshine on anorak's UHD 630.
//! Sound is captured the way Sunshine does it: a null sink becomes the default
//! output, and its monitor is recorded. Opus takes it from about 1.5 Mbit/s
//! of PCM to 128 kbit/s, which matters over Tailscale; PCM stays as the
//! fallback because `opusenc` comes with gst-plugins-base, not always there.

use std::process::Command;
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::display::{Frame, Slot};
use crate::proto::ScreenMode;

/// The headless monitor made for a box with no screen, by this name.
const HEADLESS: &str = "GLIDE";
/// The null sink whose monitor is recorded.
const SINK: &str = "glide_view";
const BITRATE_KBPS: &str = "30000";
/// X11 has no refresh rate to ask Hyprland for.
const X11_HZ: u32 = 60;
/// What each encoder is asked for, where it has the setting at all.
const TUNING: [(&str, &str); 3] = [
    ("bitrate", BITRATE_KBPS),
    ("b-frames", "0"),
    ("target-usage", "7"),
];
/// Byte stream, one access unit per buffer, SPS and PPS before every
/// keyframe: what the Mac's parser expects.
const TAIL: &str =
    "h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
                    ! appsink name=sink sync=false";
const PCM: &str = "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";
/// 10 ms packets, CELT only (`restricted-lowdelay` drops the speech modes'
/// look-ahead): the least delay `opusenc` has, at a bitrate where music
/// sounds like the PCM it replaces.
const OPUS_TUNING: [(&str, &str); 3] = [
    ("bitrate", "128000"),
    ("frame-size", "10"),
    ("audio-type", "restricted-lowdelay"),
];

/// Whether this machine can be viewed: Hyprland always, X11 with `ximagesrc`.
/// Asked once: it does not change while the daemon runs.
pub fn can_capture() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(|| {
        hyprland()
            || (std::env::var_os("DISPLAY").is_some()
                && gst::init().is_ok()
                && gst::ElementFactory::find("ximagesrc").is_some())
    })
}

fn hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

enum Source {
    /// A Hyprland output, by name.
    Wayland(String),
    X11,
}

/// The screen to send. Dropping it removes the headless monitor, if one was
/// made for it.
pub struct Screen {
    pub mode: ScreenMode,
    source: Source,
    headless: bool,
}

impl Screen {
    /// Check everything needed is installed and pick the screen. Blocking: it
    /// may wait a moment for a new headless monitor to appear.
    pub fn prepare() -> Result<Self> {
        gst::init()?;
        encoder()?;
        require("h264parse", "gst-plugins-bad")?;
        if hyprland() {
            prepare_hyprland()
        } else {
            prepare_x11()
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.headless {
            remove_headless();
        }
    }
}

fn prepare_hyprland() -> Result<Screen> {
    let created = crate::display_linux::focused_monitor().is_err();
    if created {
        hyprctl(&["output", "create", "headless", HEADLESS])?;
    }
    let found = wait_for_monitor();
    if found.is_err() && created {
        remove_headless();
    }
    let (output, mode) = found?;
    // Ours if we made it now, or if a crash left the last one behind.
    let headless = output == HEADLESS;
    Ok(Screen {
        mode,
        source: Source::Wayland(output),
        headless,
    })
}

/// A new headless monitor takes a moment to be listed.
fn wait_for_monitor() -> Result<(String, ScreenMode)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match crate::display_linux::focused_monitor() {
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            found => return found,
        }
    }
}

fn remove_headless() {
    if let Err(e) = hyprctl(&["output", "remove", HEADLESS]) {
        tracing::warn!("cannot remove the headless monitor: {e:#}");
    }
}

/// `ximagesrc` captures the root window, which spans every monitor.
fn prepare_x11() -> Result<Screen> {
    require("ximagesrc", "gst-plugins-good")?;
    let screens = crate::screens::local();
    let w = screens
        .iter()
        .map(|s| s.x + s.w as i32)
        .max()
        .context("xrandr lists no monitor")?;
    let h = screens
        .iter()
        .map(|s| s.y + s.h as i32)
        .max()
        .context("xrandr lists no monitor")?;
    Ok(Screen {
        mode: ScreenMode {
            w: w as u32,
            h: h as u32,
            hz: X11_HZ,
        },
        source: Source::X11,
        headless: false,
    })
}

fn require(element: &str, package: &str) -> Result<()> {
    match gst::ElementFactory::find(element) {
        Some(_) => Ok(()),
        None => bail!("install {package} (for {element})"),
    }
}

/// The hardware encoder when there is one, with the GPU converting colours
/// for it (`vapostproc`, same plugin) instead of the CPU; x264 on the CPU
/// otherwise.
fn encoder() -> Result<&'static str> {
    if gst::ElementFactory::find("vah264enc").is_some() {
        Ok("vapostproc ! vah264enc name=enc")
    } else if gst::ElementFactory::find("x264enc").is_some() {
        Ok("videoconvert n-threads=0 ! x264enc name=enc tune=zerolatency speed-preset=ultrafast")
    } else {
        bail!("install gst-plugin-va or gst-plugins-ugly (for an H.264 encoder)")
    }
}

/// The screen, encoded and dropped into the slot frame by frame.
pub struct Encoder {
    capture: Option<crate::capture_wayland::Capture>,
    pipeline: gst::Pipeline,
}

impl Encoder {
    pub fn start(screen: &Screen, slot: Arc<Slot>) -> Result<Self> {
        let source = match &screen.source {
            // Blocks the capture thread while a frame waits, so frames are
            // taken no faster than they are encoded.
            Source::Wayland(_) => {
                "appsrc name=src is-live=true format=time do-timestamp=true block=true".to_string()
            }
            Source::X11 => format!(
                "ximagesrc show-pointer=true use-damage=false ! video/x-raw,framerate={}/1",
                screen.mode.hz
            ),
        };
        let pipeline = launch(&format!(
            "{source} ! {} ! {TAIL}",
            encoder()?
        ))?;
        let enc = pipeline.by_name("enc").context("no encoder")?;
        for (name, value) in TUNING {
            if enc.find_property(name).is_some() {
                enc.set_property_from_str(name, value);
            }
        }
        appsink(&pipeline)?.set_callbacks(frames_into(slot, enc));
        crate::display_linux::watch(&pipeline);
        pipeline.set_state(gst::State::Playing)?;
        let capture = match &screen.source {
            Source::Wayland(output) => {
                let src = pipeline
                    .by_name("src")
                    .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
                    .context("no appsrc")?;
                Some(crate::capture_wayland::Capture::start(
                    output,
                    src,
                    screen.mode.hz,
                )?)
            }
            Source::X11 => None,
        };
        Ok(Self { capture, pipeline })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // The pipeline first: a capture thread blocked pushing a frame into it
        // is let go, so joining that thread cannot hang.
        let _ = self.pipeline.set_state(gst::State::Null);
        drop(self.capture.take());
    }
}

/// Each encoded frame into the slot. A keyframe asked for (a new receiver, or
/// one that fell behind) is asked of the encoder here, the next time it
/// hands a frame over.
fn frames_into(slot: Arc<Slot>, enc: gst::Element) -> gst_app::AppSinkCallbacks {
    gst_app::AppSinkCallbacks::builder()
        .new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
            let key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let data = buffer
                .map_readable()
                .map_err(|_| gst::FlowError::Error)?
                .as_slice()
                .to_vec();
            slot.push(Frame { key, data });
            if slot.take_key_request() {
                let ask = gst::Structure::builder("GstForceKeyUnit")
                    .field("all-headers", true)
                    .build();
                let _ = enc.send_event(gst::event::CustomUpstream::new(ask));
            }
            Ok(gst::FlowSuccess::Ok)
        })
        .build()
}

/// The sound: everything played here goes to a null sink, and that sink's
/// monitor is recorded. Dropping it puts the old default output back.
pub struct Sound {
    pipeline: gst::Pipeline,
    module: String,
    previous: Option<String>,
    /// Whether `chunks` gets Opus packets rather than PCM.
    pub opus: bool,
}

impl Sound {
    /// `chunks` gets one 10 ms Opus packet at a time, or about 10 ms of PCM
    /// without `opusenc`; when it is full, the chunk is dropped rather than
    /// waited for.
    pub fn start(chunks: tokio::sync::mpsc::Sender<Vec<u8>>) -> Result<Self> {
        gst::init()?;
        let source = format!("pulsesrc device={SINK}.monitor buffer-time=20000 latency-time=10000");
        let opus = gst::ElementFactory::find("opusenc").is_some();
        let encode = if opus { " ! opusenc name=opus" } else { "" };
        let pipeline = launch(&format!(
            "{source} ! audioconvert ! audioresample ! {PCM}{encode} ! appsink name=sink sync=false"
        ))?;
        if let Some(enc) = pipeline.by_name("opus") {
            for (name, value) in OPUS_TUNING {
                if enc.find_property(name).is_some() {
                    enc.set_property_from_str(name, value);
                }
            }
        }
        appsink(&pipeline)?.set_callbacks(chunks_into(chunks));
        // A crash mid-view leaves the null sink the default; never go back to it.
        let previous = pactl(&["get-default-sink"])
            .ok()
            .filter(|sink| sink != SINK);
        let module = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
        ])?;
        // From here on, dropping `sound` undoes the sink, whatever fails next.
        let sound = Self {
            pipeline,
            module,
            previous,
            opus,
        };
        pactl(&["set-default-sink", SINK])?;
        sound.pipeline.set_state(gst::State::Playing)?;
        Ok(sound)
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(previous) = &self.previous {
            let _ = pactl(&["set-default-sink", previous]);
        }
        if let Err(e) = pactl(&["unload-module", &self.module]) {
            tracing::warn!("cannot remove the {SINK} sink: {e:#}");
        }
    }
}

fn chunks_into(chunks: tokio::sync::mpsc::Sender<Vec<u8>>) -> gst_app::AppSinkCallbacks {
    gst_app::AppSinkCallbacks::builder()
        .new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
            // opusenc's OpusHead and OpusTags: the Mac's decoder is set up by
            // the stream's tag instead, and would only fail on them.
            if buffer.flags().contains(gst::BufferFlags::HEADER) {
                return Ok(gst::FlowSuccess::Ok);
            }
            let chunk = buffer
                .map_readable()
                .map_err(|_| gst::FlowError::Error)?
                .as_slice()
                .to_vec();
            // Full: the network is behind. A click now beats a delay forever.
            let _ = chunks.try_send(chunk);
            Ok(gst::FlowSuccess::Ok)
        })
        .build()
}

fn launch(description: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))
}

fn appsink(pipeline: &gst::Pipeline) -> Result<gst_app::AppSink> {
    pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .context("no appsink")
}

fn hyprctl(args: &[&str]) -> Result<String> {
    run("hyprctl", args)
}

fn pactl(args: &[&str]) -> Result<String> {
    run("pactl", args)
}

/// hyprctl exits 0 even when it refuses; the monitor that then never appears
/// is what says so.
fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
