//! Where the screens sit, in screen coordinates: each peer parked against
//! its edge, and the edge a dropped group lands on.

use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::screens::Screen;
use crate::Side;

use super::PeerView;

/// One screen as drawn: where it sits, and the label written on it.
pub(super) type Labeled = (NSRect, String);

/// Screens on one desktop touch; the other machine is drawn touching too, the
/// way the Displays pane does it. The accent border is what tells them apart,
/// not a canyon of empty space.
const MACHINE_GAP: f64 = 0.0;

/// A peer's screens moved to rest against the edge it sits on: the display it
/// belongs to, or the whole desktop. A peer that has not said what it has
/// gets one stand-in screen.
pub(super) fn park(local: &[Labeled], peer: &PeerView) -> Vec<Labeled> {
    let bounds = peer
        .display
        .get()
        .and_then(|d| local.get(d - 1))
        .map(|(r, _)| *r)
        .or_else(|| union(local))
        .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)));
    let screens: Vec<Labeled> = if peer.screens.is_empty() {
        vec![(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1920.0, 1080.0)), "screen".to_string())]
    } else {
        peer.screens.iter().map(|s| (rect_of(s), describe(s))).collect()
    };
    let theirs = union(&screens).unwrap_or(bounds);
    let (dx, dy) = match peer.side.get() {
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
    screens
        .into_iter()
        .map(|(r, label)| (NSRect::new(NSPoint::new(r.origin.x + dx, r.origin.y + dy), r.size), label))
        .collect()
}

/// Where a dropped group belongs: the edge of the nearest of this Mac's drawn
/// screens that faces it. One display means the whole desktop, said plainly.
pub(super) fn judge(drawn: &[NSRect], mine: NSPoint, theirs: NSPoint) -> (Side, Option<usize>) {
    let nearest = drawn
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| gap(centre(**a), theirs).total_cmp(&gap(centre(**b), theirs)))
        .map(|(i, r)| (i + 1, centre(*r)));
    let (display, mine) = match nearest {
        Some((index, centre)) => (Some(index), centre),
        None => (None, mine),
    };
    let (dx, dy) = (theirs.x - mine.x, theirs.y - mine.y);
    let side = if dx.abs() >= dy.abs() {
        if dx > 0.0 { Side::Right } else { Side::Left }
    } else if dy > 0.0 {
        Side::Bottom
    } else {
        Side::Top
    };
    (side, if drawn.len() > 1 { display } else { None })
}

/// Name over resolution, the way the Displays pane labels them.
pub(super) fn describe(screen: &Screen) -> String {
    format!("{}\n{} × {}", screen.name, screen.w, screen.h)
}

pub(super) fn rect_of(screen: &Screen) -> NSRect {
    NSRect::new(
        NSPoint::new(screen.x as f64, screen.y as f64),
        NSSize::new(screen.w as f64, screen.h as f64),
    )
}

/// Distance between two points, for picking the nearest screen.
fn gap(a: NSPoint, b: NSPoint) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

pub(super) fn centre(r: NSRect) -> NSPoint {
    NSPoint::new(r.origin.x + r.size.width / 2.0, r.origin.y + r.size.height / 2.0)
}

/// The offset that centres a group of this length against another.
fn centre_on(mine: f64, mine_len: f64, theirs: f64, theirs_len: f64) -> f64 {
    mine + (mine_len - theirs_len) / 2.0 - theirs
}

pub(super) fn union(boxes: &[Labeled]) -> Option<NSRect> {
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

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    #[test]
    fn a_drop_lands_on_the_nearest_display_edge() {
        // Two displays side by side; a group dropped right of the second.
        let drawn = [rect(0.0, 0.0, 100.0, 60.0), rect(100.0, 0.0, 100.0, 60.0)];
        assert_eq!(judge(&drawn, NSPoint::new(100.0, 30.0), NSPoint::new(260.0, 30.0)), (Side::Right, Some(2)));
        // Above the first one.
        assert_eq!(judge(&drawn, NSPoint::new(100.0, 30.0), NSPoint::new(50.0, -80.0)), (Side::Top, Some(1)));
        // With one display the edge belongs to the whole desktop.
        assert_eq!(judge(&drawn[..1], NSPoint::new(50.0, 30.0), NSPoint::new(-90.0, 30.0)), (Side::Left, None));
    }

    #[test]
    fn each_peer_parks_on_its_own_edge() {
        let local = vec![(rect(0.0, 0.0, 1000.0, 600.0), String::new())];
        let top = PeerView::new("anorak".into(), vec![], Side::Top, None);
        let right = PeerView::new("anton".into(), vec![], Side::Right, None);
        let (t, r) = (park(&local, &top)[0].0, park(&local, &right)[0].0);
        assert_eq!(t.origin.y + t.size.height, 0.0, "anorak sits right above");
        assert_eq!(r.origin.x, 1000.0, "anton sits right of the Mac");
    }
}
