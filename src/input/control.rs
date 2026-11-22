//! Commands from outside the session: the menu bar, the Arrange and View
//! windows, pairing and the control socket.

use anyhow::Result;
use input_capture::CaptureHandle;
use tracing::{info, warn};

use super::{live, open_capture, open_emulation, release, Session};
use crate::proto::Msg;
use crate::{display, view, Peer, Side};

/// Asked for from outside the daemon — today, from the macOS menu bar, which
/// is the only thing that builds one.
#[derive(Debug)]
pub enum Control {
    /// Move a peer's screen to another edge, optionally of one display only.
    // Only the macOS Arrange window and menu bar build these two.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    SetPosition(String, Side, Option<usize>),
    /// Try the input backends again, after permission was granted.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    RetryBackends,
    /// Show the extended display on this device, or on none.
    Extend(Option<String>),
    /// Show this device's screen in a window here, or stop. Input reaches it
    /// through that window only while the user is in it (`view_input_mac.rs`).
    View(Option<String>),
    /// Switch one peer off (or back on). Off: the link is closed and the
    /// peer told why; nothing crosses until it is switched on again.
    SetEnabled(String, bool),
    /// Let the cursor cross into one peer (or stop it). Without it a switched
    /// on peer shares the clipboard only.
    Drive(String, bool),
    /// A device was just paired (or re-paired under its old name).
    AddPeer(Peer),
    /// Forget a device: off for good, and out of the config.
    Forget(String),
    /// From this machine's own bar: a client stops being driven (and tells the
    /// server); undone with `false`.
    Pause(bool),
    /// Bring input back to this machine now, wherever the cursor is.
    Release,
}

/// Switching a peer off while it holds the cursor must bring the cursor home
/// first: its edge is about to go, and nothing else would return it.
fn pause_takes_back(sending: Option<CaptureHandle>, handle: CaptureHandle, on: bool) -> bool {
    !on && sending == Some(handle)
}

/// Only one device is on at a time: switching one on switches every other
/// live, switched-on device off first. Returns (device, on) in the order to apply.
fn switches(enabled: &[bool], gone: &[bool], device: usize, on: bool) -> Vec<(usize, bool)> {
    let others = (0..enabled.len()).filter(|&j| on && j != device && enabled[j] && !gone[j]);
    others.map(|j| (j, false)).chain([(device, on)]).collect()
}

impl Session {
    pub(super) async fn on_control(&mut self, asked: Control) -> Result<()> {
        match asked {
            Control::SetPosition(name, side, on_display) => self.set_position(name, side, on_display).await,
            Control::RetryBackends => self.retry_backends().await,
            Control::Extend(target) => self.extend_to(target).await,
            Control::View(name) => self.view_screen(name).await,
            Control::SetEnabled(peer, on) => self.switch(peer, on).await,
            Control::Drive(peer, on) => self.drive(peer, on).await,
            Control::AddPeer(peer) => {
                self.add_peer(peer).await;
                Ok(())
            }
            Control::Forget(name) => self.forget(name).await,
            Control::Pause(paused) => {
                self.pause(paused).await;
                Ok(())
            }
            Control::Release => self.release_now().await,
        }
    }

    async fn extend_to(&mut self, target: Option<String>) -> Result<()> {
        let target = target.and_then(|name| live(&self.names, &self.gone, &name));
        if !self.server || target == self.extend {
            return Ok(());
        }
        self.extend = target;
        let name = target.map(|i| self.names[i].clone());
        info!(device = ?name, "extended display");
        if let Err(e) = crate::set_extend(&self.config_path, name.as_deref()) {
            warn!("cannot save the extended display: {e}");
        }
        self.status.set(|s| s.extending = name.clone().unwrap_or_default());
        let _ = self.display.send(display::Event::Target(target.map(|i| self.config.peers[i].clone())));
        // Extending to the device that has the cursor: bring it home first, or
        // it stays there with its edge gone.
        if let Some(handle) = self.handover.sending().filter(|h| Some(*h as usize) == self.extend) {
            self.links.send(&self.names[handle as usize], Msg::Leave);
            self.handover.take_back();
            release(self.capture.as_mut()).await?;
        }
        self.sync_edges().await?;
        self.publish();
        Ok(())
    }

    async fn view_screen(&mut self, name: Option<String>) -> Result<()> {
        if !self.server {
            return Ok(());
        }
        // One screen cannot both show the extended display and be shown here;
        // the menu stops Extend first.
        let viewable = |n: &str| live(&self.names, &self.gone, n).filter(|&i| self.linked[i] && self.extend != Some(i));
        let target = match name.as_deref().map(viewable) {
            Some(None) => {
                warn!(peer = ?name, "cannot view it: not linked, or showing the extended display");
                return Ok(());
            }
            target => target.flatten(),
        };
        if target == self.viewing {
            return Ok(());
        }
        // Its edge is about to go: input it holds comes home first.
        if let Some(handle) = self.handover.sending().filter(|&h| Some(h as usize) == target) {
            self.hand_back(handle as usize).await?;
        }
        self.viewing = target;
        let name = target.map(|i| self.names[i].clone());
        info!(device = ?name, "viewing a screen");
        self.status.set(|s| s.viewing = name.clone().unwrap_or_default());
        let _ = self.view.send(view::Event::Target(target.map(|i| self.config.peers[i].clone())));
        // The viewed device takes input through its window only, so its edge
        // goes, and the previous one's comes back.
        self.sync_edges().await?;
        self.publish();
        Ok(())
    }

    async fn add_peer(&mut self, peer: Peer) {
        match live(&self.names, &self.gone, &peer.name) {
            // Re-paired: new certificate and address, same place. Links
            // already closed the link under the old key.
            Some(i) => self.config.peers[i] = peer,
            None => {
                let i = self.names.len();
                self.names.push(peer.name.clone());
                self.gone.push(false);
                self.armed.push(false);
                self.linked.push(false);
                self.enabled.push(true);
                self.control_on.push(!self.server);
                self.held_off.push(false);
                if let Some(emulation) = self.emulation.as_mut() {
                    emulation.create(i as CaptureHandle).await;
                }
                info!(peer = %peer.name, side = ?peer.position, display = ?peer.display, "device added");
                self.config.peers.push(peer);
            }
        }
    }

    async fn forget(&mut self, name: String) -> Result<()> {
        let Some(i) = live(&self.names, &self.gone, &name) else { return Ok(()) };
        if self.handover.sending() == Some(i as CaptureHandle) {
            self.hand_back(i).await?;
        }
        self.gone[i] = true;
        self.enabled[i] = false;
        // Gone, so this only ever drops the edge.
        self.sync_edge(i, false).await?;
        if self.extend == Some(i) {
            self.extend = None;
            self.status.set(|s| s.extending.clear());
            let _ = self.display.send(display::Event::Target(None));
        }
        self.links.forget(&name);
        self.status.remove_peer(&name);
        if let Err(e) = crate::forget_peer(&self.config_path, &name) {
            warn!(peer = %name, "cannot remove it from the config: {e}");
        }
        info!(peer = %name, "device forgotten");
        self.publish();
        Ok(())
    }

    async fn release_now(&mut self) -> Result<()> {
        if let Some(handle) = self.handover.sending() {
            info!(peer = %self.names[handle as usize], "release asked for, taking input back");
            self.hand_back(handle as usize).await?;
        } else if let Some(handle) = self.handover.receiving() {
            // A client gives the cursor back the way its edge would.
            self.links.send(&self.names[handle as usize], Msg::Leave);
            self.lift_keys(handle).await;
            self.handover.take_back();
        }
        self.publish();
        Ok(())
    }

    /// Only a client has anything to refuse; the server's own switch is per
    /// client (SetEnabled).
    async fn pause(&mut self, paused: bool) {
        if self.server {
            return;
        }
        self.handover.driven = !paused;
        for i in 0..self.names.len() {
            if self.gone[i] {
                continue;
            }
            let name = &self.names[i];
            self.links.send(name, Msg::Paused(paused));
            self.status.set_peer(name, |p| p.paused = paused);
            if paused && self.handover.receiving() == Some(i as CaptureHandle) {
                self.lift_keys(i as CaptureHandle).await;
                self.handover.take_back();
            }
        }
        info!(paused, "this machine paused itself");
        self.publish();
    }

    async fn switch(&mut self, peer: String, on: bool) -> Result<()> {
        let Some(device) = live(&self.names, &self.gone, &peer).filter(|_| self.server) else { return Ok(()) };
        for (i, on) in switches(&self.enabled, &self.gone, device, on) {
            let peer = self.names[i].clone();
            if pause_takes_back(self.handover.sending(), i as CaptureHandle, on) {
                info!(%peer, "switched off while it had the cursor, taking input back");
                self.hand_back(i).await?;
            }
            self.enabled[i] = on;
            self.config.peers[i].enabled = on;
            self.links.set_off(&peer, !on);
            if on {
                // The dialer picks it up within a couple of seconds.
                self.status.set_peer(&peer, |p| p.mode = "offline".into());
            } else {
                // Closing the link drops its edge and pauses an extended
                // display, which resumes when it is back.
                self.links.disconnect(&peer, crate::link::OFF);
                self.status.set_peer(&peer, |p| p.mode = "off".into());
            }
            if let Err(e) = crate::set_enabled(&self.config_path, &peer, on) {
                warn!(%peer, "cannot save the switch: {e}");
            }
            self.sync_edge(i, false).await?;
            info!(%peer, on, "peer switched");
        }
        self.publish();
        Ok(())
    }

    async fn drive(&mut self, peer: String, on: bool) -> Result<()> {
        let Some(i) = live(&self.names, &self.gone, &peer).filter(|_| self.server) else { return Ok(()) };
        if pause_takes_back(self.handover.sending(), i as CaptureHandle, on) {
            info!(%peer, "control turned off while it had the cursor, taking input back");
            self.hand_back(i).await?;
        }
        self.control_on[i] = on;
        self.config.peers[i].control = on;
        self.status.set_peer(&peer, |p| p.control = on);
        if let Err(e) = crate::update_peer(&self.config_path, &peer, |p| p.control = on) {
            warn!(%peer, "cannot save control: {e}");
        }
        self.sync_edge(i, false).await?;
        info!(%peer, on, "control switched");
        self.publish();
        Ok(())
    }

    /// Permission may have just been granted, so build whichever backend this
    /// machine's role needs.
    async fn retry_backends(&mut self) -> Result<()> {
        if self.capture.is_none() {
            self.capture = open_capture().await;
            if self.capture.is_some() {
                // No edge is ever armed without a capture behind it, so none
                // is yet: arm every one the rules want, for peers already linked.
                self.sync_edges().await?;
                info!("input capture is working now");
            }
        }
        if !self.server && self.emulation.is_none() {
            self.emulation = open_emulation().await;
            if let Some(emulation) = self.emulation.as_mut() {
                for i in 0..self.config.peers.len() {
                    emulation.create(i as CaptureHandle).await;
                }
                info!("input emulation is working now");
            }
        }
        self.degraded = self.capture.is_none() || (!self.server && self.emulation.is_none());
        self.publish();
        Ok(())
    }

    async fn set_position(&mut self, name: String, side: Side, on_display: Option<usize>) -> Result<()> {
        let Some(i) = live(&self.names, &self.gone, &name) else { return Ok(()) };
        let (handle, peer) = (i as CaptureHandle, &self.names[i]);
        // Kept current, so re-arming later puts the edge where it is now
        // rather than where it was at startup.
        self.config.peers[i].position = side;
        self.config.peers[i].display = on_display;
        // Re-arm the edge: the capture backend keys a grab to a screen side,
        // so moving the peer means dropping it and taking a new one.
        if let Some(capture) = self.capture.as_mut().filter(|_| self.armed[i]) {
            capture.destroy(handle).await?;
            capture.restrict_to_display(on_display.map(|d| d.saturating_sub(1))).await;
            capture.create(handle, side.into()).await?;
            info!(%peer, ?side, display = ?on_display, "screen edge moved");
        }
        // Tell the peer which of its own edges now faces us, so both ends
        // agree even if the roles are ever swapped.
        self.links.send(peer, Msg::Arrange(side.opposite()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::tests::devices;

    #[test]
    fn switching_one_device_on_switches_the_others_off() {
        let enabled = [true, true, false, true];
        let gone = [false, false, false, true];
        assert_eq!(
            switches(&enabled, &gone, 2, true),
            vec![(0, false), (1, false), (2, true)]
        );
        assert_eq!(
            switches(&enabled, &gone, 0, true),
            vec![(1, false), (0, true)]
        );
        assert_eq!(switches(&enabled, &gone, 1, false), vec![(1, false)]);
    }

    #[test]
    fn pausing_the_peer_with_the_cursor_takes_it_back() {
        assert!(pause_takes_back(Some(1), 1, false), "off while it has the cursor: bring it home");
        assert!(!pause_takes_back(Some(0), 1, false), "another peer has it: leave it");
        assert!(!pause_takes_back(None, 1, false), "nobody has it: nothing to take");
        assert!(!pause_takes_back(Some(1), 1, true), "switching on never takes anything");
    }

    /// The menu's switch, end to end: one device on turns the others off, and
    /// takes their edges with them.
    #[tokio::test]
    async fn switching_a_device_on_from_the_menu_turns_the_rest_off() {
        let mut s = devices(true, 3);
        s.on_control(Control::SetEnabled("peer1".into(), true)).await.unwrap();
        assert_eq!(s.enabled, [false, true, false]);
        assert_eq!(s.config.peers.iter().map(|p| p.enabled).collect::<Vec<_>>(), [false, true, false]);
        assert!(!s.wants_edge(0, false) && s.wants_edge(1, false) && !s.wants_edge(2, false));
    }

    /// A client has nothing to switch: the command is ignored there.
    #[tokio::test]
    async fn a_client_ignores_the_servers_switches() {
        let mut s = devices(false, 2);
        s.on_control(Control::SetEnabled("peer1".into(), true)).await.unwrap();
        s.on_control(Control::Drive("peer0".into(), false)).await.unwrap();
        assert_eq!(s.enabled, [true, true]);
        assert_eq!(s.control_on, [true, true]);
    }
}
