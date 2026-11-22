//! Who owns the keyboard and mouse right now, and where their events go.
//!
//! Input flows one way. The **server** is the machine whose keyboard and mouse
//! are shared: it watches its own screen edges, and when the cursor leaves one
//! it starts sending. The **client** only types what arrives, and builds no
//! capture of its own to send from.
//!
//! A client does arm one screen edge, but only to hand control back. Crossing
//! it sends a single `Leave` and nothing else — no key, no motion — because
//! otherwise there is no way back: the server's capture holds the cursor until
//! something tells it to let go, and a client that shares no input has nothing
//! else to say.
//!
//! The session reacts to this machine's own capture (`capture`), to commands
//! from the menu bar and the control socket (`control`), and to peers' links
//! and messages (`peer`). Who owns input is decided apart, in `handover`, and
//! which screen edges are armed in `edge`.

mod capture;
mod control;
mod edge;
mod handover;
mod peer;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use input_capture::{CaptureError, CaptureEvent, CaptureHandle, InputCapture, Position};
use input_emulation::InputEmulation;
use input_event::{Event, KeyboardEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

pub use control::Control;
use handover::Handover;

use crate::display;
use crate::link::Links;
use crate::proto::Msg;
use crate::status::Status;
use crate::view;
use crate::{Config, Role, Side};

impl From<Side> for Position {
    fn from(side: Side) -> Self {
        match side {
            Side::Left => Position::Left,
            Side::Right => Position::Right,
            Side::Top => Position::Top,
            Side::Bottom => Position::Bottom,
        }
    }
}

/// The handle of a device that has not been forgotten.
fn live(names: &[String], gone: &[bool], name: &str) -> Option<usize> {
    names.iter().enumerate().position(|(i, n)| n == name && !gone[i])
}

/// Everything the session keeps between events.
struct Session {
    config: Config,
    config_path: PathBuf,
    links: Arc<Links>,
    status: Arc<Status>,
    display: UnboundedSender<display::Event>,
    view: UnboundedSender<view::Event>,
    server: bool,
    // Handles are indices into these lists, so a forgotten device keeps its
    // slot (marked gone) until the next start: every other device's handle,
    // and the edge the capture backend keyed to it, stays put.
    names: Vec<String>,
    gone: Vec<bool>,
    /// Edges are armed when the link comes up, not at start. An edge with no
    /// link behind it grabs the cursor and feeds it to nobody, and with the
    /// peer above this screen that is every trip to the menu bar.
    armed: Vec<bool>,
    linked: Vec<bool>,
    enabled: Vec<bool>,
    control_on: Vec<bool>,
    /// A client that paused itself: like switched off, but its choice, not saved here.
    held_off: Vec<bool>,
    /// The device showing the extended display. Only a server has one to extend.
    extend: Option<usize>,
    /// The device whose screen is shown here. Server only. Its edge stays
    /// disarmed meanwhile, like an extended device's.
    viewing: Option<usize>,
    capture: Option<InputCapture>,
    emulation: Option<InputEmulation>,
    /// A backend this machine's role needs is missing.
    degraded: bool,
    handover: Handover,
    /// What the status last showed; see `publish`.
    shown: (&'static str, Option<CaptureHandle>, bool),
}

#[allow(clippy::too_many_arguments)]
pub async fn session(
    config: Config,
    config_path: PathBuf,
    links: Arc<Links>,
    status: Arc<Status>,
    from_links: UnboundedReceiver<(String, Msg)>,
    link_state: UnboundedReceiver<(String, bool)>,
    control: UnboundedReceiver<Control>,
    display: UnboundedSender<display::Event>,
    view: UnboundedSender<view::Event>,
) -> Result<()> {
    let server = config.role == Role::Server;
    // Both ends watch an edge — the server to take the cursor, the client to
    // give it back — but only a client can be typed on from the wire.
    let capture = open_capture().await;
    let emulation = if server { None } else { open_emulation().await };
    let degraded = capture.is_none() || (!server && emulation.is_none());
    let names: Vec<String> = config.peers.iter().map(|p| p.name.clone()).collect();
    let count = names.len();
    let extend = if server {
        crate::extend_target(&config).and_then(|t| names.iter().position(|n| *n == t.name))
    } else {
        None
    };
    let session = Session {
        enabled: config.peers.iter().map(|p| !server || p.enabled).collect(),
        control_on: config.peers.iter().map(|p| !server || p.control).collect(),
        gone: vec![false; count],
        armed: vec![false; count],
        linked: vec![false; count],
        held_off: vec![false; count],
        viewing: None,
        handover: Handover::new(!server),
        shown: ("", None, !degraded),
        config, config_path, links, status, display, view, server, names, extend, capture, emulation, degraded,
    };
    session.run(from_links, link_state, control).await
}

impl Session {
    /// Say what works, then hand each event to the job it belongs to until a
    /// source runs dry.
    async fn run(
        mut self,
        mut from_links: UnboundedReceiver<(String, Msg)>,
        mut link_state: UnboundedReceiver<(String, bool)>,
        mut control: UnboundedReceiver<Control>,
    ) -> Result<()> {
        self.start().await;
        loop {
            self.drop_lost_view();
            tokio::select! {
                captured = next_capture(self.capture.as_mut()) => {
                    let Some(captured) = captured else {
                        warn!("capture ended");
                        break;
                    };
                    self.on_capture(captured).await?;
                },
                change = link_state.recv() => {
                    let Some((peer, up)) = change else { break };
                    self.on_link(peer, up).await?;
                },
                asked = control.recv() => {
                    let Some(asked) = asked else { break };
                    self.on_control(asked).await?;
                },
                incoming = from_links.recv() => {
                    let Some((peer, msg)) = incoming else { break };
                    self.on_message(peer, msg).await?;
                }
            }
        }
        if let Some(emulation) = self.emulation.as_mut() {
            emulation.terminate().await;
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.terminate().await?;
        }
        Ok(())
    }

    async fn start(&mut self) {
        self.status.allowed(!self.degraded);
        for (peer, &on) in self.config.peers.iter().zip(&self.control_on) {
            self.status.set_peer(&peer.name, |p| p.control = on);
        }
        if let Some(emulation) = self.emulation.as_mut() {
            for i in 0..self.config.peers.len() {
                emulation.create(i as CaptureHandle).await;
            }
        }
        let (flow, degraded) = (self.handover.name(), self.degraded);
        self.status.set(|s| {
            s.flow = flow.into();
            s.active.clear();
            s.denied = degraded;
        });
    }

    /// A view outlives input coming home, but not its device: unlinked,
    /// switched off, forgotten (which switches it off) or made the extended
    /// display. Checked before every event because most handlers return early.
    fn drop_lost_view(&mut self) {
        if let Some(i) = self.viewing.filter(|&i| !self.linked[i] || !self.enabled[i] || self.extend == Some(i)) {
            info!(peer = %self.names[i], "device unavailable, view stopped");
            self.viewing = None;
            self.status.set(|s| s.viewing.clear());
            let _ = self.view.send(view::Event::Target(None));
        }
    }

    /// Show who has input. This runs on every captured motion event, so it
    /// must touch nothing that allocates unless something actually changed:
    /// `Status::set` clones the whole state to compare it, and at a thousand
    /// events a second that is all wasted work on the path that has to stay quick.
    fn publish(&mut self) {
        let now = (self.handover.name(), self.handover.peer(), !self.degraded);
        if self.shown != now {
            self.shown = now;
            let (names, degraded) = (&self.names, self.degraded);
            self.status.set(|s| {
                s.flow = now.0.into();
                s.active = now.1.and_then(|h| names.get(h as usize)).cloned().unwrap_or_default();
                s.denied = degraded;
            });
        }
    }

    /// Bring input home from device `i`: the peer saw these keys go down but
    /// will never see them come up, so say so before saying goodbye.
    async fn hand_back(&mut self, i: usize) -> Result<()> {
        let peer = &self.names[i];
        let held = self.capture.as_mut().map(|c| c.take_pressed_keys()).unwrap_or_default();
        for key in held {
            self.links.send(peer, Msg::Input(Event::Keyboard(KeyboardEvent::Key { time: 0, key: key as u32, state: 0 })));
        }
        self.links.send(peer, Msg::Leave);
        self.handover.take_back();
        release(self.capture.as_mut()).await
    }

    /// Lift any key the peer left held down here.
    async fn lift_keys(&mut self, handle: CaptureHandle) {
        if let Some(emulation) = self.emulation.as_mut() {
            let _ = emulation.release_keys(handle).await;
        }
    }
}

/// A backend the platform refuses — on macOS, input permission not granted yet
/// — must not take the whole daemon down with it. The link and the clipboard
/// still work, and the status says what is missing.
async fn open_capture() -> Option<InputCapture> {
    match InputCapture::new(None).await {
        Ok(capture) => Some(capture),
        Err(e) => {
            warn!("cannot capture input on this machine: {e}");
            None
        }
    }
}

async fn open_emulation() -> Option<InputEmulation> {
    match InputEmulation::new(None).await {
        Ok(emulation) => Some(emulation),
        Err(e) => {
            warn!("cannot type on this machine: {e}");
            None
        }
    }
}

/// The capture stream, or a future that never finishes when there is no
/// capture to listen to — which is every client.
async fn next_capture(
    capture: Option<&mut InputCapture>,
) -> Option<Result<(CaptureHandle, CaptureEvent), CaptureError>> {
    match capture {
        Some(capture) => capture.next().await,
        None => std::future::pending().await,
    }
}

async fn release(capture: Option<&mut InputCapture>) -> Result<()> {
    if let Some(capture) = capture {
        capture.release().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Peer;

    /// A session with `count` devices, every one linked, switched on and under
    /// control, and no input backends, as before permission is granted.
    pub(super) fn devices(server: bool, count: usize) -> Session {
        let peer = |i: usize| Peer {
            name: format!("peer{i}"),
            addrs: vec![],
            fingerprint: String::new(),
            position: Side::Right,
            display: None,
            enabled: true,
            control: true,
        };
        let peers: Vec<Peer> = (0..count).map(peer).collect();
        let role = if server { Role::Server } else { Role::Client };
        let status = std::env::temp_dir().join(format!("glide-input-test-{}-{server}-{count}", std::process::id()));
        Session {
            names: peers.iter().map(|p| p.name.clone()).collect(),
            gone: vec![false; count],
            armed: vec![false; count],
            linked: vec![true; count],
            enabled: vec![true; count],
            control_on: vec![true; count],
            held_off: vec![false; count],
            extend: None,
            viewing: None,
            capture: None,
            emulation: None,
            degraded: true,
            handover: Handover::new(!server),
            shown: ("", None, false),
            links: Arc::new(Links::new(peers.clone(), server)),
            status: Arc::new(Status::at(status)),
            display: tokio::sync::mpsc::unbounded_channel().0,
            view: tokio::sync::mpsc::unbounded_channel().0,
            config: Config { name: "here".into(), role, listen: crate::default_listen(), display: Default::default(), peers },
            config_path: PathBuf::new(),
            server,
        }
    }

    /// Forgetting a device must not shift anyone else's handle: the capture
    /// backend keyed each edge to one.
    #[test]
    fn a_forgotten_device_keeps_its_slot() {
        let names: Vec<String> = ["anorak", "nullbyte", "anton"].map(String::from).to_vec();
        let gone = [false, true, false];
        assert_eq!(live(&names, &gone, "anton"), Some(2), "anton's handle is unchanged");
        assert_eq!(live(&names, &gone, "nullbyte"), None, "the forgotten one is gone");
        let names: Vec<String> = ["anorak", "nullbyte", "anton", "nullbyte"].map(String::from).to_vec();
        assert_eq!(live(&names, &[false, true, false, false], "nullbyte"), Some(3), "re-paired after forgetting: the new slot");
    }
}
