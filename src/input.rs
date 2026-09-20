//! Who owns the keyboard and mouse right now, and where their events go.
//!
//! Input flows one way. The **server** is the machine whose keyboard and mouse
//! are shared: it watches its own screen edges, and when the cursor leaves one
//! it starts sending. The **client** only types what arrives. A client arms no
//! edge and a server builds no emulation backend, so neither machine can be
//! driven by the other except in the one direction the config asks for.

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use input_capture::{CaptureEvent, CaptureHandle, InputCapture, Position};
use input_emulation::InputEmulation;
use input_event::scancode::Linux as Key;
use input_event::{Event, KeyboardEvent};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use crate::link::Links;
use crate::proto::Msg;
use crate::status::Status;
use crate::{Config, Role, Side};

/// Held together, these hand input back to this machine even if the peer never
/// answers again — the way out of a wedged remote session.
const RELEASE_CHORD: [Key; 4] = [
    Key::KeyLeftCtrl,
    Key::KeyLeftShift,
    Key::KeyLeftAlt,
    Key::KeyLeftMeta,
];

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
enum Cleanup {
    Nothing,
    /// Stop grabbing local input.
    ReleaseCapture,
    /// Lift any key the peer left held down here.
    ReleaseKeys,
}

/// The rule for who owns input, kept apart from the backends so it can be
/// tested without a compositor.
struct Handover {
    mode: Mode,
    /// Whether a peer is allowed to drive this machine at all. False on a
    /// server: its keyboard and mouse are the ones being shared, and nothing
    /// arriving over the link may type here.
    driven: bool,
}

impl Handover {
    fn new(driven: bool) -> Self {
        Self { mode: Mode::Local, driven }
    }

    fn name(&self) -> &'static str {
        match self.mode {
            Mode::Local => "local",
            Mode::Sending(_) => "sending",
            Mode::Receiving(_) => "receiving",
        }
    }

    /// Our own capture grabbed the cursor at this edge.
    fn began(&mut self, handle: CaptureHandle) {
        self.mode = Mode::Sending(handle);
    }

    fn forwarding(&self, handle: CaptureHandle) -> bool {
        self.mode == Mode::Sending(handle)
    }

    /// The peer says it has the cursor. True when our capture must let go.
    fn peer_entered(&mut self, handle: CaptureHandle) -> bool {
        if !self.driven {
            return false;
        }
        let was_sending = matches!(self.mode, Mode::Sending(_));
        self.mode = Mode::Receiving(handle);
        was_sending
    }

    /// The peer handed control back, or its link died.
    fn peer_left(&mut self, handle: CaptureHandle) -> Cleanup {
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
    fn peer_input(&mut self, handle: CaptureHandle) -> bool {
        if !self.driven {
            return false;
        }
        match self.mode {
            Mode::Receiving(h) => h == handle,
            Mode::Local => {
                self.mode = Mode::Receiving(handle);
                true
            }
            Mode::Sending(_) => false,
        }
    }

    /// The release chord: take input back whatever the peer thinks.
    fn take_back(&mut self) {
        self.mode = Mode::Local;
    }
}

/// Asked for from outside the daemon — today, from the macOS menu bar, which
/// is the only thing that builds one.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Control {
    /// Move a peer's screen to another edge, optionally of one display only.
    SetPosition(CaptureHandle, Side, Option<usize>),
    /// Try the input backends again, after permission was granted.
    RetryBackends,
}

pub async fn session(
    config: Config,
    config_path: std::path::PathBuf,
    links: Arc<Links>,
    status: Arc<Status>,
    mut from_links: UnboundedReceiver<(String, Msg)>,
    mut control: UnboundedReceiver<Control>,
) -> Result<()> {
    let names: Vec<String> = config.peers.iter().map(|p| p.name.clone()).collect();
    let server = config.role == Role::Server;

    // Only the end that needs a backend builds one, so a client never grabs a
    // screen edge and a server can never be typed on from the wire.
    let mut capture = if server { open_capture().await } else { None };
    let mut emulation = if server { None } else { open_emulation().await };
    let mut degraded = if server { capture.is_none() } else { emulation.is_none() };
    status.allowed(!degraded);

    for (i, peer) in config.peers.iter().enumerate() {
        let handle = i as CaptureHandle;
        if let Some(capture) = capture.as_mut() {
            // A display numbered for people is 1-based; the backend counts from 0.
            capture.restrict_to_display(peer.display.map(|d| d.saturating_sub(1))).await;
            capture.create(handle, peer.position.into()).await?;
            info!(peer = %peer.name, side = ?peer.position, display = ?peer.display, "screen edge armed");
        }
        if let Some(emulation) = emulation.as_mut() {
            emulation.create(handle).await;
        }
    }

    let mut handover = Handover::new(!server);
    // Every captured motion event used to lock the status and compare strings.
    // Only a change is worth that, and the label is a `&'static str`.
    let mut shown = "";
    let publish = |handover: &Handover, degraded: bool, shown: &mut &'static str| {
        let now = handover.name();
        if *shown != now {
            *shown = now;
            status.set(|s| s.mode = now.into());
        }
        status.allowed(!degraded);
    };
    publish(&handover, degraded, &mut shown);
    loop {
        tokio::select! {
            captured = next_capture(capture.as_mut()) => match captured {
                Some(Ok((handle, event))) => {
                    let Some(peer) = names.get(handle as usize) else { continue };
                    match event {
                        CaptureEvent::Begin => {
                            info!(%peer, "cursor left this screen");
                            handover.began(handle);
                            links.send(peer, Msg::Enter);
                            push_clipboard(&links, peer);
                        }
                        CaptureEvent::Input(event) => {
                            if handover.forwarding(handle) {
                                links.send(peer, Msg::Input(event));
                            }
                        }
                    }
                    let chord = capture.as_ref().is_some_and(|c| c.keys_pressed(&RELEASE_CHORD));
                    if chord {
                        info!("release chord pressed, taking input back");
                        // The peer saw these keys go down but will never see
                        // them come up, so say so before we stop sending.
                        let held = capture.as_mut().map(|c| c.take_pressed_keys()).unwrap_or_default();
                        for key in held {
                            links.send(peer, Msg::Input(Event::Keyboard(KeyboardEvent::Key {
                                time: 0,
                                key: key as u32,
                                state: 0,
                            })));
                        }
                        links.send(peer, Msg::Leave);
                        handover.take_back();
                        release(capture.as_mut()).await?;
                    }
                    publish(&handover, degraded, &mut shown);
                }
                Some(Err(e)) => warn!("capture error: {e}"),
                None => {
                    warn!("capture ended");
                    break;
                }
            },
            asked = control.recv() => {
                let Some(asked) = asked else { break };
                let Control::SetPosition(handle, side, on_display) = asked else {
                    // RetryBackends: permission may have just been granted, so
                    // build whichever backend this machine's role needs.
                    if server && capture.is_none() {
                        capture = open_capture().await;
                        if let Some(capture) = capture.as_mut() {
                            for (i, peer) in config.peers.iter().enumerate() {
                                capture
                                    .restrict_to_display(peer.display.map(|d| d.saturating_sub(1)))
                                    .await;
                                capture.create(i as CaptureHandle, peer.position.into()).await?;
                            }
                            info!("input capture is working now");
                        }
                    }
                    if !server && emulation.is_none() {
                        emulation = open_emulation().await;
                        if let Some(emulation) = emulation.as_mut() {
                            for i in 0..config.peers.len() {
                                emulation.create(i as CaptureHandle).await;
                            }
                            info!("input emulation is working now");
                        }
                    }
                    degraded = if server { capture.is_none() } else { emulation.is_none() };
                    publish(&handover, degraded, &mut shown);
                    continue;
                };
                let Some(peer) = names.get(handle as usize) else { continue };
                // Re-arm the edge: the capture backend keys a grab to a screen
                // side, so moving the peer means dropping it and taking a new one.
                if let Some(capture) = capture.as_mut() {
                    capture.destroy(handle).await?;
                    capture.restrict_to_display(on_display.map(|d| d.saturating_sub(1))).await;
                    capture.create(handle, side.into()).await?;
                    info!(%peer, ?side, display = ?on_display, "screen edge moved");
                }
                // Tell the peer which of its own edges now faces us, so both
                // ends agree even if the roles are ever swapped.
                links.send(peer, Msg::Arrange(side.opposite()));
            },
            incoming = from_links.recv() => {
                let Some((peer, msg)) = incoming else { break };
                let Some(handle) = names.iter().position(|n| *n == peer).map(|i| i as CaptureHandle) else { continue };
                match msg {
                    Msg::Enter => {
                        if handover.peer_entered(handle) {
                            release(capture.as_mut()).await?;
                        }
                        info!(%peer, "peer took the cursor");
                    }
                    Msg::Leave => match handover.peer_left(handle) {
                        Cleanup::ReleaseKeys => {
                            // Never leave a modifier held down here because the
                            // link dropped mid-chord.
                            if let Some(emulation) = emulation.as_mut() {
                                let _ = emulation.release_keys(handle).await;
                            }
                            info!(%peer, "input is local again");
                        }
                        Cleanup::ReleaseCapture => {
                            release(capture.as_mut()).await?;
                            warn!(%peer, "peer went away while it had the cursor, input is local again");
                        }
                        Cleanup::Nothing => {}
                    },
                    // On a server `peer_input` is always false, so a client
                    // that sends input anyway is ignored rather than obeyed.
                    Msg::Input(event) => {
                        if handover.peer_input(handle) {
                            match emulation.as_mut() {
                                Some(emulation) => {
                                    if let Err(e) = emulation.consume(event, handle).await {
                                        warn!(%peer, "cannot emulate: {e}");
                                    }
                                }
                                None => warn!(%peer, "input arrived but this machine cannot type"),
                            }
                        } else if server {
                            debug!(%peer, "ignoring input from a client");
                        }
                    }
                    Msg::Clipboard(clip) => {
                        info!(%peer, "clipboard arrived");
                        tokio::spawn(crate::clipboard::write(clip));
                    }
                    Msg::Arrange(side) => {
                        info!(%peer, ?side, "peer says it sits on our {side:?}");
                        if let Err(e) = crate::set_peer_side(&config_path, &peer, side) {
                            warn!(%peer, "cannot save the arrangement: {e}");
                        }
                        if let Some(capture) = capture.as_mut() {
                            capture.destroy(handle).await?;
                            capture.create(handle, side.into()).await?;
                        }
                    }
                    Msg::Screens(screens) => {
                        info!(%peer, count = screens.len(), "peer described its screens");
                        status.set_screens(&peer, screens);
                    }
                    Msg::Ping(_) | Msg::Pong(_) => {}
                }
                publish(&handover, degraded, &mut shown);
            }
        }
    }
    if let Some(emulation) = emulation.as_mut() {
        emulation.terminate().await;
    }
    if let Some(capture) = capture.as_mut() {
        capture.terminate().await?;
    }
    Ok(())
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
) -> Option<Result<(CaptureHandle, CaptureEvent), input_capture::CaptureError>> {
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
