//! Drawing the arrange window: the backdrop, each screen as a lit slab, the
//! machines' name pills, and the edge a drag would snap to.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSGradient,
    NSGraphicsContext, NSShadow, NSStringDrawing,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::Side;

use super::layout::{centre, describe, park, rect_of, union, Labeled};
use super::ArrangeView;

/// Empty space around the drawing, in points. A machine's name pill sits in
/// this margin, above or below its screens.
const PADDING: f64 = 40.0;
/// Room kept at the bottom for the hint line.
const CAPTION: f64 = 34.0;
/// What the titlebar covers when the view has no window to ask — the snapshot
/// renders offscreen, and its picture should match what the window shows. The
/// window itself measures instead; 32 is what this style mask reports today.
const TITLEBAR: f64 = 32.0;

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

impl ArrangeView {
    /// Every machine in one coordinate space: this Mac where the system puts
    /// it, each peer parked off its chosen edge. No drag applied — dragging
    /// moves the drawing, not the layout, so the scale never jitters.
    fn boxes(&self) -> (Vec<Labeled>, Vec<Vec<Labeled>>) {
        let state = self.ivars();
        let local: Vec<Labeled> =
            state.local.iter().map(|s| (rect_of(s), describe(s))).collect();
        let peers = state.peers.iter().map(|peer| park(&local, peer)).collect();
        (local, peers)
    }

    /// The part of the view the drawing may use: everything the titlebar does
    /// not cover, less the strip the hint sits in. The window draws its content
    /// edge to edge, so nothing moves the drawing out from under the title but
    /// this — otherwise the peer's name pill lands on top of it.
    fn canvas(&self) -> NSRect {
        let bounds = self.bounds();
        let covered = self
            .window()
            .map(|w| bounds.size.height - w.contentLayoutRect().size.height)
            .unwrap_or(TITLEBAR)
            .clamp(0.0, bounds.size.height);
        NSRect::new(
            NSPoint::new(bounds.origin.x, bounds.origin.y + covered),
            NSSize::new(bounds.size.width, (bounds.size.height - covered - CAPTION).max(1.0)),
        )
    }

    pub(super) fn draw(&self) {
        let view = self.bounds();
        match gradient(&ink(0.105, 1.0), &ink(0.07, 1.0)) {
            Some(backdrop) => backdrop.drawInRect_angle(view, 270.0),
            None => {
                ink(0.09, 1.0).setFill();
                NSBezierPath::fillRect(view);
            }
        }
        self.caption(view);

        let Some((local_view, peer_views)) = self.placed() else { return };
        let state = self.ivars();
        let dragging = state.dragging.get();
        // Remember where everything ended up, so a click and a drop can be judged.
        *state.drawn.borrow_mut() = local_view.iter().map(|(r, _)| *r).collect();
        *state.groups.borrow_mut() = peer_views.iter().map(|g| union(g).unwrap_or_default()).collect();
        if let Some(mine) = union(&local_view) {
            let theirs = peer_views.iter().map(|g| union(g).map_or(centre(mine), centre)).collect();
            *state.centres.borrow_mut() = (centre(mine), theirs);
            if let Some(i) = dragging {
                let (side, display) = self.landed_on(i);
                let target = display.and_then(|d| local_view.get(d - 1)).map(|(r, _)| *r).unwrap_or(mine);
                self.show_target(target, side);
            }
        }

        for (rect, label) in &local_view {
            self.screen(*rect, label, false);
        }
        for group in &peer_views {
            for (rect, label) in group {
                self.screen(*rect, label, true);
            }
        }

        // Keep each name clear of the other machines' screens.
        let above = state.peers.iter().any(|p| p.side.get() == Side::Top);
        if let Some(mine) = union(&local_view) {
            self.badge(mine, "This Mac", false, above);
        }
        for (peer, group) in state.peers.iter().zip(&peer_views) {
            if let Some(theirs) = union(group) {
                self.badge(theirs, &peer.name, true, peer.side.get() == Side::Bottom);
            }
        }
    }

    /// Every screen where it is drawn: the layout scaled to fit the canvas,
    /// and the group being dragged moved along with the hand.
    fn placed(&self) -> Option<(Vec<Labeled>, Vec<Vec<Labeled>>)> {
        let canvas = self.canvas();
        let (local, peers) = self.boxes();
        let all: Vec<Labeled> = local.iter().chain(peers.iter().flatten()).cloned().collect();
        let world = union(&all)?;

        let scale = ((canvas.size.width - PADDING * 2.0) / world.size.width)
            .min((canvas.size.height - PADDING * 2.0) / world.size.height);
        let left = canvas.origin.x + (canvas.size.width - world.size.width * scale) / 2.0;
        let top = canvas.origin.y + (canvas.size.height - world.size.height * scale) / 2.0;
        let place = |r: NSRect| {
            NSRect::new(
                NSPoint::new(
                    left + (r.origin.x - world.origin.x) * scale,
                    top + (r.origin.y - world.origin.y) * scale,
                ),
                NSSize::new(r.size.width * scale, r.size.height * scale),
            )
        };

        let state = self.ivars();
        let (moved, dragging) = (state.drag.get(), state.dragging.get());
        let local_view: Vec<Labeled> =
            local.iter().map(|(r, l)| (place(*r), l.clone())).collect();
        let peer_views: Vec<Vec<Labeled>> = peers
            .iter()
            .enumerate()
            .map(|(i, group)| {
                let shift = if dragging == Some(i) { moved } else { NSPoint::new(0.0, 0.0) };
                group
                    .iter()
                    .map(|(r, l)| {
                        let r = place(*r);
                        (NSRect::new(NSPoint::new(r.origin.x + shift.x, r.origin.y + shift.y), r.size), l.clone())
                    })
                    .collect()
            })
            .collect();
        Some((local_view, peer_views))
    }

    /// While dragging, light up the edge the drop would choose.
    fn show_target(&self, mine: NSRect, side: Side) {
        let inset = 3.0;
        let (from, to) = match side {
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

        self.write_label(rect, label, is_peer);
    }

    /// The screen's name, with its size beneath when known, centred on the slab.
    fn write_label(&self, rect: NSRect, label: &str, is_peer: bool) {
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

    /// The hint, along the bottom. It lived in the window's subtitle before,
    /// where a whole sentence stretched the titlebar across the drawing.
    fn caption(&self, view: NSRect) {
        let text = NSString::from_str(HINT);
        let attributes = text_style(11.0, ink(0.46, 1.0));
        let size = unsafe { text.sizeWithAttributes(Some(&attributes)) };
        unsafe {
            text.drawAtPoint_withAttributes(
                NSPoint::new(
                    (view.size.width - size.width) / 2.0,
                    view.size.height - (CAPTION + size.height) / 2.0,
                ),
                Some(&attributes),
            )
        };
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

/// Shown along the bottom, because a drag that snaps needs explaining.
const HINT: &str = "Drag each machine to the side of this Mac it sits on";
