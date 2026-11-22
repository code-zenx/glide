//! The sending end of an extended display on macOS: a virtual screen macOS
//! treats as real, captured by ScreenCaptureKit and encoded to HEVC on the
//! media engine. Frames never pass through this process's CPU until they are
//! a few kilobytes of bitstream.
//!
//! `CGVirtualDisplay` is private API — the same one DeskPad and BetterDisplay
//! use — so it is reached through the Objective-C runtime by name. If a macOS
//! update removes it, starting the display fails with a message and nothing
//! else in glide is affected.
//!
//! Everything lives on a thread of its own, away from the event tap: the
//! capture and encoder callbacks run on their own queues and only ever touch
//! the frame `Slot`.

use std::ffi::CStr;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::kCVImageBufferYCbCrMatrix_ITU_R_709_2;
use objc2_core_media::{kCMTimeInvalid, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use objc2_video_toolbox::VTCompressionSession;

mod hevc;
mod virtual_display;

use crate::display::Slot;
use crate::proto::ScreenMode;
use crate::Side;
use hevc::{encoder, force_key};
use virtual_display::{arrange, use_hidpi, virtual_display};

/// '420v': the encoder's own input format, so nothing converts in between.
const PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"420v");
/// How long macOS may take to list a virtual display it just created.
const APPEAR_TIMEOUT: Duration = Duration::from_secs(5);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Where the peer's screen sits: which edge, of which display (1-based, as the
/// Arrange window numbers them), or of the whole desktop when none is named.
#[derive(Clone, Copy, Debug)]
pub struct Place {
    pub side: Side,
    pub display: Option<usize>,
}

/// A running extended display. Dropping it stops the capture and removes the
/// virtual screen, and macOS moves its windows back to the main display.
pub struct Capture {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(screen: ScreenMode, place: Place, hidpi: bool, bitrate: u32, slot: Arc<Slot>) -> Result<Self> {
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            // Adds Glide to the Screen Recording list and asks, once.
            unsafe { CGRequestScreenCaptureAccess() };
            bail!("needs Screen Recording permission");
        }
        let (ready, started) = mpsc::channel();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("glide-display".into()).spawn(move || {
            match Running::new(screen, place, hidpi, bitrate, slot) {
                Ok(running) => {
                    let _ = ready.send(Ok(()));
                    let _ = stopped.recv();
                    running.stop();
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            }
        })?;
        // Blocks the calling task for as long as macOS takes to list the new
        // screen: well under a second, once, when the mode is switched.
        started.recv().map_err(|_| anyhow!("the display thread died"))??;
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stop.take());
        // Wait, so a display restarted at a new mode never overlaps the old one.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Everything that has to stay alive while the display is up. Not `Send`:
/// it is built, kept and torn down on the display thread.
struct Running {
    stream: Retained<SCStream>,
    session: CFRetained<VTCompressionSession>,
    _sink: Retained<FrameSink>,
    _display: Retained<AnyObject>,
    _queue: DispatchRetained<DispatchQueue>,
    /// The encoder callback's context. Must outlive the session.
    _slot: Arc<Slot>,
}

impl Running {
    fn new(screen: ScreenMode, place: Place, hidpi: bool, bitrate: u32, slot: Arc<Slot>) -> Result<Self> {
        let queue = DispatchQueue::new("dev.glide.display", DispatchQueueAttr::SERIAL);
        let (display, id) = virtual_display(screen, hidpi, &queue)?;
        // Blurrier or misplaced is still usable, so these are logged, not fatal.
        if hidpi {
            if let Err(e) = unsafe { use_hidpi(id, screen) } {
                tracing::warn!("cannot switch the virtual display to HiDPI: {e}");
            }
        }
        if let Err(e) = arrange(id, screen, place) {
            tracing::warn!("cannot place the virtual display: {e}");
        }
        let shared = shareable(id)?;
        let session = encoder(screen, bitrate, &slot)?;

        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &shared, &NSArray::new())
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            // Always the client's pixels. In a HiDPI mode macOS draws more
            // than that, and ScreenCaptureKit scales it down on the GPU.
            config.setWidth(screen.w as usize);
            config.setHeight(screen.h as usize);
            config.setMinimumFrameInterval(CMTime::new(1, screen.hz as i32));
            config.setPixelFormat(PIXEL_FORMAT);
            config.setShowsCursor(true);
            // Capture and stream both say BT.709, so the Mac's colours are
            // what anorak draws instead of whatever its decoder guesses.
            config.setColorMatrix(kCVImageBufferYCbCrMatrix_ITU_R_709_2);
        }
        let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None) };
        let sink = FrameSink::new(Sink { session: session.clone(), slot: slot.clone(), force_key: force_key() });
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*sink),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("cannot add the capture output: {}", e.localizedDescription()))?;
        }
        wait(|done| unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) })
            .context("cannot start capturing the virtual display")?;
        Ok(Self { stream, session, _sink: sink, _display: display, _queue: queue, _slot: slot })
    }

    fn stop(self) {
        let _ = wait(|done| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) });
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

/// Run an API that reports back through an error-or-nothing block, and wait.
fn wait(call: impl FnOnce(RcBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    call(done);
    match rx.recv_timeout(APPEAR_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(anyhow!(why)),
        Err(_) => Err(anyhow!("ScreenCaptureKit never answered")),
    }
}

/// An Objective-C class by name, or why not. View Screen finds AVFoundation's
/// layer this way too.
pub(crate) fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).with_context(|| format!("{name:?} is missing from this macOS"))
}

/// `SCDisplay` is only handed out through a completion block.
struct Found(Option<Retained<SCDisplay>>, Option<String>);
// SAFETY: moved once, from ScreenCaptureKit's callback to the waiting thread.
unsafe impl Send for Found {}

/// ScreenCaptureKit's handle on the virtual display, once macOS lists it.
fn shareable(id: u32) -> Result<Retained<SCDisplay>> {
    let deadline = std::time::Instant::now() + APPEAR_TIMEOUT;
    loop {
        let (tx, rx) = mpsc::channel();
        let answer = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
            let display = unsafe { content.as_ref() }
                .and_then(|c| unsafe { c.displays() }.iter().find(|d| unsafe { d.displayID() } == id));
            let why = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
            let _ = tx.send(Found(display, why));
        });
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&answer) };
        match rx.recv_timeout(APPEAR_TIMEOUT) {
            Ok(Found(Some(display), _)) => return Ok(display),
            Ok(Found(None, Some(why))) => bail!("cannot list screens: {why}"),
            _ if std::time::Instant::now() > deadline => bail!("the virtual display never appeared"),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

pub struct Sink {
    session: CFRetained<VTCompressionSession>,
    slot: Arc<Slot>,
    force_key: CFRetained<CFDictionary<CFString, CFType>>,
}

define_class!(
    /// Where ScreenCaptureKit delivers frames: straight into the encoder.
    #[unsafe(super(NSObject))]
    #[name = "GlideFrameSink"]
    #[ivars = Sink]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                unsafe { self.ivars().encode(sample) };
            }
        }
    }
);

impl FrameSink {
    fn new(sink: Sink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(sink);
        unsafe { msg_send![super(this), init] }
    }
}

impl Sink {
    /// Idle frames (nothing changed on screen) carry no picture, and sending
    /// nothing for them is what makes a still screen cost nothing.
    unsafe fn encode(&self, sample: &CMSampleBuffer) {
        if unsafe { frame_status(sample) } != Some(SCFrameStatus::Complete) {
            return;
        }
        let Some(image) = (unsafe { sample.image_buffer() }) else { return };
        let options = self.slot.take_key_request().then(|| self.force_key.as_opaque());
        let status = unsafe {
            self.session.encode_frame(
                &image,
                sample.presentation_time_stamp(),
                kCMTimeInvalid,
                options,
                null_mut(),
                null_mut(),
            )
        };
        if status != 0 {
            tracing::debug!(status, "encoder rejected a frame");
        }
    }
}

unsafe fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: toll-free bridged, as in `is_keyframe`.
    let attachments: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(CFRetained::as_ptr(&attachments).as_ptr() as *const _) };
    let status = attachments.firstObject()?.objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    Some(SCFrameStatus(status.downcast::<NSNumber>().ok()?.integerValue()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::hevc::START;

    /// The real thing, end to end on this Mac: a virtual display appears,
    /// ScreenCaptureKit captures it, and the first frame out of the encoder
    /// is a keyframe carrying its parameter sets. Needs Screen Recording for
    /// whatever runs the test, so it only runs when asked.
    #[test]
    #[ignore]
    fn a_virtual_display_streams_hevc() {
        let slot = Arc::new(Slot::default());
        let screen = ScreenMode { w: 1920, h: 1080, hz: 60 };
        let place = Place { side: Side::Top, display: Some(1) };
        let capture = Capture::start(screen, place, true, 20_000_000, slot.clone()).expect("start");
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let frame = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), slot.pop()).await })
            .expect("no frame within 5 s");
        drop(capture);
        assert!(frame.key, "the first frame must be a keyframe");
        // VPS, SPS and PPS are NAL types 32, 33 and 34, in bits 1..7 of the
        // byte after the start code.
        let kinds: Vec<u8> = frame.data.windows(5).filter(|w| w[..4] == START).map(|w| (w[4] >> 1) & 0x3f).collect();
        assert_eq!(&kinds[..3], &[32, 33, 34], "{kinds:?}");
    }
}
