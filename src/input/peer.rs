//! Peers: their links coming and going, and what they send.

use anyhow::Result;
use input_capture::CaptureHandle;
use input_event::Event;
use tracing::{debug, info, warn};

use super::handover::Cleanup;
use super::{live, release, Session};
use crate::proto::Msg;
use crate::{display, view, Side};

/// Whether this machine can show a server's extended display: the receiving
/// end drives Hyprland, so an X11 desktop cannot.
fn can_extend_here() -> bool {
    cfg!(not(target_os = "macos")) && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

impl Session {
    pub(super) async fn on_link(&mut self, peer: String, up: bool) -> Result<()> {
        let _ = self.display.send(display::Event::Link(peer.clone(), up));
        let _ = self.view.send(view::Event::Link(peer.clone(), up));
        let Some(i) = live(&self.names, &self.gone, &peer) else { return Ok(()) };
        self.linked[i] = up;
        if up && !self.server {
            self.links.send(&peer, Msg::Caps { extend: can_extend_here(), view: view::can_share() });
        }
        // A client that was offline when it was moved in the Arrange window
        // still has the old side; say it again on every link.
        if up && self.server {
            self.links.send(&peer, Msg::Arrange(self.config.peers[i].position.opposite()));
        }
        if !up {
            self.held_off[i] = false;
        }
        if !self.sync_edge(i, false).await? {
            return Ok(());
        }
        let at = &self.config.peers[i];
        if self.armed[i] {
            let purpose = if self.server { "to send from" } else { "to hand control back" };
            info!(%peer, side = ?at.position, display = ?at.display, "screen edge armed {purpose}");
        } else {
            info!(%peer, "link down, screen edge dropped");
        }
        Ok(())
    }

    pub(super) async fn on_message(&mut self, peer: String, msg: Msg) -> Result<()> {
        let Some(handle) = live(&self.names, &self.gone, &peer).map(|i| i as CaptureHandle) else { return Ok(()) };
        match msg {
            Msg::Enter => {
                if self.handover.peer_entered(handle) {
                    release(self.capture.as_mut()).await?;
                }
                info!(%peer, "peer took the cursor");
            }
            Msg::Leave => self.on_leave(handle, &peer).await?,
            Msg::Input(event) => self.type_input(handle, &peer, event).await,
            Msg::Clipboard(clip) => {
                info!(%peer, "clipboard arrived");
                tokio::spawn(crate::clipboard::write(clip));
            }
            // Sent on every link, so most of the time nothing moved.
            Msg::Arrange(side) if self.config.peers[handle as usize].position == side => {}
            Msg::Arrange(side) => self.peer_moved(handle, &peer, side).await?,
            Msg::Screens(screens) => {
                info!(%peer, count = screens.len(), "peer described its screens");
                self.status.set_screens(&peer, screens);
            }
            Msg::Paused(paused) => self.peer_paused(handle, &peer, paused).await?,
            Msg::Caps { extend: can, view: can_view } => {
                info!(%peer, can_extend = can, can_view, "peer described what it can do");
                self.status.set_peer(&peer, |p| {
                    p.can_extend = can;
                    p.can_view = can_view;
                });
            }
            Msg::Display(_) | Msg::DisplayReady(_) => {
                let _ = self.display.send(display::Event::Msg(peer, msg));
            }
            Msg::View(on) if !self.server => {
                self.sync_edge(handle as usize, on).await?;
                let _ = self.view.send(view::Event::Msg(peer, msg));
            }
            Msg::View(_) | Msg::ViewReady(_) => {
                let _ = self.view.send(view::Event::Msg(peer, msg));
            }
            Msg::Ping(_) | Msg::Pong(_) => {}
        }
        self.publish();
        Ok(())
    }

    async fn on_leave(&mut self, handle: CaptureHandle, peer: &str) -> Result<()> {
        match self.handover.peer_left(handle) {
            Cleanup::ReleaseKeys => {
                // Never leave a modifier held down here because the link
                // dropped mid-chord.
                self.lift_keys(handle).await;
                info!(%peer, "input is local again");
            }
            Cleanup::ReleaseCapture => {
                release(self.capture.as_mut()).await?;
                warn!(%peer, "peer went away while it had the cursor, input is local again");
            }
            Cleanup::Nothing => {}
        }
        Ok(())
    }

    /// On a server `peer_input` is always false, so a client that sends input
    /// anyway is ignored rather than obeyed.
    async fn type_input(&mut self, handle: CaptureHandle, peer: &str, event: Event) {
        if !self.handover.peer_input(handle) {
            if self.server {
                debug!(%peer, "ignoring input from a client");
            }
            return;
        }
        let Some(emulation) = self.emulation.as_mut() else {
            warn!(%peer, "input arrived but this machine cannot type");
            return;
        };
        #[cfg(not(target_os = "macos"))]
        let event = stamp_now(event);
        if let Err(e) = emulation.consume(event, handle).await {
            warn!(%peer, "cannot emulate: {e}");
        }
    }

    async fn peer_moved(&mut self, handle: CaptureHandle, peer: &str, side: Side) -> Result<()> {
        info!(%peer, ?side, "peer says it sits on our {side:?}");
        if let Err(e) = crate::set_peer_side(&self.config_path, peer, side) {
            warn!(%peer, "cannot save the arrangement: {e}");
        }
        let i = handle as usize;
        self.config.peers[i].position = side;
        if let Some(capture) = self.capture.as_mut().filter(|_| self.armed[i]) {
            capture.destroy(handle).await?;
            capture.create(handle, side.into()).await?;
        }
        Ok(())
    }

    /// From the server: it switched this machine off or on; only shown, the
    /// server already stopped sending. From a client: it paused itself, so its
    /// edge goes and the cursor comes home.
    async fn peer_paused(&mut self, handle: CaptureHandle, peer: &str, paused: bool) -> Result<()> {
        if !self.server {
            self.status.set_peer(peer, |p| p.paused = paused);
            info!(%peer, paused, "server switched this machine");
            return Ok(());
        }
        let i = handle as usize;
        if paused && self.handover.sending() == Some(handle) {
            self.hand_back(i).await?;
        }
        self.held_off[i] = paused;
        self.status.set_peer(peer, |p| p.paused = paused);
        self.sync_edge(i, false).await?;
        info!(%peer, paused, "client paused itself");
        Ok(())
    }
}

/// Stamp an arriving event with this machine's clock before it is typed.
///
/// The Mac capture sends every event with `time: 0`, and the wlroots backend
/// hands that straight to the compositor. Chromium ignores a dialog click that
/// looks older than the dialog (its guard against accidental clicks), so Edge's
/// "Add extension" button never worked through glide. Wayland times are
/// `CLOCK_MONOTONIC` in milliseconds, wrapping at `u32`.
#[cfg(not(target_os = "macos"))]
fn stamp_now(mut event: Event) -> Event {
    use input_event::{KeyboardEvent, PointerEvent};
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let now = (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32;
    match &mut event {
        Event::Pointer(
            PointerEvent::Motion { time, .. }
            | PointerEvent::Button { time, .. }
            | PointerEvent::Axis { time, .. },
        )
        | Event::Keyboard(KeyboardEvent::Key { time, .. }) => *time = now,
        _ => {}
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::tests::devices;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn arriving_events_get_a_real_time() {
        use input_event::PointerEvent;
        let click = Event::Pointer(PointerEvent::Button { time: 0, button: 0x110, state: 1 });
        let Event::Pointer(PointerEvent::Button { time, .. }) = stamp_now(click) else { unreachable!() };
        assert_ne!(time, 0);
    }

    /// A client that pauses itself takes its edge off the server until it
    /// resumes, or its link drops and comes back.
    #[tokio::test]
    async fn a_paused_client_has_no_edge_on_the_server() {
        let mut s = devices(true, 2);
        s.on_message("peer0".into(), Msg::Paused(true)).await.unwrap();
        assert!(!s.wants_edge(0, false), "paused: no edge");
        assert!(s.wants_edge(1, false), "the other client is untouched");
        s.on_message("peer0".into(), Msg::Paused(false)).await.unwrap();
        assert!(s.wants_edge(0, false), "resumed");
        s.on_message("peer0".into(), Msg::Paused(true)).await.unwrap();
        s.on_link("peer0".into(), false).await.unwrap();
        s.on_link("peer0".into(), true).await.unwrap();
        assert!(s.wants_edge(0, false), "a new link forgets the pause");
    }
}
