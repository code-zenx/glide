//! The virtual screen itself: a display macOS believes is plugged in, the
//! modes it offers, where it sits beside the real displays, and its HiDPI
//! mode.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dispatch2::DispatchQueue;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use super::{class, Place};
use crate::proto::ScreenMode;
use crate::screens::Screen;
use crate::Side;

/// A virtual display with no scaling reports 96 dpi, the same as the monitor it
/// stands in for is assumed to have.
const MM_PER_PIXEL: f64 = 25.4 / 96.0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
    fn CGConfigureDisplayOrigin(config: *mut c_void, display: u32, x: i32, y: i32) -> i32;
    fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
}

/// kCGConfigureForSession: kept until logout, never written to the user's
/// saved arrangement.
const FOR_SESSION: u32 = 1;

/// How much larger than the client's monitor each offered mode is drawn. In
/// HiDPI, 2× reads as "looks like" the monitor's own size at Retina
/// sharpness, and the smaller steps give bigger text; 1× is plain pixels.
const HIDPI_SCALES: [f64; 4] = [2.0, 5.0 / 3.0, 4.0 / 3.0, 1.0];

/// The pixel sizes the virtual display offers, largest (the default) first.
/// Kept even: encoders and scalers dislike odd sizes.
fn modes(screen: ScreenMode, hidpi: bool) -> Vec<(u32, u32)> {
    let scales: &[f64] = if hidpi { &HIDPI_SCALES } else { &[1.0] };
    scales
        .iter()
        .map(|s| {
            let even = |n: u32| ((n as f64 * s).round() as u32) & !1;
            (even(screen.w), even(screen.h))
        })
        .collect()
}

/// A screen macOS believes is plugged in, sized for the client's monitor.
pub(super) fn virtual_display(screen: ScreenMode, hidpi: bool, queue: &DispatchQueue) -> Result<(Retained<AnyObject>, u32)> {
    let offered = modes(screen, hidpi);
    let (max_w, max_h) = offered[0];
    unsafe {
        let descriptor: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
        let _: () = msg_send![&descriptor, setDispatchQueue: queue];
        let _: () = msg_send![&descriptor, setName: &*NSString::from_str("Glide")];
        let _: () = msg_send![&descriptor, setMaxPixelsWide: max_w];
        let _: () = msg_send![&descriptor, setMaxPixelsHigh: max_h];
        let size = CGSize::new(screen.w as f64 * MM_PER_PIXEL, screen.h as f64 * MM_PER_PIXEL);
        let _: () = msg_send![&descriptor, setSizeInMillimeters: size];
        // Stable IDs, so macOS remembers where this screen was arranged.
        let _: () = msg_send![&descriptor, setVendorID: 0x676c_u32];
        let _: () = msg_send![&descriptor, setProductID: 0x6964_u32];
        let _: () = msg_send![&descriptor, setSerialNum: 1_u32];

        let allocated: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplay")?, alloc];
        let display: Option<Retained<AnyObject>> = msg_send![allocated, initWithDescriptor: &*descriptor];
        let display = display.context("macOS refused to create a virtual display")?;

        let mode_class = class(c"CGVirtualDisplayMode")?;
        let mut all = Vec::with_capacity(offered.len());
        for (w, h) in offered {
            let allocated: Allocated<AnyObject> = msg_send![mode_class, alloc];
            let mode: Retained<AnyObject> =
                msg_send![allocated, initWithWidth: w, height: h, refreshRate: screen.hz as f64];
            all.push(mode);
        }
        let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
        let _: () = msg_send![&settings, setHiDPI: hidpi as u32];
        let modes = NSArray::from_retained_slice(&all);
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
pub(super) fn arrange(id: u32, screen: ScreenMode, place: Place) -> Result<()> {
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

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCopyAllDisplayModes(display: u32, options: *const c_void) -> *const c_void;
    fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGConfigureDisplayWithDisplayMode(config: *mut c_void, display: u32, mode: *const c_void, options: *const c_void) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFRelease(cf: *mut c_void);
}

/// Switch to the HiDPI mode that looks like the client's monitor: drawn at
/// twice its pixels, shown at its size. macOS starts a new virtual display at
/// 1x, and may take a moment to list the rest.
///
/// ponytail: sets this default every time Extend starts, so a scaled mode
/// picked in RDM or System Settings lasts until the display is next rebuilt.
pub(super) unsafe fn use_hidpi(id: u32, screen: ScreenMode) -> Result<()> {
    // The value of kCGDisplayShowDuplicateLowResolutionModes, which is not
    // its own name. Without it the HiDPI modes are left out of the list.
    let key = NSString::from_str("kCGDisplayResolution");
    let options = NSDictionary::from_slices(&[&*key], &[&*NSNumber::numberWithBool(true)]);
    let (w, h) = (screen.w as usize, screen.h as usize);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let list = unsafe { CGDisplayCopyAllDisplayModes(id, Retained::as_ptr(&options).cast()) };
        if !list.is_null() {
            let found = (0..unsafe { CFArrayGetCount(list) })
                .map(|i| unsafe { CFArrayGetValueAtIndex(list, i) })
                .find(|&m| unsafe {
                    CGDisplayModeGetWidth(m) == w && CGDisplayModeGetHeight(m) == h && CGDisplayModeGetPixelWidth(m) == 2 * w
                });
            let applied = found.map(|mode| unsafe {
                let mut config = null_mut();
                CGBeginDisplayConfiguration(&mut config);
                CGConfigureDisplayWithDisplayMode(config, id, mode, std::ptr::null());
                CGCompleteDisplayConfiguration(config, FOR_SESSION)
            });
            unsafe { CFRelease(list as *mut c_void) };
            match applied {
                Some(0) => {
                    tracing::info!(w, h, "virtual display in HiDPI");
                    return Ok(());
                }
                Some(status) => bail!("macOS refused the mode (status {status})"),
                None => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("macOS never offered a {w}×{h} HiDPI mode")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch2::DispatchQueueAttr;

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
    fn hidpi_offers_retina_steps_with_the_monitor_size_first() {
        let fhd = ScreenMode { w: 1920, h: 1080, hz: 60 };
        assert_eq!(modes(fhd, true), [(3840, 2160), (3200, 1800), (2560, 1440), (1920, 1080)]);
        assert_eq!(modes(fhd, false), [(1920, 1080)]);
        let odd = ScreenMode { w: 1366, h: 768, hz: 60 };
        assert!(modes(odd, true).iter().all(|(w, h)| w % 2 == 0 && h % 2 == 0), "{:?}", modes(odd, true));
    }

    /// The private API is the part most likely to break with a macOS update:
    /// creating a display must add one to the system, and dropping it must
    /// take it away again. Needs no permission, but does change the screen
    /// layout for a moment, so it only runs when asked.
    #[test]
    #[ignore]
    fn a_virtual_display_comes_and_goes() {
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGDisplayIsActive(display: u32) -> bool;
            fn CGDisplayBounds(display: u32) -> objc2_core_foundation::CGRect;
        }
        let before = crate::screens::local();
        let queue = DispatchQueue::new("dev.glide.test", DispatchQueueAttr::SERIAL);
        let (display, id) = virtual_display(ScreenMode { w: 1920, h: 1080, hz: 60 }, true, &queue).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        assert!(unsafe { CGDisplayIsActive(id) }, "the display never came up");
        let points = unsafe { CGDisplayBounds(id) }.size;
        assert_eq!((points.width, points.height), (1920.0, 1080.0));
        unsafe { use_hidpi(id, ScreenMode { w: 1920, h: 1080, hz: 60 }) }.expect("switch to HiDPI");
        std::thread::sleep(Duration::from_millis(500));
        let points = unsafe { CGDisplayBounds(id) }.size;
        assert_eq!((points.width, points.height), (1920.0, 1080.0), "HiDPI still looks like the monitor's size");
        assert_eq!(crate::screens::local(), before, "glide's own display must not count as a real one");
        drop(display);
        let dropped = std::time::Instant::now();
        let listed = || {
            #[link(name = "CoreGraphics", kind = "framework")]
            unsafe extern "C" {
                fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
            }
            let (mut ids, mut n) = ([0u32; 16], 0u32);
            unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) };
            ids[..n as usize].contains(&id)
        };
        eprintln!("right after drop: active={} listed={}", unsafe { CGDisplayIsActive(id) }, listed());
        while listed() && dropped.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("gone after {:?}", dropped.elapsed());
        assert!(!listed(), "the display must go away when dropped");
    }
}
