//! The rule for who owns input, kept apart from the backends so it can be
//! tested without a compositor.

use std::time::{Duration, Instant};

use input_capture::CaptureHandle;

/// Far longer than input takes to arrive from a peer on the same network.
const HAND_BACK_SETTLE: Duration = Duration::from_millis(200);

#[derive(PartialEq, Debug, Clone, Copy, Default)]
enum Mode {
    /// Input belongs to this machine.
    #[default]
    Local,
    /// This machine has the cursor and is feeding the peer at this handle.
    Sending(CaptureHandle),
    /// The peer at this handle has the cursor and we type what it sends.
    Receiving(CaptureHandle),
}

/// What has to be undone when control moves away from us.
#[derive(PartialEq, Debug)]
pub(super) enum Cleanup {
    Nothing,
    /// Stop grabbing local input.
    ReleaseCapture,
    /// Lift any key the peer left held down here.
    ReleaseKeys,
}

pub(super) struct Handover {
    mode: Mode,
    /// Whether a peer is allowed to drive this machine at all. False on a
    /// server: its keyboard and mouse are the ones being shared, and nothing
    /// arriving over the link may type here.
    pub(super) driven: bool,
    /// After this machine hands control back, input still in flight from the
    /// peer must not read as the peer taking it again.
    settling_until: Option<Instant>,
}

impl Handover {
    pub(super) fn new(driven: bool) -> Self {
        Self { mode: Mode::Local, driven, settling_until: None }
    }

    pub(super) fn name(&self) -> &'static str {
        match self.mode {
            Mode::Local => "local",
            Mode::Sending(_) => "sending",
            Mode::Receiving(_) => "receiving",
        }
    }

    /// The peer input is flowing to or from, if any.
    pub(super) fn peer(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Local => None,
            Mode::Sending(handle) | Mode::Receiving(handle) => Some(handle),
        }
    }

    /// Our own capture grabbed the cursor at this edge.
    pub(super) fn began(&mut self, handle: CaptureHandle) {
        self.mode = Mode::Sending(handle);
    }

    /// The peer driving this machine, if any.
    pub(super) fn receiving(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Receiving(handle) => Some(handle),
            _ => None,
        }
    }

    /// The peer this machine is driving, if any.
    pub(super) fn sending(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Sending(handle) => Some(handle),
            _ => None,
        }
    }

    pub(super) fn forwarding(&self, handle: CaptureHandle) -> bool {
        self.mode == Mode::Sending(handle)
    }

    /// The peer says it has the cursor. True when our capture must let go.
    pub(super) fn peer_entered(&mut self, handle: CaptureHandle) -> bool {
        if !self.driven {
            return false;
        }
        let was_sending = matches!(self.mode, Mode::Sending(_));
        self.mode = Mode::Receiving(handle);
        self.settling_until = None;
        was_sending
    }

    /// The peer handed control back, or its link died.
    pub(super) fn peer_left(&mut self, handle: CaptureHandle) -> Cleanup {
        match self.mode {
            Mode::Receiving(h) if h == handle => {
                self.mode = Mode::Local;
                Cleanup::ReleaseKeys
            }
            // The link dropped while we were driving that peer: our capture is
            // still holding the cursor hostage with nowhere to send it.
            Mode::Sending(h) if h == handle => {
                self.mode = Mode::Local;
                Cleanup::ReleaseCapture
            }
            _ => Cleanup::Nothing,
        }
    }

    /// Input arrived from a peer. True when it should be typed here.
    ///
    /// Motion rides unreliable datagrams and can overtake the `Enter` that
    /// announced it, so input from a peer while we are idle is taken as the
    /// announcement — only a pinned peer can send it at all.
    pub(super) fn peer_input(&mut self, handle: CaptureHandle) -> bool {
        if !self.driven {
            return false;
        }
        match self.mode {
            Mode::Receiving(h) => h == handle,
            Mode::Local if self.settling_until.is_some_and(|until| Instant::now() < until) => false,
            Mode::Local => {
                self.mode = Mode::Receiving(handle);
                true
            }
            Mode::Sending(_) => false,
        }
    }

    /// The release chord: take input back whatever the peer thinks.
    pub(super) fn take_back(&mut self) {
        self.mode = Mode::Local;
    }

    /// This client's edge gave the cursor back to the server.
    pub(super) fn handed_back(&mut self) {
        self.mode = Mode::Local;
        self.settling_until = Some(Instant::now() + HAND_BACK_SETTLE);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn input_in_flight_after_a_hand_back_does_not_take_the_cursor_again() {
        let mut handover = Handover::new(true);
        handover.peer_entered(0);
        handover.handed_back();
        assert_eq!(handover.name(), "local");
        assert!(!handover.peer_input(0));
        assert_eq!(handover.name(), "local");
        assert!(!handover.peer_entered(0));
        assert!(handover.peer_input(0));
        assert_eq!(handover.name(), "receiving");
    }

    use super::*;

    /// A client: driven by the server, shares nothing of its own.
    fn client() -> Handover {
        Handover::new(true)
    }

    #[test]
    fn control_follows_the_cursor() {
        let mut h = client();
        assert_eq!(h.name(), "local");

        h.began(0);
        assert!(h.forwarding(0), "our own edge should send");
        assert!(!h.forwarding(1), "only the peer whose edge was crossed");
        assert!(!h.peer_input(0), "a peer cannot type here while we drive it");

        assert!(h.peer_entered(0), "capture must be released when the peer takes over");
        assert_eq!(h.name(), "receiving");
        assert!(h.peer_input(0));
        assert!(!h.forwarding(0));
    }

    #[test]
    fn motion_that_overtakes_its_announcement_still_lands() {
        let mut h = client();
        assert!(h.peer_input(0), "input while idle is taken as the announcement");
        assert_eq!(h.name(), "receiving");
    }

    #[test]
    fn a_dead_link_never_leaves_input_stuck() {
        let mut h = client();
        h.began(0);
        assert_eq!(h.peer_left(0), Cleanup::ReleaseCapture, "we were driving a peer that vanished");
        assert_eq!(h.name(), "local");

        h.peer_entered(0);
        assert_eq!(h.peer_left(0), Cleanup::ReleaseKeys, "the peer was typing here");
        assert_eq!(h.name(), "local");

        assert_eq!(h.peer_left(1), Cleanup::Nothing, "an idle peer leaving changes nothing");
    }

    #[test]
    fn the_release_chord_wins() {
        let mut h = client();
        h.began(0);
        h.take_back();
        assert_eq!(h.name(), "local");
        assert!(!h.forwarding(0));
    }

    #[test]
    fn extending_knows_who_has_the_cursor() {
        let mut h = Handover::new(false);
        assert_eq!(h.sending(), None);
        h.began(3);
        assert_eq!(h.sending(), Some(3));
        h.take_back();
        assert_eq!(h.sending(), None);
    }

    /// The whole point of the roles: a server's keyboard and mouse are shared
    /// outward, and nothing on the wire can take them over.
    #[test]
    fn a_server_is_never_driven_by_its_peer() {
        let mut h = Handover::new(false);
        assert!(!h.peer_input(0), "a client may not type on the server");
        assert_eq!(h.name(), "local", "and may not put it into receiving either");
        assert!(!h.peer_entered(0), "nor claim the cursor");
        assert_eq!(h.name(), "local");

        // Sending still works, and is still given up cleanly.
        h.began(0);
        assert!(h.forwarding(0));
        assert!(!h.peer_input(0));
        assert_eq!(h.peer_left(0), Cleanup::ReleaseCapture);
        assert_eq!(h.name(), "local");
    }
}
