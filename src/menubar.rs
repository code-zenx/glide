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
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardTypeString, NSStatusBar, NSVariableStatusItemLength, NSWindow,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSBundle, NSObject, NSSize, NSString, NSTimer};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::pair::{notify, Me};
use crate::status::Status;

mod devices;
mod permissions;

use devices::{device_row, update_row, Device};
use permissions::{open_settings, request_permissions};

/// How often the bar re-reads the daemon's state.
const REFRESH_SECONDS: f64 = 1.0;
/// The menu bar is 22 points tall; 18 leaves the standard breathing room.
const ICON_POINTS: f64 = 18.0;

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
