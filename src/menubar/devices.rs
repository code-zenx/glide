//! The device rows: one per paired device, with a dot and a word for what
//! it is doing, and a submenu whose ticks follow its state.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{sel, AnyThread};
use objc2_app_kit::{
    NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFontWeightRegular, NSImage, NSImageSymbolConfiguration,
    NSMenu, NSMenuItem, NSMenuItemBadge,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSString};

use crate::status::{PeerState, State};

use super::{command, MenuTarget};

/// The status dot in front of each device.
const DOT_POINTS: f64 = 9.0;

/// One device's row and the submenu items whose state follows it.
pub(super) struct Device {
    pub(super) name: String,
    pub(super) row: Retained<NSMenuItem>,
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

pub(super) fn device_row(mtm: MainThreadMarker, target: &MenuTarget, index: usize, name: &str) -> Device {
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

pub(super) fn update_row(device: &Device, state: &State, peer: &PeerState) {
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
