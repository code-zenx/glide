//! A window that draws the real screens on both machines and lets you drag the
//! other machine into place, the way Displays → Arrange does.
//!
//! What it can honestly control: which edge of this Mac the other machine sits
//! on. The capture backend grabs a whole screen side, so a peer dropped
//! anywhere to the right becomes "right" — the drop snaps rather than aligning
//! to the pixel. The snap target is shown while dragging so that is not a
//! surprise.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSEvent, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{ns_string, MainThreadMarker, NSDictionary, NSPoint, NSRect, NSSize};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::screens::Screen;
use crate::Side;

mod layout;
mod paint;

use layout::judge;

/// A drag shorter than this is a slip of the hand, not a rearrangement.
const NUDGE: f64 = 12.0;

/// One client as the window draws it.
pub struct PeerView {
    pub name: String,
    pub screens: Vec<Screen>,
    pub side: Cell<Side>,
    /// Which of this Mac's displays the edge belongs to, counting from 1.
    pub display: Cell<Option<usize>>,
}

impl PeerView {
    pub fn new(name: String, screens: Vec<Screen>, side: Side, display: Option<usize>) -> Self {
        Self { name, screens, side: Cell::new(side), display: Cell::new(display) }
    }
}

struct ViewState {
    local: Vec<Screen>,
    peers: Vec<PeerView>,
    /// Where each of this Mac's screens was drawn, so a drop can be matched to
    /// the screen it landed against.
    drawn: RefCell<Vec<NSRect>>,
    /// Where each peer's group was drawn, to find the one a click lands on.
    groups: RefCell<Vec<NSRect>>,
    /// How far the grabbed group has been dragged, in view points.
    drag: Cell<NSPoint>,
    /// Which peer is being dragged, by its place in `peers`.
    dragging: Cell<Option<usize>>,
    /// Filled in while drawing so the drop can be judged in the same space:
    /// this Mac's centre, and each peer group's.
    centres: RefCell<(NSPoint, Vec<NSPoint>)>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "GlideArrangeView"]
    #[ivars = ViewState]
    struct ArrangeView;

    impl ArrangeView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            // Screen coordinates run downwards; matching them keeps this maths
            // the same as what both systems report.
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // Grab whichever peer group is under the click; empty space grabs nothing.
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            let hit = self.ivars().groups.borrow().iter().position(|g| {
                at.x >= g.origin.x && at.x <= g.origin.x + g.size.width && at.y >= g.origin.y && at.y <= g.origin.y + g.size.height
            });
            self.ivars().dragging.set(hit);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().dragging.get().is_none() {
                return;
            }
            // One point of cursor travel is one point of box travel.
            let at = self.ivars().drag.get();
            self.ivars()
                .drag
                .set(NSPoint::new(at.x + event.deltaX(), at.y + event.deltaY()));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(i) = self.ivars().dragging.take() {
                self.settle(i);
            }
        }
    }
);

impl ArrangeView {
    fn new(mtm: MainThreadMarker, state: ViewState) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(state);
        unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(760.0, 480.0))]
        }
    }

    /// Which edge of which display peer `i` has been dragged to, judged by
    /// where it ended up rather than by how far the hand moved.
    fn landed_on(&self, i: usize) -> (Side, Option<usize>) {
        let state = self.ivars();
        let peer = &state.peers[i];
        let moved = state.drag.get();
        if moved.x.abs() < NUDGE && moved.y.abs() < NUDGE {
            return (peer.side.get(), peer.display.get());
        }
        let centres = state.centres.borrow();
        let theirs = centres.1.get(i).copied().unwrap_or(centres.0);
        judge(&state.drawn.borrow(), centres.0, theirs)
    }

    fn settle(&self, i: usize) {
        let state = self.ivars();
        let (side, display) = self.landed_on(i);
        state.drag.set(NSPoint::new(0.0, 0.0));
        let peer = &state.peers[i];
        if (side, display) != (peer.side.get(), peer.display.get()) {
            peer.side.set(side);
            peer.display.set(display);
            if let Err(e) = crate::set_position(&state.config_path, &peer.name, side, display) {
                tracing::warn!("cannot save the new arrangement: {e}");
            }
            let _ = state.control.send(Control::SetPosition(peer.name.clone(), side, display));
        }
        self.setNeedsDisplay(true);
    }
}

/// Render the window's drawing straight to a PNG, so the design can be looked
/// at without a screen recording permission or a human with a camera.
pub fn snapshot(
    mtm: MainThreadMarker,
    local: Vec<Screen>,
    peers: Vec<PeerView>,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    use objc2_app_kit::NSBitmapImageFileType;

    let (control, _rx) = tokio::sync::mpsc::unbounded_channel();
    let view = ArrangeView::new(
        mtm,
        ViewState {
            local,
            peers,
            drawn: RefCell::new(Vec::new()),
            groups: RefCell::new(Vec::new()),
            drag: Cell::new(NSPoint::new(0.0, 0.0)),
            dragging: Cell::new(None),
            centres: RefCell::new((NSPoint::new(0.0, 0.0), Vec::new())),
            control,
            config_path: PathBuf::new(),
        },
    );
    let bounds = view.bounds();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or_else(|| anyhow::anyhow!("cannot make a bitmap for the view"))?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or_else(|| anyhow::anyhow!("cannot encode the view as a PNG"))?;
    std::fs::write(out, data.to_vec())?;
    Ok(())
}

pub fn window(
    mtm: MainThreadMarker,
    local: Vec<Screen>,
    peers: Vec<PeerView>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
) -> Retained<NSWindow> {
    let view = ArrangeView::new(
        mtm,
        ViewState {
            local,
            peers,
            drawn: RefCell::new(Vec::new()),
            groups: RefCell::new(Vec::new()),
            drag: Cell::new(NSPoint::new(0.0, 0.0)),
            dragging: Cell::new(None),
            centres: RefCell::new((NSPoint::new(0.0, 0.0), Vec::new())),
            control,
            config_path,
        },
    );

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(760.0, 480.0)),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("Arrange Screens"));
    window.setTitlebarAppearsTransparent(true);
    window.setMinSize(NSSize::new(520.0, 360.0));
    window.setContentView(Some(&view));
    window.center();
    unsafe { window.setReleasedWhenClosed(false) };
    window
}
