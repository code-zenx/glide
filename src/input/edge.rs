//! Which screen edges are armed. Every change of link, switch, control,
//! pause, extended or viewed device asks here, so the rule lives in one place.

use anyhow::Result;
use input_capture::{CaptureHandle, InputCapture};

use super::Session;
use crate::Peer;

/// Whether a peer's screen edge should be armed: only with a live link behind
/// it, never while the two screens are one picture — on a server, the peer
/// showing our extended display, where the cursor belongs on the virtual
/// screen; on a client, the server viewing this screen, where an edge would
/// walk the cursor out of the picture — and never while the peer is switched
/// off. Other peers stay reachable meanwhile.
fn edge_wanted(linked: bool, screen_shared: bool, enabled: bool) -> bool {
    linked && !screen_shared && enabled
}

pub(super) async fn set_edge(capture: &mut InputCapture, handle: usize, peer: &Peer, on: bool) -> Result<()> {
    if on {
        // A display numbered for people is 1-based; the backend counts from 0.
        capture.restrict_to_display(peer.display.map(|d| d.saturating_sub(1))).await;
        capture.create(handle as CaptureHandle, peer.position.into()).await?;
    } else {
        capture.destroy(handle as CaptureHandle).await?;
    }
    Ok(())
}

impl Session {
    /// Whether device `i`'s edge should be armed now, by `edge_wanted`.
    /// `viewed`: the server is showing this client's screen. A client hears
    /// that in each `View` message and keeps no record of it, so only that
    /// handler passes true.
    pub(super) fn wants_edge(&self, i: usize, viewed: bool) -> bool {
        let shared = viewed || self.extend == Some(i) || self.viewing == Some(i);
        let on = self.enabled[i] && self.control_on[i] && !self.held_off[i];
        !self.gone[i] && edge_wanted(self.linked[i], shared, on)
    }

    /// Arm or drop device `i`'s edge to match `wants_edge`. True when it
    /// changed. Without a capture nothing changes: `armed` mirrors what the
    /// backend actually holds.
    pub(super) async fn sync_edge(&mut self, i: usize, viewed: bool) -> Result<bool> {
        let want = self.wants_edge(i, viewed);
        let Some(capture) = self.capture.as_mut().filter(|_| want != self.armed[i]) else { return Ok(false) };
        self.armed[i] = want;
        set_edge(capture, i, &self.config.peers[i], want).await?;
        Ok(true)
    }

    /// Every device's edge, after the extended or viewed device changed.
    pub(super) async fn sync_edges(&mut self) -> Result<()> {
        for i in 0..self.config.peers.len() {
            self.sync_edge(i, false).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::tests::devices;

    /// Extending must take the edge away, or the cursor leaves for the peer
    /// instead of walking onto the virtual display beside it.
    #[test]
    fn extending_disarms_the_edge() {
        assert!(edge_wanted(true, false, true));
        assert!(!edge_wanted(true, true, true), "extended: the edge must be gone");
        assert!(!edge_wanted(false, false, true), "no link: no edge");
        assert!(!edge_wanted(false, true, true));
        assert!(!edge_wanted(true, false, false), "switched off: no edge");
    }

    /// Every reason a device's edge must stay down, each on its own.
    #[test]
    fn an_edge_needs_every_switch_on() {
        let mut s = devices(true, 2);
        assert!(s.wants_edge(0, false), "linked, on, under control");
        assert!(!s.wants_edge(0, true), "the server viewing this screen");
        s.extend = Some(0);
        assert!(!s.wants_edge(0, false), "showing the extended display");
        assert!(s.wants_edge(1, false), "the other device stays reachable");
        s.extend = None;
        s.viewing = Some(0);
        assert!(!s.wants_edge(0, false), "its screen is shown here");
        s.viewing = None;
        s.linked[0] = false;
        assert!(!s.wants_edge(0, false), "no link");
        s.linked[0] = true;
        s.enabled[0] = false;
        assert!(!s.wants_edge(0, false), "switched off");
        s.enabled[0] = true;
        s.control_on[0] = false;
        assert!(!s.wants_edge(0, false), "control not chosen");
        s.control_on[0] = true;
        s.held_off[0] = true;
        assert!(!s.wants_edge(0, false), "the client paused itself");
        s.held_off[0] = false;
        s.gone[0] = true;
        assert!(!s.wants_edge(0, false), "forgotten");
    }

    /// Without a capture backend nothing is armed, so `armed` never claims an
    /// edge the backend does not hold.
    #[tokio::test]
    async fn no_capture_arms_nothing() {
        let mut s = devices(true, 1);
        assert!(s.wants_edge(0, false));
        assert!(!s.sync_edge(0, false).await.unwrap());
        assert!(!s.armed[0]);
    }
}
