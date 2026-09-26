//! The receiving end of an extended display on Hyprland: hardware HEVC decode
//! straight into a fullscreen Wayland surface.
//!
//! The server picks the workspace. This end switches to it, lets
//! `waylandsink` open fullscreen there, and switches back when the display
//! goes away — no window rules, nothing written to `hyprland.conf`.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::proto::ScreenMode;

/// The hardware decoder, from `gst-plugin-va`. Without it the display is off:
/// decoding on the CPU would blow the latency this exists for.
const DECODER: &str = "vah265dec";

/// The monitor the display will show on, and where to go back to after.
pub struct Screen {
    pub mode: ScreenMode,
    pub output: String,
    workspace: u8,
    previous: Option<i64>,
}

impl Screen {
    /// Check this machine can show the display, and on which monitor.
    pub fn prepare(workspace: u8) -> Result<Self> {
        gst::init()?;
        if gst::ElementFactory::find(DECODER).is_none() {
            bail!("install gst-plugin-va");
        }
        let monitors: Vec<serde_json::Value> = hyprctl(&["-j", "monitors"])?;
        let monitor = monitors
            .iter()
            .find(|m| m["focused"].as_bool() == Some(true))
            .or(monitors.first())
            .context("Hyprland reports no monitor")?;
        let mode = ScreenMode {
            w: monitor["width"].as_u64().unwrap_or(0) as u32,
            h: monitor["height"].as_u64().unwrap_or(0) as u32,
            hz: monitor["refreshRate"].as_f64().unwrap_or(60.0).round() as u32,
        };
        if mode.w == 0 || mode.h == 0 {
            bail!("cannot read the monitor's mode");
        }
        let active: serde_json::Value = hyprctl(&["-j", "activeworkspace"])?;
        Ok(Self {
            mode,
            output: monitor["name"].as_str().unwrap_or_default().to_string(),
            workspace,
            previous: active["id"].as_i64(),
        })
    }

    /// Go to the display's workspace, so the sink opens there.
    pub fn enter(&self) {
        dispatch(&self.workspace.to_string());
    }

    /// Back to wherever this machine was before the display took over.
    pub fn restore(self) {
        if let Some(previous) = self.previous.filter(|&p| p != self.workspace as i64) {
            dispatch(&previous.to_string());
        }
    }
}

fn hyprctl<T: serde::de::DeserializeOwned>(args: &[&str]) -> Result<T> {
    let output = std::process::Command::new("hyprctl").args(args).output().context("cannot run hyprctl")?;
    serde_json::from_slice(&output.stdout).context("hyprctl said something unexpected")
}

fn dispatch(workspace: &str) {
    let result = std::process::Command::new("hyprctl").args(["dispatch", "workspace", workspace]).output();
    if let Err(e) = result {
        tracing::warn!("cannot switch workspace: {e}");
    }
}

/// Decodes and shows frames as fast as they come: a live source, no queues,
/// and a sink that draws each frame the moment it is decoded instead of
/// waiting for a clock.
pub struct Player {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
}

impl Player {
    pub fn start(output: &str) -> Result<Self> {
        gst::init()?;
        let fullscreen_on = if output.is_empty() { String::new() } else { format!("fullscreen-output={output}") };
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
               caps=video/x-h265,stream-format=byte-stream,alignment=au \
             ! h265parse ! {DECODER} \
             ! waylandsink sync=false fullscreen=true {fullscreen_on}"
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .context("no appsrc")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("not an appsrc"))?;
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self { pipeline, src })
    }

    pub fn push(&self, frame: Vec<u8>) {
        if let Err(e) = self.src.push_buffer(gst::Buffer::from_mut_slice(frame)) {
            tracing::debug!("decoder refused a frame: {e}");
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.src.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
