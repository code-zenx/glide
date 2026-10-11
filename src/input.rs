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

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use input_capture::{CaptureEvent, CaptureHandle, InputCapture, Position};
use input_emulation::InputEmulation;
use input_event::scancode::Linux as Key;
use input_event::{Event, KeyboardEvent};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use crate::display::{self, Mode as DisplayMode};
use crate::link::Links;
use crate::proto::Msg;
use crate::status::Status;
use crate::{Config, Peer, Role, Side};

/// Held together, these hand input back to this machine even if the peer never
/// answers again — the way out of a wedged remote session.
const RELEASE_CHORD: [Key; 4] = [
    Key::KeyLeftCtrl,
    Key::KeyLeftShift,
    Key::KeyLeftAlt,
    Key::KeyLeftMeta,
];

/// The chord spelled out, logged the moment the cursor leaves — the one time
/// someone might need it is the one time they cannot look it up.
const RELEASE_HINT: &str = "Ctrl+Shift+Alt+Cmd together";

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

    /// The peer input is flowing to or from, if any.
    fn peer(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Local => None,
            Mode::Sending(handle) | Mode::Receiving(handle) => Some(handle),
        }
    }

    /// Our own capture grabbed the cursor at this edge.
    fn began(&mut self, handle: CaptureHandle) {
        self.mode = Mode::Sending(handle);
    }

    /// The peer driving this machine, if any.
    fn receiving(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Receiving(handle) => Some(handle),
            _ => None,
        }
    }

    /// The peer this machine is driving, if any.
    fn sending(&self) -> Option<CaptureHandle> {
        match self.mode {
            Mode::Sending(handle) => Some(handle),
            _ => None,
        }
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
#[derive(Debug)]
pub enum Control {
    /// Move a peer's screen to another edge, optionally of one display only.
    SetPosition(CaptureHandle, Side, Option<usize>),
    /// Try the input backends again, after permission was granted.
    RetryBackends,
    /// Switch between driving the peer and extending this screen onto it.
    SetMode(DisplayMode),
    /// Switch one peer off (or back on). Paused: its edge is disarmed, no
    /// clipboard crosses, and it is told so; the link itself stays up.
    SetEnabled(CaptureHandle, bool),
    /// From this machine's own bar: a client stops being driven (and tells the
    /// server); undone with `false`.
    Pause(bool),
    /// Bring input back to this machine now, wherever the cursor is.
    Release,
}

/// Whether a peer's screen edge should be armed: only with a live link behind
/// it, never while the peer is showing our extended display — the cursor
/// then belongs on the virtual screen, not in the peer — and never while the
/// peer is switched off.
fn edge_wanted(linked: bool, extending: bool, enabled: bool) -> bool {
    linked && !extending && enabled
}

/// Switching a peer off while it holds the cursor must bring the cursor home
/// first: its edge is about to go, and nothing else would return it.
fn pause_takes_back(sending: Option<CaptureHandle>, handle: CaptureHandle, on: bool) -> bool {
    !on && sending == Some(handle)
}

/// Bring input home from `peer`: the peer saw these keys go down but will
/// never see them come up, so say so before saying goodbye.
async fn hand_back(capture: &mut Option<InputCapture>, links: &Links, peer: &str, handover: &mut Handover) -> Result<()> {
    let held = capture.as_mut().map(|c| c.take_pressed_keys()).unwrap_or_default();
    for key in held {
        links.send(peer, Msg::Input(Event::Keyboard(KeyboardEvent::Key { time: 0, key: key as u32, state: 0 })));
    }
    links.send(peer, Msg::Leave);
    handover.take_back();
    release(capture.as_mut()).await
}

async fn set_edge(capture: &mut InputCapture, handle: usize, peer: &Peer, on: bool) -> Result<()> {
    if on {
        // A display numbered for people is 1-based; the backend counts from 0.
        capture.restrict_to_display(peer.display.map(|d| d.saturating_sub(1))).await;
        capture.create(handle as CaptureHandle, peer.position.into()).await?;
    } else {
        capture.destroy(handle as CaptureHandle).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn session(
    mut config: Config,
    config_path: std::path::PathBuf,
    links: Arc<Links>,
    status: Arc<Status>,
    mut from_links: UnboundedReceiver<(String, Msg)>,
    mut link_state: UnboundedReceiver<(String, bool)>,
    mut control: UnboundedReceiver<Control>,
    display: tokio::sync::mpsc::UnboundedSender<display::Event>,
) -> Result<()> {
    let names: Vec<String> = config.peers.iter().map(|p| p.name.clone()).collect();
    let server = config.role == Role::Server;

    // Both ends watch an edge — the server to take the cursor, the client to
    // give it back — but only a client can be typed on from the wire.
    let mut capture = open_capture().await;
    let mut emulation = if server { None } else { open_emulation().await };
    let mut degraded = capture.is_none() || (!server && emulation.is_none());
    status.allowed(!degraded);

    // Edges are armed when the link comes up, not now. An edge with no link
    // behind it grabs the cursor and feeds it to nobody, and with the peer
    // above this screen that is every trip to the menu bar.
    let mut armed = vec![false; config.peers.len()];
    let mut linked = vec![false; config.peers.len()];
    let mut enabled: Vec<bool> = config.peers.iter().map(|p| !server || p.enabled).collect();
    // A client that paused itself: like switched off, but its choice, not saved here.
    let mut held_off = vec![false; config.peers.len()];
    for (i, on) in enabled.iter().enumerate() {
        links.set_paused(&names[i], !on);
        status.set_peer(&names[i], |p| p.paused = !on);
    }
    // Only a server has a display to extend.
    let mut extending = server && config.display.mode == DisplayMode::Extend;
    for i in 0..config.peers.len() {
        if let Some(emulation) = emulation.as_mut() {
            emulation.create(i as CaptureHandle).await;
        }
    }

    let mut handover = Handover::new(!server);
    // This runs on every captured motion event, so it must touch nothing that
    // allocates unless something actually changed: `Status::set` clones the
    // whole state to compare it, and at a thousand events a second that is all
    // wasted work on the path that has to stay quick.
    let mut shown = ("", None, !degraded);
    let publish = |handover: &Handover, degraded: bool, shown: &mut (&'static str, Option<CaptureHandle>, bool)| {
        let now = (handover.name(), handover.peer(), !degraded);
        if *shown != now {
            *shown = now;
            status.set(|s| {
                s.flow = now.0.into();
                s.active = now.1.and_then(|h| names.get(h as usize)).cloned().unwrap_or_default();
                s.denied = degraded;
            });
        }
    };
    status.set(|s| {
        s.flow = handover.name().into();
        s.active.clear();
        s.denied = degraded;
    });
    loop {
        tokio::select! {
            captured = next_capture(capture.as_mut()) => match captured {
                Some(Ok((handle, event))) => {
                    let Some(peer) = names.get(handle as usize) else { continue };
                    match event {
                        CaptureEvent::Begin => match crossing(server, links.connected(peer)) {
                            // Says "you have it back" and nothing else: no key
                            // and no motion ever crosses this way.
                            Crossing::HandBack => {
                                info!(%peer, "cursor reached this edge, handing control back");
                                links.send(peer, Msg::Leave);
                                release(capture.as_mut()).await?;
                            }
                            Crossing::Stay => {
                                warn!(%peer, "cursor left this screen but the peer is not connected, keeping input here");
                                release(capture.as_mut()).await?;
                            }
                            Crossing::Send => {
                                info!(%peer, "cursor left this screen; {RELEASE_HINT} takes it back");
                                handover.began(handle);
                                links.send(peer, Msg::Enter);
                                push_clipboard(&links, peer);
                            }
                        },
                        CaptureEvent::Input(event) => {
                            if server && handover.forwarding(handle) {
                                links.send(peer, Msg::Input(event));
                            }
                        }
                    }
                    let chord =
                        server && capture.as_ref().is_some_and(|c| c.keys_pressed(&RELEASE_CHORD));
                    if chord {
                        info!("release chord pressed, taking input back");
                        hand_back(&mut capture, &links, peer, &mut handover).await?;
                    }
                    publish(&handover, degraded, &mut shown);
                }
                // The event tap dies for reasons outside this process: a
                // password field turning on secure input, a revoked grant. Put
                // input back on this machine rather than leaving the cursor
                // believed to be somewhere else.
                Some(Err(e)) => {
                    warn!("capture error, input is local again: {e}");
                    handover.take_back();
                    publish(&handover, degraded, &mut shown);
                }
                None => {
                    warn!("capture ended");
                    break;
                }
            },
            change = link_state.recv() => {
                let Some((peer, up)) = change else { break };
                let _ = display.send(display::Event::Link(peer.clone(), up));
                let Some(handle) = names.iter().position(|n| *n == peer) else { continue };
                linked[handle] = up;
                if up && !enabled[handle] {
                    links.send(&peer, Msg::Paused(true));
                }
                let Some(capture) = capture.as_mut() else { continue };
                if !up {
                    held_off[handle] = false;
                }
                let want = edge_wanted(up, extending, enabled[handle] && !held_off[handle]);
                if want == armed[handle] {
                    continue;
                }
                armed[handle] = want;
                let at = &config.peers[handle];
                set_edge(capture, handle, at, want).await?;
                if want {
                    let purpose = if server { "to send from" } else { "to hand control back" };
                    info!(%peer, side = ?at.position, display = ?at.display, "screen edge armed {purpose}");
                } else {
                    info!(%peer, "link down, screen edge dropped");
                }
            },
            asked = control.recv() => {
                let Some(asked) = asked else { break };
                if let Control::SetMode(mode) = asked {
                    extending = server && mode == DisplayMode::Extend;
                    info!(?mode, "display mode");
                    if let Err(e) = crate::set_display_mode(&config_path, mode) {
                        warn!("cannot save the display mode: {e}");
                    }
                    let _ = display.send(display::Event::Mode(mode));
                    // Extending while the cursor is in the peer: bring it home
                    // first, or it stays there with its edge gone.
                    if let Some(handle) = handover.sending().filter(|_| extending) {
                        links.send(&names[handle as usize], Msg::Leave);
                        handover.take_back();
                        release(capture.as_mut()).await?;
                    }
                    if let Some(capture) = capture.as_mut() {
                        for (i, peer) in config.peers.iter().enumerate() {
                            let want = edge_wanted(linked[i], extending, enabled[i] && !held_off[i]);
                            if want != armed[i] {
                                armed[i] = want;
                                set_edge(capture, i, peer, want).await?;
                            }
                        }
                    }
                    publish(&handover, degraded, &mut shown);
                    continue;
                }
                if let Control::Release = asked {
                    if let Some(handle) = handover.sending() {
                        info!(peer = %names[handle as usize], "release asked for, taking input back");
                        hand_back(&mut capture, &links, &names[handle as usize].clone(), &mut handover).await?;
                    } else if let Some(handle) = handover.receiving() {
                        // A client gives the cursor back the way its edge would.
                        links.send(&names[handle as usize], Msg::Leave);
                        if let Some(emulation) = emulation.as_mut() {
                            let _ = emulation.release_keys(handle).await;
                        }
                        handover.take_back();
                    }
                    publish(&handover, degraded, &mut shown);
                    continue;
                }
                if let Control::Pause(paused) = asked {
                    // Only a client has anything to refuse; the server's own
                    // switch is per client (SetEnabled).
                    if !server {
                        handover.driven = !paused;
                        for (i, name) in names.iter().enumerate() {
                            links.send(name, Msg::Paused(paused));
                            status.set_peer(name, |p| p.paused = paused);
                            if paused && handover.receiving() == Some(i as CaptureHandle) {
                                if let Some(emulation) = emulation.as_mut() {
                                    let _ = emulation.release_keys(i as CaptureHandle).await;
                                }
                                handover.take_back();
                            }
                        }
                        info!(paused, "this machine paused itself");
                        publish(&handover, degraded, &mut shown);
                    }
                    continue;
                }
                if let Control::SetEnabled(handle, on) = asked {
                    let i = handle as usize;
                    let Some(peer) = names.get(i).cloned() else { continue };
                    if pause_takes_back(handover.sending(), handle, on) {
                        info!(%peer, "switched off while it had the cursor, taking input back");
                        hand_back(&mut capture, &links, &peer, &mut handover).await?;
                    }
                    enabled[i] = on;
                    config.peers[i].enabled = on;
                    links.set_paused(&peer, !on);
                    status.set_peer(&peer, |p| p.paused = !on);
                    links.send(&peer, Msg::Paused(!on));
                    if let Err(e) = crate::set_enabled(&config_path, &peer, on) {
                        warn!(%peer, "cannot save the switch: {e}");
                    }
                    let want = edge_wanted(linked[i], extending, on && !held_off[i]);
                    if let Some(capture) = capture.as_mut().filter(|_| want != armed[i]) {
                        armed[i] = want;
                        set_edge(capture, i, &config.peers[i], want).await?;
                    }
                    info!(%peer, on, "peer switched");
                    publish(&handover, degraded, &mut shown);
                    continue;
                }
                let Control::SetPosition(handle, side, on_display) = asked else {
                    // RetryBackends: permission may have just been granted, so
                    // build whichever backend this machine's role needs.
                    if capture.is_none() {
                        capture = open_capture().await;
                        if let Some(capture) = capture.as_mut() {
                            for (i, peer) in config.peers.iter().enumerate() {
                                if !armed[i] {
                                    continue;
                                }
                                set_edge(capture, i, peer, true).await?;
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
                    degraded = capture.is_none() || (!server && emulation.is_none());
                    publish(&handover, degraded, &mut shown);
                    continue;
                };
                let Some(peer) = names.get(handle as usize) else { continue };
                // Kept current, so re-arming later puts the edge where it is
                // now rather than where it was at startup.
                config.peers[handle as usize].position = side;
                config.peers[handle as usize].display = on_display;
                // Re-arm the edge: the capture backend keys a grab to a screen
                // side, so moving the peer means dropping it and taking a new one.
                if let Some(capture) = capture.as_mut().filter(|_| armed[handle as usize]) {
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
                                    #[cfg(not(target_os = "macos"))]
                                    let event = stamp_now(event);
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
                        config.peers[handle as usize].position = side;
                        if let Some(capture) = capture.as_mut().filter(|_| armed[handle as usize]) {
                            capture.destroy(handle).await?;
                            capture.create(handle, side.into()).await?;
                        }
                    }
                    Msg::Screens(screens) => {
                        info!(%peer, count = screens.len(), "peer described its screens");
                        status.set_screens(&peer, screens);
                    }
                    // The server switched this machine off or on. Only shown:
                    // the server already stopped sending.
                    // From the server: it switched this machine off or on; only
                    // shown, the server already stopped sending. From a client:
                    // it paused itself, so its edge goes and the cursor comes home.
                    Msg::Paused(paused) => {
                        let i = handle as usize;
                        if server {
                            if paused && handover.sending() == Some(handle) {
                                hand_back(&mut capture, &links, &peer, &mut handover).await?;
                            }
                            held_off[i] = paused;
                            status.set_peer(&peer, |p| p.paused = paused || !enabled[i]);
                            let want = edge_wanted(linked[i], extending, enabled[i] && !paused);
                            if let Some(capture) = capture.as_mut().filter(|_| want != armed[i]) {
                                armed[i] = want;
                                set_edge(capture, i, &config.peers[i], want).await?;
                            }
                            info!(%peer, paused, "client paused itself");
                        } else {
                            status.set_peer(&peer, |p| p.paused = paused);
                            info!(%peer, paused, "server switched this machine");
                        }
                        publish(&handover, degraded, &mut shown);
                    }
                    Msg::Display(_) | Msg::DisplayReady(_) => {
                        let _ = display.send(display::Event::Msg(peer, msg));
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

/// Stamp an arriving event with this machine's clock before it is typed.
///
/// The Mac capture sends every event with `time: 0`, and the wlroots backend
/// hands that straight to the compositor. Chromium ignores a dialog click that
/// looks older than the dialog (its guard against accidental clicks), so Edge's
/// "Add extension" button never worked through glide. Wayland times are
/// `CLOCK_MONOTONIC` in milliseconds, wrapping at `u32`.
#[cfg(not(target_os = "macos"))]
fn stamp_now(mut event: Event) -> Event {
    use input_event::PointerEvent;
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

    /// A client: driven by the server, shares nothing of its own.
    fn client() -> Handover {
        Handover::new(true)
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn arriving_events_get_a_real_time() {
        use input_event::PointerEvent;
        let click = Event::Pointer(PointerEvent::Button { time: 0, button: 0x110, state: 1 });
        let Event::Pointer(PointerEvent::Button { time, .. }) = stamp_now(click) else { unreachable!() };
        assert_ne!(time, 0);
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

    #[test]
    fn pausing_the_peer_with_the_cursor_takes_it_back() {
        assert!(pause_takes_back(Some(1), 1, false), "off while it has the cursor: bring it home");
        assert!(!pause_takes_back(Some(0), 1, false), "another peer has it: leave it");
        assert!(!pause_takes_back(None, 1, false), "nobody has it: nothing to take");
        assert!(!pause_takes_back(Some(1), 1, true), "switching on never takes anything");
    }
}
