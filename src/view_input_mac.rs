//! View Screen's input, the way Moonlight does it: the Mac's keyboard and
//! mouse reach the viewed device only while the user is in its window. A click
//! on the picture engages; leaving the window any way macOS offers (Cmd+Tab, a
//! Space swipe, another app, leaving fullscreen) disengages, and the picture
//! keeps playing. Nothing is captured globally, so those ways out always work.
//!
//! Events are read from AppKit, not the input-capture tap, and sent as the
//! same `Msg::Enter`, `Msg::Input` and `Msg::Leave` an edge crossing sends, so
//! the device types them through its usual path.

use std::cell::RefCell;
use std::ptr::{null_mut, NonNull};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context, Result};
use block2::{DynBlock, RcBlock};
use input_event::{Event, KeyboardEvent, PointerEvent, BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSCursor, NSEvent, NSEventMask, NSEventType, NSWindow, NSWindowDidResignKeyNotification,
    NSWindowWillCloseNotification, NSWindowWillExitFullScreenNotification, NSWorkspace,
    NSWorkspaceActiveSpaceDidChangeNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName, NSObjectProtocol};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::link::Links;
use crate::proto::Msg;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
}

/// Every event the device can use. The window's other events stay macOS's.
const WATCHED: [NSEventType; 14] = {
    use NSEventType as Kind;
    [
        Kind::LeftMouseDown, Kind::LeftMouseUp, Kind::RightMouseDown, Kind::RightMouseUp,
        Kind::OtherMouseDown, Kind::OtherMouseUp, Kind::MouseMoved, Kind::LeftMouseDragged,
        Kind::RightMouseDragged, Kind::OtherMouseDragged, Kind::ScrollWheel,
        Kind::KeyDown, Kind::KeyUp, Kind::FlagsChanged,
    ]
};

/// A wheel's line, in the steps of 120 the device counts in: the
/// input-capture tap's own figure (3 lines a step).
const V120_PER_LINE: f64 = 40.0;

/// NX_DEVICE*KEYMASK: the bit each modifier key sets in an event's
/// modifierFlags while that very key, not just one of its kind, is down.
const MODIFIER_BITS: [(u16, usize); 8] = [
    (0x3B, 0x0001), // left control
    (0x38, 0x0002), // left shift
    (0x3C, 0x0004), // right shift
    (0x37, 0x0008), // left command
    (0x36, 0x0010), // right command
    (0x3A, 0x0020), // left option
    (0x3D, 0x0040), // right option
    (0x3E, 0x2000), // right control
];
/// kVK_CapsLock.
const CAPS_LOCK: u16 = 0x39;

/// Who the window's input is for, and whom to tell when it is closed.
pub struct Device {
    pub peer: String,
    pub links: Arc<Links>,
    pub control: UnboundedSender<Control>,
}

/// Whether the device has the keyboard and mouse.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Engaged {
    #[default]
    No,
    /// Yes, but the click that engaged is still down.
    Clicking,
    Yes,
}

/// What one event in the window means, decided apart from AppKit so the rules
/// can be tested.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// Not the device's: macOS handles it as usual.
    Pass,
    /// The click that engages. It is the user reaching for the device, not
    /// input for it, so it is not forwarded.
    Engage,
    /// That click's release, which is no more the device's than its press.
    Swallow,
    /// The device's: forwarded, and hidden from macOS (no beep, no shortcut).
    Forward,
}

/// The first click on a window that is not key is macOS's, to bring it
/// forward; engaging before the window has the keyboard would hide the cursor
/// with nothing to give it back.
fn verdict(engaged: Engaged, kind: NSEventType, clicked_picture: bool, key_window: bool) -> Verdict {
    match engaged {
        Engaged::No if clicked_picture && key_window => Verdict::Engage,
        Engaged::No => Verdict::Pass,
        Engaged::Clicking if kind == NSEventType::LeftMouseUp => Verdict::Swallow,
        Engaged::Clicking | Engaged::Yes => Verdict::Forward,
    }
}

/// The window, the device, and whether input goes there right now.
struct Engagement {
    window: Retained<NSWindow>,
    peer: String,
    links: Arc<Links>,
    engaged: Engaged,
}

impl Engagement {
    /// True when macOS must not see the event.
    fn take(&mut self, event: &NSEvent) -> bool {
        let window = event.window(self.window.mtm());
        if window.is_none_or(|w| Retained::as_ptr(&w) != Retained::as_ptr(&self.window)) {
            return false;
        }
        let kind = event.r#type();
        let clicked = kind == NSEventType::LeftMouseDown && self.on_picture(event);
        match verdict(self.engaged, kind, clicked, self.window.isKeyWindow()) {
            Verdict::Pass => false,
            Verdict::Engage => {
                self.engage();
                true
            }
            Verdict::Swallow => {
                self.engaged = Engaged::Yes;
                true
            }
            Verdict::Forward => {
                for input in translate(event) {
                    self.links.send(&self.peer, Msg::Input(input));
                }
                true
            }
        }
    }

    /// Below the title bar, when there is one.
    fn on_picture(&self, event: &NSEvent) -> bool {
        let Some(view) = self.window.contentView() else { return false };
        let at = view.convertPoint_fromView(event.locationInWindow(), None);
        view.mouse_inRect(at, view.bounds())
    }

    fn engage(&mut self) {
        self.engaged = Engaged::Clicking;
        self.links.send(&self.peer, Msg::Enter);
        NSCursor::hide();
        // Frozen as well as hidden, so the mouse cannot carry the cursor onto
        // another screen, a hot corner or the menu bar meanwhile.
        unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
    }

    fn disengage(&mut self) {
        if std::mem::take(&mut self.engaged) == Engaged::No {
            return;
        }
        // The device lifts every key it saw go down when it hears this, so a
        // shortcut half typed on the way out (Cmd+Tab's Cmd) is not left held.
        self.links.send(&self.peer, Msg::Leave);
        unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        NSCursor::unhide();
    }
}

type Observer = (Retained<NSNotificationCenter>, Retained<ProtocolObject<dyn NSObjectProtocol>>);

/// The window's input, listened to while this lives. Dropping it disengages
/// and stops listening.
pub struct Input {
    state: Rc<RefCell<Engagement>>,
    monitor: Retained<AnyObject>,
    observers: Vec<Observer>,
}

impl Input {
    pub fn attach(window: &Retained<NSWindow>, device: Device) -> Result<Self> {
        window.setAcceptsMouseMovedEvents(true);
        let engagement = Engagement { window: window.clone(), peer: device.peer, links: device.links, engaged: Engaged::No };
        let state = Rc::new(RefCell::new(engagement));
        let mask = WATCHED.iter().fold(NSEventMask(0), |mask, &kind| mask | NSEventMask::from_type(kind));
        let seen = state.clone();
        // A local monitor sees every event before AppKit dispatches it, so an
        // event taken here never reaches a menu shortcut, and a key let go
        // with Cmd held, which AppKit never dispatches, still reaches the device.
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit hands the monitor a live event.
            if seen.borrow_mut().take(unsafe { event.as_ref() }) {
                null_mut()
            } else {
                event.as_ptr()
            }
        });
        // SAFETY: the handler returns the event it was given, or null.
        let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) }
            .context("AppKit refused an event monitor")?;
        let mut input = Self { state, monitor, observers: Vec::new() };
        input.watch_exits(window, device.control);
        Ok(input)
    }

    /// Every way out of the window disengages; its close button also ends
    /// the view.
    fn watch_exits(&mut self, window: &NSWindow, control: UnboundedSender<Control>) {
        let windows = NSNotificationCenter::defaultCenter();
        let spaces = NSWorkspace::sharedWorkspace().notificationCenter();
        let exits = unsafe {
            [
                (&windows, NSWindowDidResignKeyNotification, Some(window)),
                (&windows, NSWindowWillExitFullScreenNotification, Some(window)),
                // A swipe to a Space with no windows leaves the window key.
                (&spaces, NSWorkspaceActiveSpaceDidChangeNotification, None),
            ]
        };
        for (center, name, object) in exits {
            let state = self.state.clone();
            let left = RcBlock::new(move |_: NonNull<NSNotification>| state.borrow_mut().disengage());
            self.observe(center, name, object, &left);
        }
        let state = self.state.clone();
        let closed = RcBlock::new(move |_: NonNull<NSNotification>| {
            state.borrow_mut().disengage();
            let _ = control.send(Control::View(None));
        });
        self.observe(&windows, unsafe { NSWindowWillCloseNotification }, Some(window), &closed);
    }

    fn observe(
        &mut self,
        center: &Retained<NSNotificationCenter>,
        name: &NSNotificationName,
        object: Option<&NSWindow>,
        block: &DynBlock<dyn Fn(NonNull<NSNotification>)>,
    ) {
        let object = object.map(|w| -> &AnyObject { w });
        // SAFETY: a nil queue runs the block on the posting thread, which for
        // these notifications is the main thread that owns the state.
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), object, None, block) };
        self.observers.push((center.clone(), token));
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.state.borrow_mut().disengage();
        // SAFETY: the monitor and the tokens are what AppKit handed out above.
        unsafe { NSEvent::removeMonitor(&self.monitor) };
        for (center, token) in &self.observers {
            unsafe { center.removeObserver(token.as_ref()) };
        }
    }
}

/// The device's events for one AppKit event. Only what exists for the event's
/// type is read: AppKit raises on the rest.
fn translate(event: &NSEvent) -> Vec<Event> {
    use NSEventType as Kind;
    let kind = event.r#type();
    match kind {
        Kind::MouseMoved | Kind::LeftMouseDragged | Kind::RightMouseDragged | Kind::OtherMouseDragged => {
            vec![Event::Pointer(PointerEvent::Motion { time: 0, dx: event.deltaX(), dy: event.deltaY() })]
        }
        Kind::LeftMouseDown | Kind::RightMouseDown | Kind::OtherMouseDown => vec![button(event.buttonNumber(), true)],
        Kind::LeftMouseUp | Kind::RightMouseUp | Kind::OtherMouseUp => vec![button(event.buttonNumber(), false)],
        Kind::ScrollWheel => {
            scroll(event.scrollingDeltaX(), event.scrollingDeltaY(), event.hasPreciseScrollingDeltas())
        }
        // The device repeats a held key itself, at its own rate.
        Kind::KeyDown if event.isARepeat() => vec![],
        Kind::KeyDown | Kind::KeyUp => key(event.keyCode(), kind == Kind::KeyDown).into_iter().collect(),
        Kind::FlagsChanged => modifier(event.keyCode(), event.modifierFlags().0),
        _ => vec![],
    }
}

/// AppKit numbers buttons 0 left, 1 right, then the rest; mapped as the
/// input-capture tap maps them.
fn button(number: isize, down: bool) -> Event {
    let button = match number {
        0 => BTN_LEFT,
        1 => BTN_RIGHT,
        3 => BTN_BACK,
        4 => BTN_FORWARD,
        _ => BTN_MIDDLE,
    };
    Event::Pointer(PointerEvent::Button { time: 0, button, state: down as u32 })
}

/// A trackpad's precise deltas go as they are and a wheel's lines as steps of
/// 120, as the input-capture tap sends them. Axis 0 is vertical.
fn scroll(dx: f64, dy: f64, precise: bool) -> Vec<Event> {
    let moved = [(0, dy), (1, dx)].into_iter().filter(|&(_, d)| d != 0.0);
    moved
        .map(|(axis, d)| {
            Event::Pointer(if precise {
                PointerEvent::Axis { time: 0, axis, value: d }
            } else {
                PointerEvent::AxisDiscrete120 { axis, value: (V120_PER_LINE * d).round() as i32 }
            })
        })
        .collect()
}

fn key(code: u16, down: bool) -> Option<Event> {
    let key = input_capture::linux_key(code)?;
    Some(Event::Keyboard(KeyboardEvent::Key { time: 0, key, state: down as u8 }))
}

/// A modifier key went down or up, and its own bit says which. Caps Lock only
/// says that it toggled, so it is tapped and the device toggles too.
fn modifier(code: u16, flags: usize) -> Vec<Event> {
    if code == CAPS_LOCK {
        return [true, false].into_iter().filter_map(|down| key(code, down)).collect();
    }
    let bit = MODIFIER_BITS.iter().find(|&&(c, _)| c == code).map(|&(_, bit)| bit);
    bit.and_then(|bit| key(code, flags & bit != 0)).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::scancode::Linux;

    fn pressed(key: Linux, down: bool) -> Event {
        Event::Keyboard(KeyboardEvent::Key { time: 0, key: key as u32, state: down as u8 })
    }

    #[test]
    fn keys_reach_the_device_as_linux_codes() {
        assert_eq!(key(0x00, true), Some(pressed(Linux::KeyA, true)), "kVK_ANSI_A");
        assert_eq!(key(0x24, false), Some(pressed(Linux::KeyEnter, false)), "kVK_Return");
    }

    #[test]
    fn a_modifier_key_says_by_its_own_bit_whether_it_went_down() {
        let shift = 1 << 17;
        assert_eq!(modifier(0x38, shift | 0x2), vec![pressed(Linux::KeyLeftShift, true)]);
        // Right shift let go while left shift is still held.
        assert_eq!(modifier(0x3C, shift | 0x2), vec![pressed(Linux::KeyRightShift, false)]);
        assert_eq!(modifier(0x37, 1 << 20 | 0x8), vec![pressed(Linux::KeyLeftMeta, true)]);
        assert_eq!(modifier(0x36, 0), vec![pressed(Linux::KeyRightmeta, false)]);
        assert_eq!(modifier(0x3F, 1 << 23), vec![], "fn has no key on the device");
    }

    #[test]
    fn caps_lock_is_tapped_so_both_machines_toggle_together() {
        let tap = vec![pressed(Linux::KeyCapsLock, true), pressed(Linux::KeyCapsLock, false)];
        assert_eq!(modifier(CAPS_LOCK, 1 << 16), tap, "turned on");
        assert_eq!(modifier(CAPS_LOCK, 0), tap, "turned off");
    }

    #[test]
    fn buttons_and_scrolling_match_what_the_tap_sends() {
        let click = |button| Event::Pointer(PointerEvent::Button { time: 0, button, state: 1 });
        assert_eq!(button(0, true), click(BTN_LEFT));
        assert_eq!(button(1, true), click(BTN_RIGHT));
        assert_eq!(button(2, true), click(BTN_MIDDLE));
        assert_eq!(button(4, true), click(BTN_FORWARD));
        assert_eq!(
            scroll(0.0, -2.5, true),
            vec![Event::Pointer(PointerEvent::Axis { time: 0, axis: 0, value: -2.5 })]
        );
        assert_eq!(
            scroll(1.0, 0.0, false),
            vec![Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 1, value: 40 })]
        );
        assert_eq!(scroll(0.0, 0.0, true), vec![], "a scroll that moved nothing sends nothing");
    }

    #[test]
    fn a_click_engages_only_a_key_window_and_is_not_forwarded() {
        use NSEventType as Kind;
        let cases = [
            (Engaged::No, Kind::LeftMouseDown, true, true, Verdict::Engage),
            // The first click on a window that is not key brings it forward.
            (Engaged::No, Kind::LeftMouseDown, true, false, Verdict::Pass),
            // Keys and the title bar stay macOS's until the picture is clicked.
            (Engaged::No, Kind::KeyDown, false, true, Verdict::Pass),
            (Engaged::Clicking, Kind::MouseMoved, false, true, Verdict::Forward),
            // The engaging click's release stays out of the device.
            (Engaged::Clicking, Kind::LeftMouseUp, false, true, Verdict::Swallow),
            (Engaged::Yes, Kind::LeftMouseDown, true, true, Verdict::Forward),
            (Engaged::Yes, Kind::LeftMouseUp, false, true, Verdict::Forward),
        ];
        for (engaged, kind, clicked, key_window, want) in cases {
            assert_eq!(verdict(engaged, kind, clicked, key_window), want, "{engaged:?}, {kind:?}");
        }
    }
}
