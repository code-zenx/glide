//! The macOS face of glide: an icon in the menu bar, and a menu that says what
//! the link is doing and opens the arrange window.
//!
//! The daemon runs on its own thread in this same process, so the app has one
//! identity for macOS to grant input permissions to, and one icon in the bar.

use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength, NSWindow, NSWorkspace,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSBundle, NSDictionary, NSNumber, NSObject, NSSize, NSString,
    NSTimer, NSURL,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::status::Status;
use crate::Side;

/// How often the bar re-reads the daemon's state.
const REFRESH_SECONDS: f64 = 1.0;
/// The menu bar is 22 points tall; 18 leaves the standard breathing room.
const ICON_POINTS: f64 = 18.0;

pub struct Ivars {
    status: Arc<Status>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
    peer_name: String,
    side: std::cell::Cell<Side>,
    display: std::cell::Cell<Option<usize>>,
    /// Kept alive so the window survives being closed and reopened.
    window: RefCell<Option<Retained<NSWindow>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "GlideMenuTarget"]
    #[ivars = Ivars]
    struct MenuTarget;

    impl MenuTarget {
        /// Open the window that draws the real screens.
        #[unsafe(method(arrangeWindow:))]
        fn arrange_window(&self, _sender: &NSMenuItem) {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let ivars = self.ivars();
            let window = crate::arrange::window(
                mtm,
                crate::screens::local(),
                ivars.status.screens_of(&ivars.peer_name),
                ivars.peer_name.clone(),
                ivars.side.get(),
                ivars.display.get(),
                ivars.control.clone(),
                ivars.config_path.clone(),
            );
            window.makeKeyAndOrderFront(None);
            NSApplication::sharedApplication(mtm).activate();
            *ivars.window.borrow_mut() = Some(window);
        }

        /// Ask macOS for the two input permissions, and show where to grant them.
        #[unsafe(method(permissions:))]
        fn permissions(&self, _sender: &NSMenuItem) {
            request_permissions();
            open_settings("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent");
            let _ = self.ivars().control.send(Control::RetryBackends);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &NSMenuItem) {
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }
    }
);

impl MenuTarget {
    fn new(ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

/// Trigger the system's own permission prompts. macOS only shows each once per
/// process, and never at all for a capture that fails silently, so the menu
/// offers this as a deliberate action.
fn request_permissions() {
    use std::ffi::c_void;

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

fn open_settings(url: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

/// The bar shows the app's own mark, tinted by macOS to match the bar.
fn menu_bar_icon(mtm: MainThreadMarker) -> Option<Retained<NSImage>> {
    let path = NSBundle::mainBundle().pathForResource_ofType(
        Some(ns_string!("MenubarIcon")),
        Some(ns_string!("png")),
    )?;
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &path)?;
    image.setTemplate(true);
    image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));
    let _ = mtm;
    Some(image)
}

/// A line of text that is information, not a command.
fn note(mtm: MainThreadMarker, text: &str) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(text));
    item.setEnabled(false);
    item
}

fn command(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
    target: &MenuTarget,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    item
}

pub fn run(
    status: Arc<Status>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
    current: Side,
    display: Option<usize>,
    peer_name: String,
) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the menu bar must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: a menu bar item, no Dock icon, no windows.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let target = MenuTarget::new(Ivars {
        status: status.clone(),
        control,
        config_path,
        peer_name: peer_name.clone(),
        side: std::cell::Cell::new(current),
        display: std::cell::Cell::new(display),
        window: RefCell::new(None),
    });

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);

    // Three lines of state: who, how the input is flowing, how fast.
    let who = note(mtm, &peer_name);
    let flow = note(mtm, "");
    let speed = note(mtm, "");
    for line in [&who, &flow, &speed] {
        menu.addItem(line);
    }

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Arranging screens is the window's job; four compass items said the same
    // thing less clearly, so they are gone.
    menu.addItem(&command(mtm, "Arrange Screens…", sel!(arrangeWindow:), "", &target));
    let grant = command(mtm, "Grant Input Permissions…", sel!(permissions:), "", &target);
    menu.addItem(&grant);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&command(mtm, "Quit Glide", sel!(quit:), "q", &target));
    item.setMenu(Some(&menu));

    let button = item.button(mtm).context("the status item has no button")?;
    match menu_bar_icon(mtm) {
        Some(icon) => button.setImage(Some(&icon)),
        // No icon in the bundle: fall back to a letter rather than nothing.
        None => button.setTitle(ns_string!("G")),
    }

    let reader = status.clone();
    let redraw = move || {
        let state = reader.snapshot();
        who.setTitle(&NSString::from_str(&state.headline()));
        flow.setTitle(&NSString::from_str(&state.flow()));
        speed.setTitle(&NSString::from_str(&state.speed()));
        // The permission item only makes sense while something is refused.
        grant.setHidden(!state.denied);
        button.setToolTip(Some(&NSString::from_str(&state.tooltip())));
    };
    redraw();
    // Lets the window be smoke-tested without a human clicking the menu.
    if std::env::var_os("GLIDE_OPEN_ARRANGE").is_some() {
        let _: () = unsafe { msg_send![&*target, arrangeWindow: &*NSMenuItem::new(mtm)] };
    }
    // A machine that cannot capture is useless as a sender; ask straight away
    // rather than waiting for someone to find the menu.
    if status.snapshot().denied {
        request_permissions();
    }

    let tick = RcBlock::new(move |_: NonNull<NSTimer>| redraw());
    unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(REFRESH_SECONDS, true, &tick) };

    app.run();
    Ok(())
}
