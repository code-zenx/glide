//! Events from this machine's own capture: an armed edge crossed, input to
//! forward to the peer being driven, and the chords that take input back.

use std::sync::Arc;

use anyhow::Result;
use input_capture::{CaptureError, CaptureEvent, CaptureHandle};
use input_event::scancode::Linux as Key;
use tracing::{info, warn};

use super::{release, Session};
use crate::link::Links;
use crate::proto::Msg;

/// Held together, these hand input back to this machine at once, even if the
/// peer never answers again — the way out of a wedged remote session. The
/// everyday toggle is Ctrl+Option+Cmd, watched in the capture backend.
const RELEASE_CHORD: [Key; 4] = [
    Key::KeyLeftCtrl,
    Key::KeyLeftShift,
    Key::KeyLeftAlt,
    Key::KeyLeftMeta,
];

/// The chord spelled out, logged the moment the cursor leaves — the one time
/// someone might need it is the one time they cannot look it up.
const RELEASE_HINT: &str = "Ctrl+Option+Cmd";

/// What crossing an armed edge means. Decided apart from the backends, because
/// getting it wrong strands the cursor and the keyboard with it.
#[derive(PartialEq, Debug)]
enum Crossing {
    /// Take the cursor and start feeding the peer.
    Send,
    /// Give the cursor back to the server. All a client's edge ever means.
    HandBack,
    /// Let go. Holding the cursor with nowhere to send it swallows every key
    /// and leaves no way to reach the menu bar and quit.
    Stay,
}

fn crossing(server: bool, linked: bool) -> Crossing {
    match (server, linked) {
        (false, _) => Crossing::HandBack,
        (true, true) => Crossing::Send,
        (true, false) => Crossing::Stay,
    }
}

impl Session {
    // Every captured motion event comes through here, so nothing on this path
    // may allocate: names are borrowed, never cloned.
    pub(super) async fn on_capture(&mut self, captured: Result<(CaptureHandle, CaptureEvent), CaptureError>) -> Result<()> {
        let (handle, event) = match captured {
            Ok(captured) => captured,
            // The event tap dies for reasons outside this process: a password
            // field turning on secure input, a revoked grant. Put input back on
            // this machine rather than leaving the cursor believed to be
            // somewhere else.
            Err(e) => {
                warn!("capture error, input is local again: {e}");
                self.handover.take_back();
                self.publish();
                return Ok(());
            }
        };
        let Some(peer) = self.names.get(handle as usize) else { return Ok(()) };
        match event {
            CaptureEvent::Begin => self.cross(handle).await?,
            CaptureEvent::Input(event) => {
                if self.server && self.handover.forwarding(handle) {
                    self.links.send(peer, Msg::Input(event));
                }
            }
            CaptureEvent::Toggle => {
                if self.server && self.handover.forwarding(handle) {
                    info!(%peer, "toggle chord pressed, taking input back");
                    self.hand_back(handle as usize).await?;
                }
            }
        }
        let chord = self.server && self.capture.as_ref().is_some_and(|c| c.keys_pressed(&RELEASE_CHORD));
        if chord {
            info!("release chord pressed, taking input back");
            self.hand_back(handle as usize).await?;
        }
        self.publish();
        Ok(())
    }

    /// The cursor reached the armed edge toward `handle`.
    async fn cross(&mut self, handle: CaptureHandle) -> Result<()> {
        let peer = &self.names[handle as usize];
        match crossing(self.server, self.links.connected(peer)) {
            // Says "you have it back" and nothing else: no key and no motion
            // ever crosses this way.
            Crossing::HandBack => {
                info!(%peer, "cursor reached this edge, handing control back");
                self.links.send(peer, Msg::Leave);
                self.lift_keys(handle).await;
                self.handover.handed_back();
                release(self.capture.as_mut()).await?;
            }
            Crossing::Stay => {
                warn!(%peer, "cursor left this screen but the peer is not connected, keeping input here");
                release(self.capture.as_mut()).await?;
            }
            Crossing::Send => {
                info!(%peer, "cursor left this screen; {RELEASE_HINT} takes it back");
                self.handover.began(handle);
                self.links.send(peer, Msg::Enter);
                push_clipboard(&self.links, peer);
            }
        }
        Ok(())
    }
}

/// Send this machine's clipboard to the peer taking over. Off the hot path:
/// reading the clipboard can block on the compositor, and motion must not wait.
fn push_clipboard(links: &Arc<Links>, peer: &str) {
    let (links, peer) = (links.clone(), peer.to_string());
    tokio::spawn(async move {
        if let Some(clip) = crate::clipboard::read().await {
            links.send(&peer, Msg::Clipboard(clip));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Crossing an edge must never strand the cursor. Two ways it used to:
    /// a server with no peer grabbed it and forwarded into nothing, and a
    /// client had no edge to hand it back with, so only the chord got it out.
    #[test]
    fn an_edge_never_traps_the_cursor() {
        assert_eq!(crossing(true, true), Crossing::Send);
        assert_eq!(
            crossing(true, false),
            Crossing::Stay,
            "a server with nowhere to send must keep input on this machine"
        );
        assert_eq!(
            crossing(false, true),
            Crossing::HandBack,
            "a client's edge is the way back, never a way to send"
        );
        assert_eq!(
            crossing(false, false),
            Crossing::HandBack,
            "and it stays the way back even with the link down"
        );
    }
}
