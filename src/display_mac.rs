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

use std::ffi::{c_void, CStr};
use std::ptr::{null_mut, NonNull};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGSize};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_HEVC, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};

use crate::display::{Frame, Slot};
use crate::proto::ScreenMode;
use crate::screens::Screen;
use crate::Side;

/// '420v': the encoder's own input format, so nothing converts in between.
const PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"420v");
/// A virtual display with no scaling reports 96 dpi, the same as the monitor it
/// stands in for is assumed to have.
const MM_PER_PIXEL: f64 = 25.4 / 96.0;
/// How long macOS may take to list a virtual display it just created.
const APPEAR_TIMEOUT: Duration = Duration::from_secs(5);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
    fn CGConfigureDisplayOrigin(config: *mut c_void, display: u32, x: i32, y: i32) -> i32;
    fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
}

/// kCGConfigureForSession: kept until logout, never written to the user's
/// saved arrangement.
const FOR_SESSION: u32 = 1;

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
    pub fn start(screen: ScreenMode, place: Place, bitrate: u32, slot: Arc<Slot>) -> Result<Self> {
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            // Adds Glide to the Screen Recording list and asks, once.
            unsafe { CGRequestScreenCaptureAccess() };
            bail!("needs Screen Recording permission");
        }
        let (ready, started) = mpsc::channel();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("glide-display".into()).spawn(move || {
            match Running::new(screen, place, bitrate, slot) {
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
    fn new(screen: ScreenMode, place: Place, bitrate: u32, slot: Arc<Slot>) -> Result<Self> {
        let queue = DispatchQueue::new("dev.glide.display", DispatchQueueAttr::SERIAL);
        let (display, id) = virtual_display(screen, &queue)?;
        // Misplaced is still usable, so a refusal here is logged, not fatal.
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
            config.setWidth(screen.w as usize);
            config.setHeight(screen.h as usize);
            config.setMinimumFrameInterval(CMTime::new(1, screen.hz as i32));
            config.setPixelFormat(PIXEL_FORMAT);
            config.setShowsCursor(true);
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

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).with_context(|| format!("{name:?} is missing from this macOS"))
}

/// A screen macOS believes is plugged in, at exactly the client's mode.
fn virtual_display(screen: ScreenMode, queue: &DispatchQueue) -> Result<(Retained<AnyObject>, u32)> {
    unsafe {
        let descriptor: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
        let _: () = msg_send![&descriptor, setDispatchQueue: queue];
        let _: () = msg_send![&descriptor, setName: &*NSString::from_str("Glide")];
        let _: () = msg_send![&descriptor, setMaxPixelsWide: screen.w];
        let _: () = msg_send![&descriptor, setMaxPixelsHigh: screen.h];
        let size = CGSize::new(screen.w as f64 * MM_PER_PIXEL, screen.h as f64 * MM_PER_PIXEL);
        let _: () = msg_send![&descriptor, setSizeInMillimeters: size];
        // Stable IDs, so macOS remembers where this screen was arranged.
        let _: () = msg_send![&descriptor, setVendorID: 0x676c_u32];
        let _: () = msg_send![&descriptor, setProductID: 0x6964_u32];
        let _: () = msg_send![&descriptor, setSerialNum: 1_u32];

        let allocated: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplay")?, alloc];
        let display: Option<Retained<AnyObject>> = msg_send![allocated, initWithDescriptor: &*descriptor];
        let display = display.context("macOS refused to create a virtual display")?;

        let allocated: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplayMode")?, alloc];
        let mode: Retained<AnyObject> = msg_send![
            allocated,
            initWithWidth: screen.w,
            height: screen.h,
            refreshRate: screen.hz as f64
        ];
        let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
        let _: () = msg_send![&settings, setHiDPI: 0_u32];
        let modes = NSArray::from_retained_slice(&[mode]);
        let _: () = msg_send![&settings, setModes: &*modes];
        let applied: bool = msg_send![&display, applySettings: &*settings];
        if !applied {
            bail!("macOS refused {}×{} at {} Hz for the virtual display", screen.w, screen.h, screen.hz);
        }
        let id: u32 = msg_send![&display, displayID];
        Ok((display, id))
    }
}

/// Put the virtual display where the Arrange window has the peer, so the
/// cursor leaves through the same edge in Extend as in Control.
fn arrange(id: u32, screen: ScreenMode, place: Place) -> Result<()> {
    let screens = crate::screens::local();
    let target = place
        .display
        .and_then(|n| screens.get(n.checked_sub(1)?).cloned())
        .or_else(|| union(&screens))
        .context("no real display to sit beside")?;
    let (x, y) = beside(&target, screen.w as i32, screen.h as i32, place.side);
    unsafe {
        let mut config = null_mut();
        if CGBeginDisplayConfiguration(&mut config) != 0 {
            bail!("CGBeginDisplayConfiguration failed");
        }
        CGConfigureDisplayOrigin(config, id, x, y);
        let status = CGCompleteDisplayConfiguration(config, FOR_SESSION);
        if status != 0 {
            bail!("macOS refused the arrangement (status {status})");
        }
    }
    tracing::info!(x, y, side = ?place.side, display = ?place.display, "virtual display placed");
    Ok(())
}

/// The box around every display: the whole desktop.
fn union(screens: &[Screen]) -> Option<Screen> {
    let left = screens.iter().map(|s| s.x).min()?;
    let top = screens.iter().map(|s| s.y).min()?;
    let right = screens.iter().map(|s| s.x + s.w as i32).max()?;
    let bottom = screens.iter().map(|s| s.y + s.h as i32).max()?;
    Some(Screen { name: String::new(), x: left, y: top, w: (right - left) as u32, h: (bottom - top) as u32 })
}

/// The origin of a `w`×`h` screen touching `target` along `side`, centred on
/// that edge. macOS coordinates: y grows downward from the main display's top.
fn beside(target: &Screen, w: i32, h: i32, side: Side) -> (i32, i32) {
    let (tw, th) = (target.w as i32, target.h as i32);
    match side {
        Side::Top => (target.x + (tw - w) / 2, target.y - h),
        Side::Bottom => (target.x + (tw - w) / 2, target.y + th),
        Side::Left => (target.x - w, target.y + (th - h) / 2),
        Side::Right => (target.x + tw, target.y + (th - h) / 2),
    }
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

/// HEVC on the media engine, tuned for latency over everything else: real
/// time, no B-frames, no frames held back, keyframes only when asked for.
fn encoder(screen: ScreenMode, bitrate: u32, slot: &Arc<Slot>) -> Result<CFRetained<VTCompressionSession>> {
    let yes: &CFType = CFBoolean::new(true);
    let spec = CFDictionary::<CFString, CFType>::from_slices(
        &[
            unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
            unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder },
        ],
        &[yes, yes],
    );
    let mut raw: *mut VTCompressionSession = null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None,
            screen.w as i32,
            screen.h as i32,
            kCMVideoCodecType_HEVC,
            Some(spec.as_opaque()),
            None,
            None,
            Some(encoded),
            Arc::as_ptr(slot) as *mut c_void,
            NonNull::from(&mut raw),
        )
    };
    let session = NonNull::new(raw)
        .filter(|_| status == 0)
        .map(|raw| unsafe { CFRetained::from_raw(raw) })
        .ok_or_else(|| anyhow!("cannot create the HEVC encoder (status {status})"))?;

    let no: &CFType = CFBoolean::new(false);
    let rate = CFNumber::new_i32(bitrate as i32);
    let fps = CFNumber::new_i32(screen.hz as i32);
    // Never on a timer: a reliable stream only needs one after a drop.
    let key_interval = CFNumber::new_i32(i32::MAX);
    let zero = CFNumber::new_i32(0);
    let profile: &CFType = unsafe { kVTProfileLevel_HEVC_Main_AutoLevel };
    let properties: [(&CFString, &CFType); 8] = unsafe {
        [
            (kVTCompressionPropertyKey_RealTime, yes),
            (kVTCompressionPropertyKey_AllowFrameReordering, no),
            (kVTCompressionPropertyKey_ProfileLevel, profile),
            (kVTCompressionPropertyKey_AverageBitRate, &rate),
            (kVTCompressionPropertyKey_ExpectedFrameRate, &fps),
            (kVTCompressionPropertyKey_MaxKeyFrameInterval, &key_interval),
            (kVTCompressionPropertyKey_MaxFrameDelayCount, &zero),
            (kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, yes),
        ]
    };
    for (key, value) in properties {
        // Some encoders refuse some keys; each one is a preference, not a need.
        let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
        if status != 0 {
            tracing::debug!(%key, status, "encoder declined a setting");
        }
    }
    Ok(session)
}

fn force_key() -> CFRetained<CFDictionary<CFString, CFType>> {
    let yes: &CFType = CFBoolean::new(true);
    CFDictionary::from_slices(&[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }], &[yes])
}

/// VideoToolbox's output callback, on the encoder's own thread.
unsafe extern "C-unwind" fn encoded(
    slot: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let (Some(slot), Some(sample)) = (unsafe { (slot as *const Slot).as_ref() }, unsafe { sample.as_ref() }) else {
        return;
    };
    if status != 0 {
        return;
    }
    if let Some(frame) = unsafe { annex_b(sample) } {
        slot.push(frame);
    }
}

/// VideoToolbox writes length-prefixed NAL units with the parameter sets kept
/// aside; a decoder reading a plain byte stream wants start codes, and the
/// parameter sets in front of every keyframe.
unsafe fn annex_b(sample: &CMSampleBuffer) -> Option<Frame> {
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut body = vec![0u8; len];
    if unsafe { block.copy_data_bytes(0, len, NonNull::new(body.as_mut_ptr().cast())?) } != 0 {
        return None;
    }
    let key = unsafe { is_keyframe(sample) };
    let mut params: Vec<&[u8]> = Vec::new();
    let mut prefix = 4;
    if key {
        let format = unsafe { sample.format_description() }?;
        let mut count = 0usize;
        let mut index = 0usize;
        loop {
            let (mut ptr, mut size, mut header) = (std::ptr::null(), 0usize, 0i32);
            let got = unsafe {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    &format, index, &mut ptr, &mut size, &mut count, &mut header,
                )
            };
            if got != 0 || ptr.is_null() {
                break;
            }
            prefix = header as usize;
            // SAFETY: owned by the format description, which lives until this
            // frame has been rewritten below.
            params.push(unsafe { std::slice::from_raw_parts(ptr, size) });
            index += 1;
            if index >= count {
                break;
            }
        }
    }
    Some(Frame { key, data: to_annex_b(&body, prefix, &params)? })
}

/// Keyframes are the samples *not* marked "not sync".
unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    // SAFETY: CFArray of CFDictionary is toll-free bridged to NSArray of NSDictionary.
    let attachments: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(CFRetained::as_ptr(&attachments).as_ptr() as *const _) };
    let not_sync: &NSString = unsafe { &*(kCMSampleAttachmentKey_NotSync as *const CFString as *const NSString) };
    attachments
        .firstObject()
        .and_then(|dict| dict.objectForKey(not_sync))
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .is_none_or(|value| !value.boolValue())
}

const START: [u8; 4] = [0, 0, 0, 1];

/// Rewrite `prefix`-byte length-prefixed NAL units as start-code delimited
/// ones, parameter sets first. `None` if a length runs past the end.
fn to_annex_b(body: &[u8], prefix: usize, params: &[&[u8]]) -> Option<Vec<u8>> {
    if !(1..=4).contains(&prefix) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() + params.iter().map(|p| p.len() + 4).sum::<usize>());
    for param in params {
        out.extend_from_slice(&START);
        out.extend_from_slice(param);
    }
    let mut at = 0;
    while at < body.len() {
        let len = body.get(at..at + prefix)?.iter().fold(0usize, |n, &b| n << 8 | b as usize);
        at += prefix;
        out.extend_from_slice(&START);
        out.extend_from_slice(body.get(at..at + len)?);
        at += len;
    }
    Some(out)
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

    #[test]
    fn length_prefixes_become_start_codes() {
        let body = [0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC];
        let out = to_annex_b(&body, 4, &[&[0x40, 1], &[0x42]]).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]
        );
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        assert_eq!(to_annex_b(&[0, 0, 0, 9, 1], 4, &[]), None);
        assert_eq!(to_annex_b(&[0, 0], 4, &[]), None);
        assert_eq!(to_annex_b(&[1], 0, &[]), None);
    }

    #[test]
    fn the_virtual_display_sits_centred_on_the_chosen_edge() {
        // Display 1 from the Arrange window: 2560×1080 at the origin.
        let d1 = Screen { name: "Display 1".into(), x: 0, y: 0, w: 2560, h: 1080 };
        assert_eq!(beside(&d1, 1920, 1080, Side::Top), (320, -1080));
        assert_eq!(beside(&d1, 1920, 1080, Side::Bottom), (320, 1080));
        assert_eq!(beside(&d1, 1920, 1080, Side::Left), (-1920, 0));
        assert_eq!(beside(&d1, 1920, 1080, Side::Right), (2560, 0));
    }

    #[test]
    fn no_display_named_means_the_whole_desktop() {
        let screens = [
            Screen { name: "a".into(), x: -1920, y: 0, w: 1920, h: 1080 },
            Screen { name: "b".into(), x: 0, y: -200, w: 2560, h: 1440 },
        ];
        let all = union(&screens).unwrap();
        assert_eq!((all.x, all.y, all.w, all.h), (-1920, -200, 4480, 1440));
        assert!(union(&[]).is_none());
    }

    #[test]
    fn short_length_prefixes_work_too() {
        assert_eq!(to_annex_b(&[0, 1, 7], 2, &[]).unwrap(), [0, 0, 0, 1, 7]);
    }

    /// The private API is the part most likely to break with a macOS update:
    /// creating a display must add one to the system, and dropping it must
    /// take it away again. Needs no permission, but does change the screen
    /// layout for a moment, so it only runs when asked.
    #[test]
    #[ignore]
    fn a_virtual_display_comes_and_goes() {
        let before = crate::screens::local().len();
        let queue = DispatchQueue::new("dev.glide.test", DispatchQueueAttr::SERIAL);
        let (display, id) = virtual_display(ScreenMode { w: 1920, h: 1080, hz: 60 }, &queue).unwrap();
        assert_ne!(id, 0);
        std::thread::sleep(Duration::from_secs(1));
        let during = crate::screens::local();
        assert_eq!(during.len(), before + 1, "{during:?}");
        assert!(during.iter().any(|s| s.w == 1920 && s.h == 1080), "{during:?}");
        drop(display);
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(crate::screens::local().len(), before, "the display must go away when dropped");
    }

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
        let capture = Capture::start(screen, place, 20_000_000, slot.clone()).expect("start");
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
