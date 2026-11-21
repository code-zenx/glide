//! The macOS face of glide: an icon in the menu bar, and a menu that lists
//! every device with what it is doing. Each device's submenu switches it on
//! or off, makes it the extended display, views its screen, or forgets it;
//! *Add Device…* copies an invite for a new one.
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
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFontWeightRegular, NSImage, NSImageSymbolConfiguration, NSMenu,
    NSMenuItem, NSMenuItemBadge, NSPasteboard, NSPasteboardTypeString, NSStatusBar, NSVariableStatusItemLength,
    NSWindow, NSWorkspace,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSArray, NSBundle, NSDictionary, NSNumber, NSObject, NSSize, NSString, NSTimer, NSURL,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::pair::{notify, Me};
use crate::status::{PeerState, State, Status};

/// How often the bar re-reads the daemon's state.
const REFRESH_SECONDS: f64 = 1.0;
/// The menu bar is 22 points tall; 18 leaves the standard breathing room.
const ICON_POINTS: f64 = 18.0;
/// The status dot in front of each device.
const DOT_POINTS: f64 = 9.0;

/// One device's row and the submenu items whose state follows it.
struct Device {
    name: String,
    row: Retained<NSMenuItem>,
    on: Retained<NSMenuItem>,
    off: Retained<NSMenuItem>,
    control: Retained<NSMenuItem>,
    extend: Retained<NSMenuItem>,
    view: Retained<NSMenuItem>,
    /// What the row shows now, so an unchanged second redraws nothing.
    shown: RefCell<Option<Shown>>,
}

/// Everything a row and its submenu show for one device.
#[derive(PartialEq)]
struct Shown {
    dot: Dot,
    text: String,
    switched_on: bool,
    control: bool,
    extending: bool,
    can_extend: bool,
    reachable: bool,
    viewing: bool,
    can_view: bool,
}

pub struct Ivars {
    status: Arc<Status>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
    me: Me,
    menu: Retained<NSMenu>,
    /// The "Devices" heading; device rows go right after it.
    heading: Retained<NSMenuItem>,
    /// In menu order. A row's submenu items carry its place here as their tag.
    devices: RefCell<Vec<Device>>,
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
            // Read fresh, so a device paired a minute ago shows up.
            let peers = match crate::arrange_peers(&ivars.config_path) {
                Ok(peers) => peers,
                Err(e) => return tracing::warn!("cannot read the config to arrange: {e}"),
            };
            let peers = peers
                .into_iter()
                .map(|(name, side, display)| {
                    let screens = ivars.status.screens_of(&name);
                    crate::arrange::PeerView::new(name, screens, side, display)
                })
                .collect();
            let window = crate::arrange::window(
                mtm,
                crate::screens::local(),
                peers,
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

        /// Copy a fresh `glide join …` command for the new machine.
        #[unsafe(method(addDevice:))]
        fn add_device(&self, _sender: &NSMenuItem) {
            match crate::pair::invite(&self.ivars().me) {
                Ok(command) => {
                    let board = NSPasteboard::generalPasteboard();
                    board.clearContents();
                    board.setString_forType(&NSString::from_str(&command), unsafe { NSPasteboardTypeString });
                    notify("Invite copied", "Paste it into a terminal on the new machine. Valid for 10 minutes.");
                }
                Err(e) => notify("Glide could not make an invite", &e.to_string()),
            }
        }

        #[unsafe(method(deviceOn:))]
        fn device_on(&self, sender: &NSMenuItem) {
            if let Some(name) = self.device(sender) {
                let _ = self.ivars().control.send(Control::SetEnabled(name, true));
            }
        }

        #[unsafe(method(deviceOff:))]
        fn device_off(&self, sender: &NSMenuItem) {
            if let Some(name) = self.device(sender) {
                let _ = self.ivars().control.send(Control::SetEnabled(name, false));
            }
        }

        /// Let the cursor cross into this device, or stop it.
        #[unsafe(method(deviceControl:))]
        fn device_control(&self, sender: &NSMenuItem) {
            let Some(name) = self.device(sender) else { return };
            let on = self.ivars().status.snapshot().peers.iter().any(|p| p.name == name && p.control);
            let _ = self.ivars().control.send(Control::Drive(name, !on));
        }

        /// Make this device the extended display, or stop if it already is.
        #[unsafe(method(deviceExtend:))]
        fn device_extend(&self, sender: &NSMenuItem) {
            let Some(name) = self.device(sender) else { return };
            let already = self.ivars().status.snapshot().extending == name;
            let _ = self.ivars().control.send(Control::Extend((!already).then_some(name)));
        }

        /// Show this device's screen here, or stop if it already is. One
        /// screen cannot both show the extended display and be shown here,
        /// so viewing the extended device stops Extend first.
        #[unsafe(method(deviceView:))]
        fn device_view(&self, sender: &NSMenuItem) {
            let Some(name) = self.device(sender) else { return };
            let state = self.ivars().status.snapshot();
            let control = &self.ivars().control;
            if state.extending == name && state.viewing != name {
                let _ = control.send(Control::Extend(None));
            }
            let _ = control.send(Control::View((state.viewing != name).then_some(name)));
        }

        #[unsafe(method(deviceForget:))]
        fn device_forget(&self, sender: &NSMenuItem) {
            let (Some(name), Some(mtm)) = (self.device(sender), MainThreadMarker::new()) else { return };
            // An accessory app's alert would open behind whatever is in front.
            NSApplication::sharedApplication(mtm).activate();
            let alert = NSAlert::new(mtm);
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.setMessageText(&NSString::from_str(&format!("Forget {name}?")));
            alert.setInformativeText(ns_string!("It will need a new invite to connect again."));
            alert.addButtonWithTitle(ns_string!("Forget"));
            alert.addButtonWithTitle(ns_string!("Cancel"));
            if alert.runModal() == NSAlertFirstButtonReturn {
                let _ = self.ivars().control.send(Control::Forget(name));
            }
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

    /// The device a submenu item belongs to.
    fn device(&self, sender: &NSMenuItem) -> Option<String> {
        self.ivars().devices.borrow().get(sender.tag() as usize).map(|d| d.name.clone())
    }

    /// Rebuild the device rows when a device was paired or forgotten.
    fn sync_devices(&self, mtm: MainThreadMarker, names: &[&str]) {
        let ivars = self.ivars();
        if ivars.devices.borrow().iter().map(|d| d.name.as_str()).eq(names.iter().copied()) {
            return;
        }
        for old in ivars.devices.borrow_mut().drain(..) {
            ivars.menu.removeItem(&old.row);
        }
        let first = ivars.menu.indexOfItem(&ivars.heading) + 1;
        let rows: Vec<Device> = names.iter().enumerate().map(|(i, name)| device_row(mtm, self, i, name)).collect();
        for (i, device) in rows.iter().enumerate() {
            ivars.menu.insertItem_atIndex(&device.row, first + i as isize);
        }
        *ivars.devices.borrow_mut() = rows;
    }
}

/// The colour of a device's dot, and whether it is filled.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Dot {
    Controlling,
    Linked,
    Paused,
    Offline,
    Off,
    Extended,
}

/// What a device's row says: its dot and the words at the right edge.
fn describe(state: &State, peer: &PeerState) -> (Dot, String) {
    let extended = state.extending == peer.name && !state.display.is_empty() && !state.display.starts_with('⚠');
    match state.mode_of(peer).as_str() {
        // Viewed: input reaches it only while the user is in its window.
        "local" | "sending" if state.viewing == peer.name => (Dot::Controlling, "viewing".into()),
        "sending" => (Dot::Controlling, "controlling".into()),
        "off" => (Dot::Off, "off".into()),
        "local" if peer.paused => (Dot::Paused, format!("paused on {}", peer.name)),
        "local" if extended => (Dot::Extended, "extended display".into()),
        "local" => (Dot::Linked, peer.rtt_ms.map_or("linked".into(), |r| format!("linked · {r:.1} ms"))),
        _ => (Dot::Offline, "offline".into()),
    }
}

fn dot_image(dot: Dot) -> Option<Retained<NSImage>> {
    let (symbol, color) = match dot {
        Dot::Controlling => ("circle.fill", NSColor::systemBlueColor()),
        Dot::Linked => ("circle.fill", NSColor::systemGreenColor()),
        Dot::Paused => ("circle.fill", NSColor::systemOrangeColor()),
        Dot::Offline => ("circle.fill", NSColor::systemGrayColor()),
        Dot::Off => ("circle", NSColor::systemGrayColor()),
        Dot::Extended => ("circle.fill", NSColor::systemPurpleColor()),
    };
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), None)?;
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(DOT_POINTS, unsafe { NSFontWeightRegular });
    let palette = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[color]));
    image.imageWithSymbolConfiguration(&size.configurationByApplyingConfiguration(&palette))
}

fn device_row(mtm: MainThreadMarker, target: &MenuTarget, index: usize, name: &str) -> Device {
    let row = NSMenuItem::new(mtm);
    row.setTitle(&NSString::from_str(name));
    let submenu = NSMenu::new(mtm);
    // Ticks and greying out are set by hand below, not inferred by AppKit.
    submenu.setAutoenablesItems(false);
    let item = |title: &str, action| {
        let item = command(mtm, title, action, "", target);
        item.setTag(index as isize);
        item
    };
    let on = item("On", sel!(deviceOn:));
    let off = item("Off", sel!(deviceOff:));
    let control = item("Control", sel!(deviceControl:));
    let extend = item("Use as Extended Display", sel!(deviceExtend:));
    let view = item("View Screen", sel!(deviceView:));
    let forget = item(&format!("Forget {name}…"), sel!(deviceForget:));
    submenu.addItem(&on);
    submenu.addItem(&off);
    submenu.addItem(&NSMenuItem::separatorItem(mtm));
    submenu.addItem(&control);
    submenu.addItem(&extend);
    submenu.addItem(&view);
    submenu.addItem(&NSMenuItem::separatorItem(mtm));
    submenu.addItem(&forget);
    row.setSubmenu(Some(&submenu));
    Device {
        name: name.to_string(),
        row,
        on,
        off,
        control,
        extend,
        view,
        shown: RefCell::new(None),
    }
}

fn update_row(device: &Device, state: &State, peer: &PeerState) {
    let (dot, text) = describe(state, peer);
    let now = Shown {
        dot,
        text,
        switched_on: peer.mode != "off",
        control: peer.control,
        extending: state.extending == peer.name,
        can_extend: peer.can_extend,
        reachable: matches!(peer.mode.as_str(), "local"),
        viewing: state.viewing == peer.name,
        can_view: peer.can_view,
    };
    if device.shown.borrow().as_ref() == Some(&now) {
        return;
    }
    device.row.setImage(dot_image(now.dot).as_deref());
    device.row.setBadge(Some(&NSMenuItemBadge::initWithString(
        NSMenuItemBadge::alloc(),
        &NSString::from_str(&now.text),
    )));
    let tick = |on: bool| {
        if on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        }
    };
    device.on.setState(tick(now.switched_on));
    device.off.setState(tick(!now.switched_on));
    device.control.setState(tick(now.control));
    device.extend.setState(tick(now.extending));
    device
        .extend
        .setTitle(&NSString::from_str(if now.can_extend {
            "Use as Extended Display"
        } else {
            "Use as Extended Display — needs Hyprland"
        }));
    // Turning either off is always allowed; starting one needs a linked device
    // that can do it.
    device
        .extend
        .setEnabled(now.extending || (now.can_extend && now.reachable));
    device.view.setState(tick(now.viewing));
    device.view.setTitle(&NSString::from_str(if now.can_view {
        "View Screen"
    } else {
        "View Screen — not supported there"
    }));
    device
        .view
        .setEnabled(now.viewing || (now.can_view && now.reachable));
    *device.shown.borrow_mut() = Some(now);
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

pub fn run(status: Arc<Status>, control: UnboundedSender<Control>, config_path: PathBuf, me: Me) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the menu bar must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: a menu bar item, no Dock icon, no windows.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);
    let heading = note(mtm, "Devices");
    let target = MenuTarget::new(Ivars {
        status: status.clone(),
        control,
        config_path,
        me,
        menu: menu.clone(),
        heading: heading.clone(),
        devices: RefCell::new(Vec::new()),
        window: RefCell::new(None),
    });

    // One line for the whole picture, and the extended display's own line
    // while it has something to say.
    let summary = note(mtm, "");
    let shown = note(mtm, "");
    menu.addItem(&summary);
    menu.addItem(&shown);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&heading);
    // Device rows are inserted here, after the heading.
    menu.addItem(&command(mtm, "Add Device…", sel!(addDevice:), "", &target));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
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
    let rows = target.clone();
    let redraw = move || {
        let state = reader.snapshot();
        let names: Vec<&str> = state.peers.iter().map(|p| p.name.as_str()).collect();
        rows.sync_devices(mtm, &names);
        for (device, peer) in rows.ivars().devices.borrow().iter().zip(&state.peers) {
            update_row(device, &state, peer);
        }
        summary.setTitle(&NSString::from_str(&state.menu_summary()));
        shown.setTitle(&NSString::from_str(&state.display));
        shown.setHidden(state.display.is_empty());
        // The permission item only makes sense while something is refused.
        grant.setHidden(!state.denied);
        button.setToolTip(Some(&NSString::from_str(&format!("Glide: {}", state.menu_summary()))));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(peer: PeerState) -> State {
        State { peers: vec![peer], ..Default::default() }
    }

    #[test]
    fn each_device_state_reads_as_its_own_row() {
        let mut p = PeerState::new("anorak");
        assert_eq!(describe(&state(p.clone()), &p), (Dot::Offline, "offline".into()));
        p.mode = "off".into();
        assert_eq!(describe(&state(p.clone()), &p), (Dot::Off, "off".into()));
        p.mode = "local".into();
        p.rtt_ms = Some(1.74);
        assert_eq!(describe(&state(p.clone()), &p), (Dot::Linked, "linked · 1.7 ms".into()));
        p.paused = true;
        assert_eq!(describe(&state(p.clone()), &p).0, Dot::Paused);
        p.paused = false;
        let mut s = state(p.clone());
        s.extending = "anorak".into();
        s.display = "Extended: 2560×1440 at 60 Hz".into();
        assert_eq!(describe(&s, &p), (Dot::Extended, "extended display".into()));
        s.viewing = "anorak".into();
        assert_eq!(
            describe(&s, &p),
            (Dot::Controlling, "viewing".into()),
            "a view says so with input at home"
        );
        s.viewing.clear();
        s.flow = "sending".into();
        s.active = "anorak".into();
        assert_eq!(
            describe(&s, &p).0,
            Dot::Controlling,
            "having the cursor wins"
        );
        s.viewing = "anorak".into();
        assert_eq!(
            describe(&s, &p),
            (Dot::Controlling, "viewing".into()),
            "a view says so"
        );
    }
}
