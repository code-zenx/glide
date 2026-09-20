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
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSGradient, NSGraphicsContext, NSShadow, NSStringDrawing, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSDictionary, NSPoint, NSRect, NSSize, NSString,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Control;
use crate::screens::Screen;
use crate::Side;

/// Empty space around the drawing, in points.
const PADDING: f64 = 40.0;
/// Screens on one desktop touch; the other machine is drawn touching too, the
/// way the Displays pane does it. The accent border is what tells them apart,
/// not a canyon of empty space.
const MACHINE_GAP: f64 = 0.0;
/// A drag shorter than this is a slip of the hand, not a rearrangement.
const NUDGE: f64 = 12.0;

/// A vertical two-stop gradient, or `None` if AppKit declines to make one.
fn gradient(from: &NSColor, to: &NSColor) -> Option<Retained<NSGradient>> {
    NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), from, to)
}

/// The accent, matching the app icon's hue.
fn accent(alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.525, 0.435, 0.965, alpha)
}

fn ink(level: f64, alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(level, level + 0.01, level + 0.04, alpha)
}

struct ViewState {
    local: Vec<Screen>,
    peer: Vec<Screen>,
    peer_name: String,
    side: Cell<Side>,
    /// Which of this Mac's displays the edge belongs to, counting from 1.
    display: Cell<Option<usize>>,
    /// Where each of this Mac's screens was drawn, so a drop can be matched to
    /// the screen it landed against.
    drawn: RefCell<Vec<NSRect>>,
    /// How far the peer group has been dragged, in view points.
    drag: Cell<NSPoint>,
    dragging: Cell<bool>,
    /// Filled in while drawing so the drop can be judged in the same space.
    centres: Cell<(NSPoint, NSPoint)>,
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
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars().dragging.set(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if !self.ivars().dragging.get() {
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
            self.ivars().dragging.set(false);
            self.settle();
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

    /// Which edge of which display the peer has been dragged to, judged by
    /// where it ended up rather than by how far the hand moved.
    fn landed_on(&self) -> (Side, Option<usize>) {
        let state = self.ivars();
        let moved = state.drag.get();
        if moved.x.abs() < NUDGE && moved.y.abs() < NUDGE {
            return (state.side.get(), state.display.get());
        }
        let (_, theirs) = state.centres.get();
        // The screen it was dropped nearest owns the new edge.
        let drawn = state.drawn.borrow();
        let nearest = drawn
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                gap(centre(**a), theirs).total_cmp(&gap(centre(**b), theirs))
            })
            .map(|(i, r)| (i + 1, centre(*r)));
        let (display, mine) = match nearest {
            Some((index, centre)) => (Some(index), centre),
            None => (None, state.centres.get().0),
        };
        let (dx, dy) = (theirs.x - mine.x, theirs.y - mine.y);
        let side = if dx.abs() >= dy.abs() {
            if dx > 0.0 {
                Side::Right
            } else {
                Side::Left
            }
        } else if dy > 0.0 {
            Side::Bottom
        } else {
            Side::Top
        };
        // One display means the whole desktop is that display; say so plainly.
        (side, if drawn.len() > 1 { display } else { None })
    }

    fn settle(&self) {
        let state = self.ivars();
        let (side, display) = self.landed_on();
        state.drag.set(NSPoint::new(0.0, 0.0));
        if (side, display) != (state.side.get(), state.display.get()) {
            state.side.set(side);
            state.display.set(display);
            if let Err(e) = crate::set_position(&state.config_path, side, display) {
                tracing::warn!("cannot save the new arrangement: {e}");
            }
            let _ = state.control.send(Control::SetPosition(0, side, display));
        }
        self.setNeedsDisplay(true);
    }

    /// Both machines in one coordinate space: this Mac where the system puts
    /// it, the peer parked off the chosen edge. No drag applied — dragging
    /// moves the drawing, not the layout, so the scale never jitters.
    fn boxes(&self) -> (Vec<(NSRect, String)>, Vec<(NSRect, String)>) {
        let state = self.ivars();
        let local: Vec<(NSRect, String)> =
            state.local.iter().map(|s| (rect_of(s), describe(s))).collect();
        // The peer rests against the display it belongs to, so the drawing
        // matches where the barrier actually is.
        let bounds = state
            .display
            .get()
            .and_then(|d| local.get(d - 1))
            .map(|(r, _)| *r)
            .or_else(|| union(&local))
            .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)));

        let peers: Vec<(NSRect, String)> = if state.peer.is_empty() {
            // The peer has not said what it has; draw one stand-in screen.
            vec![(
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1920.0, 1080.0)),
                "screen".to_string(),
            )]
        } else {
            state.peer.iter().map(|s| (rect_of(s), describe(s))).collect()
        };
        let theirs = union(&peers).unwrap_or(bounds);

        let (dx, dy) = match state.side.get() {
            Side::Left => (
                bounds.origin.x - theirs.size.width - MACHINE_GAP - theirs.origin.x,
                centre_on(bounds.origin.y, bounds.size.height, theirs.origin.y, theirs.size.height),
            ),
            Side::Right => (
                bounds.origin.x + bounds.size.width + MACHINE_GAP - theirs.origin.x,
                centre_on(bounds.origin.y, bounds.size.height, theirs.origin.y, theirs.size.height),
            ),
            Side::Top => (
                centre_on(bounds.origin.x, bounds.size.width, theirs.origin.x, theirs.size.width),
                bounds.origin.y - theirs.size.height - MACHINE_GAP - theirs.origin.y,
            ),
            Side::Bottom => (
                centre_on(bounds.origin.x, bounds.size.width, theirs.origin.x, theirs.size.width),
                bounds.origin.y + bounds.size.height + MACHINE_GAP - theirs.origin.y,
            ),
        };
        let peers = peers
            .into_iter()
            .map(|(r, label)| {
                (NSRect::new(NSPoint::new(r.origin.x + dx, r.origin.y + dy), r.size), label)
            })
            .collect();
        (local, peers)
    }

    fn draw(&self) {
        let view = self.bounds();
        match gradient(&ink(0.105, 1.0), &ink(0.07, 1.0)) {
            Some(backdrop) => backdrop.drawInRect_angle(view, 270.0),
            None => {
                ink(0.09, 1.0).setFill();
                NSBezierPath::fillRect(view);
            }
        }

        let (local, peers) = self.boxes();
        let all: Vec<(NSRect, String)> = local.iter().chain(peers.iter()).cloned().collect();
        let Some(world) = union(&all) else { return };

        let scale = ((view.size.width - PADDING * 2.0) / world.size.width)
            .min((view.size.height - PADDING * 2.0) / world.size.height);
        let left = (view.size.width - world.size.width * scale) / 2.0;
        let top = (view.size.height - world.size.height * scale) / 2.0;
        let place = |r: NSRect| {
            NSRect::new(
                NSPoint::new(
                    left + (r.origin.x - world.origin.x) * scale,
                    top + (r.origin.y - world.origin.y) * scale,
                ),
                NSSize::new(r.size.width * scale, r.size.height * scale),
            )
        };

        let moved = self.ivars().drag.get();
        let local_view: Vec<(NSRect, String)> =
            local.iter().map(|(r, l)| (place(*r), l.clone())).collect();
        let peer_view: Vec<(NSRect, String)> = peers
            .iter()
            .map(|(r, l)| {
                let r = place(*r);
                (NSRect::new(NSPoint::new(r.origin.x + moved.x, r.origin.y + moved.y), r.size), l.clone())
            })
            .collect();

        // Remember where everything ended up, so a drop can be judged.
        *self.ivars().drawn.borrow_mut() = local_view.iter().map(|(r, _)| *r).collect();
        if let (Some(mine), Some(theirs)) = (union(&local_view), union(&peer_view)) {
            self.ivars().centres.set((centre(mine), centre(theirs)));
            if self.ivars().dragging.get() {
                let (_, display) = self.landed_on();
                let target = display
                    .and_then(|d| local_view.get(d - 1))
                    .map(|(r, _)| *r)
                    .unwrap_or(mine);
                self.show_target(target);
            }
        }

        for (rect, label) in &local_view {
            self.screen(*rect, label, false);
        }
        for (rect, label) in &peer_view {
            self.screen(*rect, label, true);
        }

        // Keep each name clear of the other machine's screens.
        let side = self.ivars().side.get();
        if let Some(mine) = union(&local_view) {
            self.badge(mine, "This Mac", false, side == Side::Top);
        }
        if let Some(theirs) = union(&peer_view) {
            self.badge(theirs, &self.ivars().peer_name, true, side == Side::Bottom);
        }
    }

    /// While dragging, light up the edge the drop would choose.
    fn show_target(&self, mine: NSRect) {
        let inset = 3.0;
        let (from, to) = match self.landed_on().0 {
            Side::Left => (
                NSPoint::new(mine.origin.x - inset, mine.origin.y),
                NSPoint::new(mine.origin.x - inset, mine.origin.y + mine.size.height),
            ),
            Side::Right => (
                NSPoint::new(mine.origin.x + mine.size.width + inset, mine.origin.y),
                NSPoint::new(mine.origin.x + mine.size.width + inset, mine.origin.y + mine.size.height),
            ),
            Side::Top => (
                NSPoint::new(mine.origin.x, mine.origin.y - inset),
                NSPoint::new(mine.origin.x + mine.size.width, mine.origin.y - inset),
            ),
            Side::Bottom => (
                NSPoint::new(mine.origin.x, mine.origin.y + mine.size.height + inset),
                NSPoint::new(mine.origin.x + mine.size.width, mine.origin.y + mine.size.height + inset),
            ),
        };
        let path = NSBezierPath::bezierPath();
        path.moveToPoint(from);
        path.lineToPoint(to);
        path.setLineWidth(4.0);
        path.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
        accent(0.9).setStroke();
        path.stroke();
    }

    /// One screen: a rounded slab, softly lit, with its name and size inside.
    fn screen(&self, rect: NSRect, label: &str, is_peer: bool) {
        let radius = (rect.size.height * 0.06).clamp(5.0, 14.0);
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);

        NSGraphicsContext::saveGraphicsState_class();
        let shadow = NSShadow::new();
        shadow.setShadowBlurRadius(18.0);
        shadow.setShadowOffset(NSSize::new(0.0, 6.0));
        shadow.setShadowColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.45)));
        shadow.set();
        let fill = if is_peer {
            gradient(&accent(0.42), &accent(0.20))
        } else {
            gradient(&ink(0.30, 1.0), &ink(0.20, 1.0))
        };
        match fill {
            Some(fill) => fill.drawInBezierPath_angle(&path, 270.0),
            None => {
                if is_peer { accent(0.30).setFill() } else { ink(0.25, 1.0).setFill() }
                path.fill();
            }
        }
        NSGraphicsContext::restoreGraphicsState_class();

        if is_peer {
            accent(0.95).setStroke();
        } else {
            ink(0.52, 1.0).setStroke();
        }
        path.setLineWidth(1.5);
        path.stroke();

        // A hairline along the top edge, the way a lit panel catches light.
        let sheen = NSBezierPath::bezierPath();
        sheen.moveToPoint(NSPoint::new(rect.origin.x + radius, rect.origin.y + 0.75));
        sheen.lineToPoint(NSPoint::new(rect.origin.x + rect.size.width - radius, rect.origin.y + 0.75));
        sheen.setLineWidth(1.5);
        ink(0.95, if is_peer { 0.22 } else { 0.16 }).setStroke();
        sheen.stroke();

        let (name, size_text) = match label.split_once('\n') {
            Some((name, size)) => (name, Some(size)),
            None => (label, None),
        };
        let colour = if is_peer { accent(1.0) } else { ink(0.82, 1.0) };
        let text = NSString::from_str(name);
        let attributes = text_style(12.0, colour);
        let measured = unsafe { text.sizeWithAttributes(Some(&attributes)) };
        let centre_y = rect.origin.y + (rect.size.height - measured.height) / 2.0;
        let stacked = size_text.is_some();
        unsafe {
            text.drawAtPoint_withAttributes(
                NSPoint::new(
                    rect.origin.x + (rect.size.width - measured.width) / 2.0,
                    if stacked { centre_y - measured.height * 0.55 } else { centre_y },
                ),
                Some(&attributes),
            )
        };
        if let Some(size_text) = size_text {
            let faint = text_style(10.0, if is_peer { accent(0.65) } else { ink(0.55, 1.0) });
            let text = NSString::from_str(size_text);
            let measured = unsafe { text.sizeWithAttributes(Some(&faint)) };
            unsafe {
                text.drawAtPoint_withAttributes(
                    NSPoint::new(
                        rect.origin.x + (rect.size.width - measured.width) / 2.0,
                        centre_y + measured.height * 0.55,
                    ),
                    Some(&faint),
                )
            };
        }
    }

    /// The machine's name on a pill above its group of screens.
    fn badge(&self, group: NSRect, name: &str, is_peer: bool, below: bool) {
        let text = NSString::from_str(name);
        let colour = if is_peer { accent(1.0) } else { ink(0.72, 1.0) };
        let attributes = text_style(12.0, colour);
        let size = unsafe { text.sizeWithAttributes(Some(&attributes)) };
        let pill = NSRect::new(
            NSPoint::new(
                group.origin.x + (group.size.width - size.width) / 2.0 - 10.0,
                if below {
                    group.origin.y + group.size.height + 9.0
                } else {
                    group.origin.y - size.height - 16.0
                },
            ),
            NSSize::new(size.width + 20.0, size.height + 7.0),
        );
        let radius = pill.size.height / 2.0;
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, radius, radius);
        if is_peer { accent(0.16).setFill() } else { ink(0.22, 1.0).setFill() }
        path.fill();
        if is_peer { accent(0.55).setStroke() } else { ink(0.34, 1.0).setStroke() }
        path.setLineWidth(1.0);
        path.stroke();
        unsafe {
            text.drawAtPoint_withAttributes(
                NSPoint::new(pill.origin.x + 10.0, pill.origin.y + 3.0),
                Some(&attributes),
            )
        };
    }
}

fn text_style(points: f64, colour: Retained<NSColor>) -> Retained<NSDictionary<NSString, AnyObject>> {
    let font = NSFont::systemFontOfSize(points);
    NSDictionary::from_slices(
        &[unsafe { NSFontAttributeName }, unsafe { NSForegroundColorAttributeName }],
        &[font.as_ref() as &AnyObject, colour.as_ref()],
    )
}

/// Name over resolution, the way the Displays pane labels them.
fn describe(screen: &Screen) -> String {
    format!("{}\n{} × {}", screen.name, screen.w, screen.h)
}

fn rect_of(screen: &Screen) -> NSRect {
    NSRect::new(
        NSPoint::new(screen.x as f64, screen.y as f64),
        NSSize::new(screen.w as f64, screen.h as f64),
    )
}

/// Distance between two points, for picking the nearest screen.
fn gap(a: NSPoint, b: NSPoint) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

fn centre(r: NSRect) -> NSPoint {
    NSPoint::new(r.origin.x + r.size.width / 2.0, r.origin.y + r.size.height / 2.0)
}

/// The offset that centres a group of this length against another.
fn centre_on(mine: f64, mine_len: f64, theirs: f64, theirs_len: f64) -> f64 {
    mine + (mine_len - theirs_len) / 2.0 - theirs
}

fn union(boxes: &[(NSRect, String)]) -> Option<NSRect> {
    let mut it = boxes.iter().map(|(r, _)| *r);
    let first = it.next()?;
    let (mut x0, mut y0) = (first.origin.x, first.origin.y);
    let (mut x1, mut y1) = (x0 + first.size.width, y0 + first.size.height);
    for r in it {
        x0 = x0.min(r.origin.x);
        y0 = y0.min(r.origin.y);
        x1 = x1.max(r.origin.x + r.size.width);
        y1 = y1.max(r.origin.y + r.size.height);
    }
    Some(NSRect::new(NSPoint::new(x0, y0), NSSize::new(x1 - x0, y1 - y0)))
}

/// Shown under the title, because a drag that snaps needs explaining.
const HINT: &str = "Drag the other machine to the side of this Mac it sits on";

/// Render the window's drawing straight to a PNG, so the design can be looked
/// at without a screen recording permission or a human with a camera.
pub fn snapshot(
    mtm: MainThreadMarker,
    local: Vec<Screen>,
    peer: Vec<Screen>,
    peer_name: String,
    side: Side,
    display: Option<usize>,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    use objc2_app_kit::NSBitmapImageFileType;

    let (control, _rx) = tokio::sync::mpsc::unbounded_channel();
    let view = ArrangeView::new(
        mtm,
        ViewState {
            local,
            peer,
            peer_name,
            side: Cell::new(side),
            display: Cell::new(display),
            drawn: RefCell::new(Vec::new()),
            drag: Cell::new(NSPoint::new(0.0, 0.0)),
            dragging: Cell::new(false),
            centres: Cell::new((NSPoint::new(0.0, 0.0), NSPoint::new(0.0, 0.0))),
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
    peer: Vec<Screen>,
    peer_name: String,
    side: Side,
    display: Option<usize>,
    control: UnboundedSender<Control>,
    config_path: PathBuf,
) -> Retained<NSWindow> {
    let view = ArrangeView::new(
        mtm,
        ViewState {
            local,
            peer,
            peer_name,
            side: Cell::new(side),
            display: Cell::new(display),
            drawn: RefCell::new(Vec::new()),
            drag: Cell::new(NSPoint::new(0.0, 0.0)),
            dragging: Cell::new(false),
            centres: Cell::new((NSPoint::new(0.0, 0.0), NSPoint::new(0.0, 0.0))),
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
    window.setSubtitle(&NSString::from_str(HINT));
    window.setTitlebarAppearsTransparent(true);
    window.setMinSize(NSSize::new(520.0, 360.0));
    window.setContentView(Some(&view));
    window.center();
    unsafe { window.setReleasedWhenClosed(false) };
    window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_union_covers_every_screen() {
        let boxes = vec![
            (NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 50.0)), String::new()),
            (NSRect::new(NSPoint::new(-40.0, 20.0), NSSize::new(10.0, 90.0)), String::new()),
        ];
        let u = union(&boxes).unwrap();
        assert_eq!((u.origin.x, u.origin.y), (-40.0, 0.0));
        assert_eq!((u.size.width, u.size.height), (140.0, 110.0));
        assert!(union(&[]).is_none());
    }

    #[test]
    fn a_group_centres_against_another() {
        // A 200-long group against a 1000-long one starts 400 in.
        assert_eq!(centre_on(0.0, 1000.0, 0.0, 200.0), 400.0);
        // Whatever the group's own origin, the offset takes it to the centre.
        assert_eq!(centre_on(0.0, 1000.0, -50.0, 200.0), 450.0);
    }
}
