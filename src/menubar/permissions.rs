//! Asking macOS for the input permissions glide needs, and opening the
//! System Settings pane where they are granted.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *mut c_void);
}
/// kIOHIDRequestTypeListenEvent
const LISTEN: u32 = 1;

extern "C" fn passthrough(
    _proxy: *mut c_void,
    _kind: u32,
    event: *mut c_void,
    _user: *mut c_void,
) -> *mut c_void {
    event
}

/// Trigger the system's own permission prompts. macOS only shows each once per
/// process, and never at all for a capture that fails silently, so the menu
/// offers this as a deliberate action.
pub(super) fn request_permissions() {
    let prompt = NSDictionary::from_slices(
        &[&*NSString::from_str("AXTrustedCheckOptionPrompt")],
        &[&*NSNumber::numberWithBool(true)],
    );
    let trusted = unsafe { AXIsProcessTrustedWithOptions(Retained::as_ptr(&prompt).cast()) };
    let listening = unsafe { IOHIDRequestAccess(LISTEN) };

    // Actually try to tap events. The capture backend checks the permission and
    // gives up before creating a tap, so without this macOS is never asked and
    // the app never appears in the Input Monitoring list at all.
    let tap = unsafe {
        CGEventTapCreate(
            0,      // kCGHIDEventTap
            0,      // kCGHeadInsertEventTap
            1,      // kCGEventTapOptionListenOnly
            1 << 5, // kCGEventMouseMoved
            passthrough,
            std::ptr::null_mut(),
        )
    };
    let tapped = !tap.is_null();
    if tapped {
        unsafe { CFRelease(tap) };
    }
    tracing::info!(trusted, listening, tapped, "asked macOS for input permission");
}

pub(super) fn open_settings(url: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}
