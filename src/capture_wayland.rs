//! Hyprland's screen, frame by frame, for View Screen: wlr-screencopy copies
//! an output into shared memory, and each frame goes to GStreamer's `appsrc`.
//!
//! A frame waits for something on screen to change, so a still screen costs
//! next to nothing. But the first frame, and one a second while nothing
//! changes, is copied at once: a receiver that just arrived, or fell behind
//! and asked for a keyframe, needs a frame to get it from. The cursor is drawn
//! into the frames by the compositor.
//!
//! ponytail: a CPU copy of every frame (about 500 MB/s at 1080p60). Measured
//! 2026-11-22 on anorak (i5-10500T), screen changing every frame at 1080p60:
//! glide 32% of one core (half this copy, half the upload to the GPU) and
//! Hyprland 16% for its readback, about 4% of the machine; a still screen
//! costs next to nothing. DMA-BUF straight into the encoder removes all three
//! if that ever matters; the `capture_cost` test measures it again.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer_app as gst_app;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

/// How long a wait for the compositor goes before looking at the stop flag.
const POLL_MS: i32 = 100;
/// The longest a still screen goes without a frame.
const IDLE: Duration = Duration::from_secs(1);

/// The capture thread. Dropping it stops the thread and waits for it.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Connect now, so a compositor without screencopy fails here, then copy
    /// `output` into `src` on a thread of its own.
    pub fn start(output: &str, src: gst_app::AppSrc, hz: u32) -> Result<Self> {
        let session = Session::connect(output)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("glide-capture".into())
            .spawn(move || session.run(&src, hz, &stopping))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the compositor asked for: a buffer of this format and size.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Spec {
    format: wl_shm::Format,
    w: u32,
    h: u32,
    stride: u32,
}

/// One frame's events, as they arrive.
#[derive(Default)]
struct Pending {
    spec: Option<Spec>,
    inverted: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    names: Vec<(WlOutput, String)>,
    frame: Pending,
}

struct Session {
    queue: EventQueue<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1,
    shm: WlShm,
    output: WlOutput,
}

impl Session {
    fn connect(name: &str) -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        // Version 2 for copy_with_damage; 3 adds events this does not need.
        let manager: ZwlrScreencopyManagerV1 =
            globals.bind(&qh, 1..=2, ()).context("no wlr-screencopy")?;
        let shm: WlShm = globals.bind(&qh, 1..=1, ())?;
        let outputs: Vec<WlOutput> = globals
            .contents()
            .clone_list()
            .into_iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| {
                globals
                    .registry()
                    .bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, ())
            })
            .collect();
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        // Outputs too old to say their name leave the first one standing in.
        let output = state
            .names
            .iter()
            .find(|(_, n)| n == name)
            .map(|(o, _)| o.clone())
            .or(outputs.first().cloned());
        let output = output.context("the compositor lists no output")?;
        Ok(Self {
            queue,
            state,
            manager,
            shm,
            output,
        })
    }

    fn run(mut self, src: &gst_app::AppSrc, hz: u32, stop: &AtomicBool) {
        let mut pool: Option<Pool> = None;
        let mut last: Option<Instant> = None;
        while !stop.load(Ordering::Relaxed) {
            let now = last.is_none_or(|at| at.elapsed() >= IDLE);
            match self.frame(&mut pool, now, stop) {
                Ok(Some(buffer)) => {
                    set_caps(src, pool.as_ref().map(|p| p.spec), hz);
                    // An error is the pipeline stopping under us.
                    if src.push_buffer(buffer).is_err() {
                        break;
                    }
                    last = Some(Instant::now());
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("screen capture stopped: {e:#}");
                    break;
                }
            }
        }
        let _ = src.end_of_stream();
    }

    /// One frame: ask for it, hand over a buffer of the size asked for, wait.
    /// `now` copies at once instead of waiting for a change. `None` when the
    /// compositor gave up, nothing changed for `IDLE`, or the capture is
    /// stopping.
    fn frame(
        &mut self,
        pool: &mut Option<Pool>,
        now: bool,
        stop: &AtomicBool,
    ) -> Result<Option<gst::Buffer>> {
        let qh = self.queue.handle();
        let until = Instant::now() + IDLE;
        self.state.frame = Pending::default();
        let frame = self.manager.capture_output(1, &self.output, &qh, ());
        self.wait(stop, until, |p| p.spec.is_some() || p.failed)?;
        let Some(spec) = self.state.frame.spec.filter(|_| !self.state.frame.failed) else {
            frame.destroy();
            return Ok(None);
        };
        let kept = pool.take().filter(|p| p.spec == spec);
        let pool = pool.insert(match kept {
            Some(kept) => kept,
            None => Pool::new(spec, &self.shm, &qh)?,
        });
        if now || self.manager.version() < 2 {
            frame.copy(&pool.buffer);
        } else {
            frame.copy_with_damage(&pool.buffer);
        }
        self.wait(stop, until, |p| p.ready || p.failed)?;
        frame.destroy();
        Ok(self
            .state
            .frame
            .ready
            .then(|| pool.pack(self.state.frame.inverted)))
    }

    /// Dispatch until `done`, the stop flag or `until`, never blocking past
    /// `POLL_MS` at a time.
    fn wait(
        &mut self,
        stop: &AtomicBool,
        until: Instant,
        done: impl Fn(&Pending) -> bool,
    ) -> Result<()> {
        while !done(&self.state.frame) && !stop.load(Ordering::Relaxed) && Instant::now() < until {
            self.queue.flush()?;
            if let Some(guard) = self.queue.prepare_read() {
                let mut fds = [libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                if unsafe { libc::poll(fds.as_mut_ptr(), 1, POLL_MS) } > 0 {
                    guard.read()?;
                }
            }
            self.queue.dispatch_pending(&mut self.state)?;
        }
        Ok(())
    }
}

/// Caps once known, and again if the output's size or format changes.
fn set_caps(src: &gst_app::AppSrc, spec: Option<Spec>, hz: u32) {
    let Some(spec) = spec else { return };
    let Some(format) = gst_format(spec.format) else {
        return;
    };
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", spec.w as i32)
        .field("height", spec.h as i32)
        .field("framerate", gst::Fraction::new(hz as i32, 1))
        .build();
    if src.caps().as_ref() != Some(&caps) {
        src.set_caps(Some(&caps));
    }
}

/// wl_shm's little-endian names, in GStreamer's byte-order words.
fn gst_format(format: wl_shm::Format) -> Option<&'static str> {
    match format {
        wl_shm::Format::Xrgb8888 => Some("BGRx"),
        wl_shm::Format::Argb8888 => Some("BGRA"),
        wl_shm::Format::Xbgr8888 => Some("RGBx"),
        wl_shm::Format::Abgr8888 => Some("RGBA"),
        _ => None,
    }
}

/// A shared-memory buffer the compositor copies into.
struct Pool {
    spec: Spec,
    map: *mut u8,
    len: usize,
    pool: WlShmPool,
    buffer: WlBuffer,
    _fd: OwnedFd,
}

// SAFETY: the mapping is only ever touched by the capture thread that owns it.
unsafe impl Send for Pool {}

impl Pool {
    fn new(spec: Spec, shm: &WlShm, qh: &QueueHandle<State>) -> Result<Self> {
        if gst_format(spec.format).is_none() {
            bail!(
                "the compositor offers {:?}, which this cannot send",
                spec.format
            );
        }
        let len = spec.stride as usize * spec.h as usize;
        let raw = unsafe { libc::memfd_create(c"glide-capture".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            bail!("ftruncate: {}", std::io::Error::last_os_error());
        }
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let map = unsafe { libc::mmap(null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            spec.w as i32,
            spec.h as i32,
            spec.stride as i32,
            spec.format,
            qh,
            (),
        );
        Ok(Self {
            spec,
            map: map.cast(),
            len,
            pool,
            buffer,
            _fd: fd,
        })
    }

    /// The picture with rows packed tight (the compositor may pad them) and
    /// the right way up.
    fn pack(&self, inverted: bool) -> gst::Buffer {
        let (row, h, stride) = (
            self.spec.w as usize * 4,
            self.spec.h as usize,
            self.spec.stride as usize,
        );
        // SAFETY: `len` bytes mapped, and the compositor is done writing them.
        let shm = unsafe { std::slice::from_raw_parts(self.map, self.len) };
        let mut out = Vec::with_capacity(row * h);
        for y in 0..h {
            let from = if inverted { h - 1 - y } else { y } * stride;
            out.extend_from_slice(&shm[from..from + row]);
        }
        gst::Buffer::from_mut_slice(out)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let frame = &mut state.frame;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                frame.spec = Some(Spec {
                    format,
                    w: width,
                    h: height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::Buffer { .. }
            | zwlr_screencopy_frame_v1::Event::Failed => frame.failed = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                frame.inverted = matches!(flags, WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => frame.ready = true,
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.names.push((output.clone(), name));
        }
    }
}

/// `registry_queue_init` keeps the global list itself; nothing to add.
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlBuffer);

#[cfg(test)]
mod tests {
    use super::*;
    use gst::prelude::*;

    /// What View Screen's capture costs at full speed: copy the screen into
    /// the GPU encoder for five seconds, then print frames per second and this
    /// process's CPU. Needs a Hyprland session:
    /// `cargo test --release capture_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn capture_cost() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch("appsrc name=src is-live=true format=time do-timestamp=true ! vapostproc ! vah264enc ! fakesink sync=false").unwrap();
        let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
        let src = pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut session = Session::connect("").unwrap();
        let (stop, mut pool, mut frames) = (AtomicBool::new(false), None, 0);
        let (cpu, started) = (cpu_seconds(), Instant::now());
        while started.elapsed() < Duration::from_secs(5) {
            if let Some(buffer) = session.frame(&mut pool, true, &stop).unwrap() {
                set_caps(&src, pool.as_ref().map(|p| p.spec), 60);
                src.push_buffer(buffer).unwrap();
                frames += 1;
            }
        }
        let wall = started.elapsed().as_secs_f64();
        println!("{:.0} fps, {:.0}% of one core", frames as f64 / wall, (cpu_seconds() - cpu) / wall * 100.0);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    fn cpu_seconds() -> f64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        secs(usage.ru_utime) + secs(usage.ru_stime)
    }
}
