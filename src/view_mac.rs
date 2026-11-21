//! The Mac end of View Screen: the device's streams, shown in a window that
//! goes fullscreen in a Space of its own and played through the speakers. The
//! window's view is backed by an `AVSampleBufferDisplayLayer`, which decodes
//! H.264 on the media engine and draws it with no copies through this
//! process. What the window does with the keyboard and mouse is
//! `view_input_mac.rs`.
//!
//! AVFoundation's classes are reached by name through the Objective-C
//! runtime, the way `display_mac.rs` reaches `CGVirtualDisplay`, so no new
//! crate is needed for them.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut, NonNull};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSScreen, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, kCMSampleAttachmentKey_DisplayImmediately, CMBlockBuffer,
    CMFormatDescription, CMSampleBuffer, CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_foundation::{MainThreadMarker, NSString};
use quinn::RecvStream;
use tokio::sync::mpsc::UnboundedSender;

use crate::view_input_mac::{Device, Input};

// Also what keeps AVFoundation linked, so its classes exist to be named.
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVLayerVideoGravityResizeAspect: &'static NSString;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionarySetValue(dict: *const c_void, key: *const c_void, value: *const c_void);
    static kCFBooleanTrue: *const c_void;
}

/// AVQueuedSampleBufferRenderingStatusFailed.
const FAILED: isize = 2;
/// The app's main thread answers at once. Without the app (`glide run`)
/// nothing runs it, and waiting longer would not help.
const MAIN_TIMEOUT: Duration = Duration::from_secs(2);

thread_local! {
    /// The open window and its input. Only the main thread may touch them, so
    /// they live there.
    static WINDOW: RefCell<Option<(Retained<NSWindow>, Input)>> = const { RefCell::new(None) };
}

/// The display layer. AVSampleBufferDisplayLayer takes frames on any thread;
/// objc2 cannot know that, hence the promise.
struct Layer(Retained<AnyObject>);
// SAFETY: only `enqueueSampleBuffer:`, `status` and `flush` are called off the
// main thread, which AVFoundation allows from one thread at a time.
unsafe impl Send for Layer {}

/// The fullscreen picture of the viewed device. Dropping it closes the window.
pub struct Screen {
    layer: Layer,
    format: Option<CFRetained<CMFormatDescription>>,
    /// The SPS and PPS `format` was made from, to rebuild it only on change.
    params: Vec<u8>,
}

// SAFETY: a format description is immutable once made.
unsafe impl Send for Screen {}

impl Screen {
    /// Open the window on the main thread and wait for it.
    pub fn open(device: Device) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        DispatchQueue::main().exec_async(move || {
            let made = MainThreadMarker::new()
                .context("not the main thread")
                .and_then(|mtm| open_window(mtm, device));
            let _ = tx.send(made.map_err(|e| e.to_string()));
        });
        let layer = rx
            .recv_timeout(MAIN_TIMEOUT)
            .map_err(|_| anyhow!("nothing to draw a window with: run Glide.app"))?
            .map_err(|why| anyhow!(why))?;
        Ok(Self {
            layer,
            format: None,
            params: Vec::new(),
        })
    }

    /// Decode and draw one frame of Annex-B H.264. Nothing shows before the
    /// first keyframe: until then there are no parameter sets to decode with.
    pub fn show(&mut self, frame: &[u8]) -> Result<()> {
        let unit = crate::h264::parse(frame);
        if let (Some(sps), Some(pps)) = (unit.sps, unit.pps) {
            self.describe(sps, pps)?;
        }
        let Some(format) = &self.format else {
            return Ok(());
        };
        if unit.avcc.is_empty() {
            return Ok(());
        }
        let sample = unsafe { sample(&unit.avcc, format) }?;
        unsafe { enqueue(&self.layer.0, &sample) };
        Ok(())
    }

    fn describe(&mut self, sps: &[u8], pps: &[u8]) -> Result<()> {
        let params = [sps, pps].concat();
        if params != self.params {
            self.format = Some(unsafe { format(sps, pps) }?);
            self.params = params;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        DispatchQueue::main().exec_async(close_window);
    }
}

/// Show one device until it stops or fails. A failure says why, and ends the
/// view through the session like any other way of stopping.
pub async fn follow(
    net: crate::link::Net,
    links: std::sync::Arc<crate::link::Links>,
    peer: crate::Peer,
    control: UnboundedSender<crate::input::Control>,
) {
    let device = Device { peer: peer.name.clone(), links, control: control.clone() };
    if let Err(e) = receive(&net, &peer, device).await {
        tracing::warn!(peer = %peer.name, "view ended: {e:#}");
        crate::pair::notify(
            &format!("Glide stopped viewing {}", peer.name),
            &format!("{e:#}"),
        );
        let _ = control.send(crate::input::Control::View(None));
    }
}

async fn receive(net: &crate::link::Net, peer: &crate::Peer, device: Device) -> Result<()> {
    let conn = net.dial_view(peer).await?;
    let (mut video, sound) = crate::view::accept_streams(&conn).await?;
    let mut screen = Screen::open(device)?;
    let playing = tokio::spawn(play(sound));
    let shown = crate::display::recv_frames(&mut video, |frame| {
        if let Err(e) = screen.show(&frame.data) {
            tracing::debug!("frame not shown: {e:#}");
        }
    })
    .await;
    playing.abort();
    shown
}

/// The device's sound, for as long as it sends any. Sound that cannot play
/// is no reason to end the view.
async fn play(mut recv: RecvStream) {
    let mut speaker = match crate::audio_mac::Speaker::open() {
        Ok(speaker) => speaker,
        Err(e) => return tracing::warn!("viewing without sound: {e:#}"),
    };
    while let Ok(chunk) = crate::link::read_frame(&mut recv).await {
        speaker.play(&chunk);
    }
}

fn open_window(mtm: MainThreadMarker, device: Device) -> Result<Layer> {
    close_window();
    let frame = screen_under_cursor(mtm)
        .context("no screen to show it on")?
        .visibleFrame();
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&format!("{} — Glide", device.peer)));
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    // Fullscreen in a Space of its own, so swiping, Mission Control and
    // Cmd+Tab can leave it and come back.
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    let view = window.contentView().context("the window has no view")?;
    let layer: Retained<AnyObject> = unsafe {
        msg_send![
            crate::display_mac::class(c"AVSampleBufferDisplayLayer")?,
            new
        ]
    };
    unsafe {
        let _: () = msg_send![&*layer, setVideoGravity: AVLayerVideoGravityResizeAspect];
        // Layer first, then wantsLayer: that makes the view host this layer
        // rather than build one of its own.
        let _: () = msg_send![&*view, setLayer: &*layer];
    }
    view.setWantsLayer(true);
    let input = Input::attach(&window, device)?;
    // A menu-bar app has no Dock icon and no place in Cmd+Tab, and a view
    // needs both to be come back to.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    window.makeKeyAndOrderFront(None);
    WINDOW.with(|w| *w.borrow_mut() = Some((window, input)));
    // On the next turn of the main loop: a window asked to go fullscreen
    // before it is on screen may stay as it is.
    DispatchQueue::main().exec_async(go_fullscreen);
    Ok(Layer(layer))
}

/// Into fullscreen, unless already there: a second toggle would bring it out.
fn go_fullscreen() {
    WINDOW.with(|w| {
        let open = w.borrow();
        let Some((window, _)) = open.as_ref() else { return };
        if !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
            window.toggleFullScreen(None);
        }
    });
}

fn close_window() {
    let Some((window, input)) = WINDOW.with(|w| w.borrow_mut().take()) else { return };
    // Input first: its close-button watch must not take this close for the
    // user's.
    drop(input);
    window.close();
    let app = NSApplication::sharedApplication(window.mtm());
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// Where the cursor is, which is where the menu was just clicked.
fn screen_under_cursor(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let at = NSEvent::mouseLocation();
    let inside = |screen: &NSScreen| {
        let f = screen.frame();
        (f.origin.x..f.origin.x + f.size.width).contains(&at.x)
            && (f.origin.y..f.origin.y + f.size.height).contains(&at.y)
    };
    NSScreen::screens(mtm)
        .iter()
        .find(|s| inside(s))
        .or_else(|| NSScreen::mainScreen(mtm))
}

/// A format description from the stream's own parameter sets.
unsafe fn format(sps: &[u8], pps: &[u8]) -> Result<CFRetained<CMFormatDescription>> {
    let pointers = [
        NonNull::from(sps).cast::<u8>(),
        NonNull::from(pps).cast::<u8>(),
    ];
    let sizes = [sps.len(), pps.len()];
    let mut out: *const CMFormatDescription = null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&pointers).cast(),
            NonNull::from(&sizes).cast(),
            4,
            NonNull::from(&mut out),
        )
    };
    match NonNull::new(out.cast_mut()) {
        Some(made) if status == 0 => Ok(unsafe { CFRetained::from_raw(made) }),
        _ => bail!("the stream's parameter sets were refused (status {status})"),
    }
}

/// One frame as CoreMedia wants it: the AVCC bytes copied into a block buffer
/// of its own, marked to be shown as soon as it is decoded.
unsafe fn sample(avcc: &[u8], format: &CMFormatDescription) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block: *mut CMBlockBuffer = null_mut();
    let len = avcc.len();
    let flags = kCMBlockBufferAssureMemoryNowFlag;
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            null_mut(),
            len,
            None,
            null(),
            0,
            len,
            flags,
            NonNull::from(&mut block),
        )
    };
    let block = NonNull::new(block)
        .filter(|_| status == 0)
        .context("cannot make a block buffer")?;
    let block = unsafe { CFRetained::from_raw(block) };
    if unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(avcc).cast(), &block, 0, len) } != 0
    {
        bail!("cannot fill the block buffer");
    }
    let mut sample: *mut CMSampleBuffer = null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            null(),
            1,
            &len,
            NonNull::from(&mut sample),
        )
    };
    let sample = NonNull::new(sample)
        .filter(|_| status == 0)
        .context("cannot make a sample buffer")?;
    let sample = unsafe { CFRetained::from_raw(sample) };
    if let Some(attachments) = unsafe { sample.sample_attachments_array(true) } {
        // SAFETY: one mutable dictionary per sample, and this sample is the only one.
        unsafe {
            let first = CFArrayGetValueAtIndex(CFRetained::as_ptr(&attachments).as_ptr().cast(), 0);
            let key = kCMSampleAttachmentKey_DisplayImmediately as *const CFString as *const c_void;
            CFDictionarySetValue(first, key, kCFBooleanTrue);
        }
    }
    Ok(sample)
}

/// A layer that failed to decode stays failed until flushed; the next
/// keyframe then picks the picture up again.
unsafe fn enqueue(layer: &AnyObject, sample: &CMSampleBuffer) {
    unsafe {
        let status: isize = msg_send![layer, status];
        if status == FAILED {
            tracing::debug!("the video layer failed, flushing it");
            let _: () = msg_send![layer, flush];
        }
        let _: () = msg_send![layer, enqueueSampleBuffer: sample];
    }
}
