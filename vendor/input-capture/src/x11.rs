//! glide: an edge watcher for X11, not a real capture backend.
//!
//! A glide client never grabs input; all it needs to know is when the cursor
//! the server is driving gets pushed against the edge that leads back. So this
//! polls the pointer and reports `Begin` once per arrival at an armed edge.
//! ponytail: polling at 100 Hz and the root window's bounding box, so a gap in
//! an L-shaped multi-monitor layout is not an edge. XInput2 barriers if a
//! server ever runs on X11.

use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use tokio::time::{self, Interval, MissedTickBehavior};
use x11::xlib;

use super::{Capture, CaptureError, CaptureEvent, Position, error::X11InputCaptureCreationError};

/// How far the cursor is stepped back off an edge after a crossing, so the
/// next push against it counts as a new arrival.
const STEP_BACK: i32 = 2;

pub struct X11InputCapture {
    display: *mut xlib::Display,
    root: xlib::Window,
    edges: Vec<Position>,
    /// False while the cursor rests on an edge it already crossed.
    armed: bool,
    /// The edge crossed last, to step back from on release.
    crossed: Option<Position>,
    interval: Interval,
}

// Only ever touched through `&mut self`, one thread at a time.
unsafe impl Send for X11InputCapture {}

impl X11InputCapture {
    pub fn new() -> std::result::Result<Self, X11InputCaptureCreationError> {
        let display = unsafe { xlib::XOpenDisplay(ptr::null()) };
        if display.is_null() {
            return Err(X11InputCaptureCreationError::NoDisplay);
        }
        let root = unsafe { xlib::XDefaultRootWindow(display) };
        let mut interval = time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok(Self { display, root, edges: Vec::new(), armed: false, crossed: None, interval })
    }

    /// Cursor position and root size, both in pixels.
    fn pointer(&self) -> Option<(i32, i32, i32, i32)> {
        let (mut root, mut child) = (0, 0);
        let (mut x, mut y, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0);
        let mut attrs: xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
        unsafe {
            let ok = xlib::XQueryPointer(
                self.display, self.root, &mut root, &mut child, &mut x, &mut y, &mut wx, &mut wy, &mut mask,
            );
            if ok == 0 || xlib::XGetWindowAttributes(self.display, self.root, &mut attrs) == 0 {
                return None;
            }
        }
        Some((x, y, attrs.width, attrs.height))
    }
}

impl Drop for X11InputCapture {
    fn drop(&mut self) {
        unsafe { xlib::XCloseDisplay(self.display) };
    }
}

/// The armed edge the cursor is on, if any. X clamps the cursor to the root
/// window, so "on the edge" means the outermost pixel row or column.
fn edge_at(x: i32, y: i32, w: i32, h: i32, edges: &[Position]) -> Option<Position> {
    edges.iter().copied().find(|edge| match edge {
        Position::Left => x <= 0,
        Position::Right => x >= w - 1,
        Position::Top => y <= 0,
        Position::Bottom => y >= h - 1,
    })
}

#[async_trait]
impl Capture for X11InputCapture {
    /// glide: this backend has no per-display barrier; the setting is ignored.
    async fn restrict_to_display(&mut self, _index: Option<usize>) {}

    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        if !self.edges.contains(&pos) {
            self.edges.push(pos);
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.edges.retain(|p| *p != pos);
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        let Some(edge) = self.crossed.take() else { return Ok(()) };
        let (dx, dy) = match edge {
            Position::Left => (STEP_BACK, 0),
            Position::Right => (-STEP_BACK, 0),
            Position::Top => (0, STEP_BACK),
            Position::Bottom => (0, -STEP_BACK),
        };
        unsafe {
            xlib::XWarpPointer(self.display, 0, 0, 0, 0, 0, 0, dx, dy);
            xlib::XFlush(self.display);
        }
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for X11InputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            ready!(self.interval.poll_tick(cx));
            if self.edges.is_empty() {
                continue;
            }
            let Some((x, y, w, h)) = self.pointer() else { continue };
            match edge_at(x, y, w, h, &self.edges) {
                Some(edge) if self.armed => {
                    self.armed = false;
                    self.crossed = Some(edge);
                    return Poll::Ready(Some(Ok((edge, CaptureEvent::Begin))));
                }
                Some(_) => {}
                None => self.armed = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_armed_edges_count() {
        let edges = [Position::Bottom];
        assert_eq!(edge_at(500, 1079, 1920, 1080, &edges), Some(Position::Bottom));
        assert_eq!(edge_at(0, 500, 1920, 1080, &edges), None, "left is not armed");
        assert_eq!(edge_at(500, 1078, 1920, 1080, &edges), None, "one pixel short");
        let all = [Position::Left, Position::Right, Position::Top];
        assert_eq!(edge_at(1919, 10, 1920, 1080, &all), Some(Position::Right));
        assert_eq!(edge_at(10, 0, 1920, 1080, &all), Some(Position::Top));
        assert_eq!(edge_at(10, 10, 1920, 1080, &all), None);
    }
}
