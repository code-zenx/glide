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
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt;
use input_capture::{CaptureEvent, CaptureHandle, InputCapture, Position};
use input_emulation::InputEmulation;
use input_event::scancode::Linux as Key;
use input_event::{Event, KeyboardEvent};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use crate::display;
use crate::link::Links;
use crate::proto::Msg;
use crate::status::Status;
use crate::view;
use crate::{Config, Peer, Role, Side};

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

/// Far longer than input takes to arrive from a peer on the same network.
const HAND_BACK_SETTLE: Duration = Duration::from_millis(200);

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
    /// After this machine hands control back, input still in flight from the
    /// peer must not read as the peer taking it again.
    settling_until: Option<Instant>,
}

impl Handover {
    fn new(driven: bool) -> Self {
        Self { mode: Mode::Local, driven, settling_until: None }
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
        self.settling_until = None;
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
            Mode::Local if self.settling_until.is_some_and(|until| Instant::now() < until) => false,
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

    /// This client's edge gave the cursor back to the server.
    fn handed_back(&mut self) {
        self.mode = Mode::Local;
        self.settling_until = Some(Instant::now() + HAND_BACK_SETTLE);
    }
}

/// Asked for from outside the daemon — today, from the macOS menu bar, which
/// is the only thing that builds one.
#[derive(Debug)]
pub enum Control {
    /// Move a peer's screen to another edge, optionally of one display only.
    SetPosition(String, Side, Option<usize>),
    /// Try the input backends again, after permission was granted.
    RetryBackends,
    /// Show the extended display on this device, or on none.
    Extend(Option<String>),
    /// Show this device's screen fullscreen here and drive it, or stop.
    View(Option<String>),
    /// Switch one peer off (or back on). Off: the link is closed and the
    /// peer told why; nothing crosses until it is switched on again.
    SetEnabled(String, bool),
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

/// Whether a peer's screen edge should be armed: only with a live link behind
/// it, never while the two screens are one picture — on a server, the peer
/// showing our extended display, where the cursor belongs on the virtual
/// screen; on a client, the server viewing this screen, where an edge would
/// walk the cursor out of the picture — and never while the peer is switched
/// off. Other peers stay reachable meanwhile.
fn edge_wanted(linked: bool, screen_shared: bool, enabled: bool) -> bool {
    linked && !screen_shared && enabled
}

/// Whether this machine can show a server's extended display: the receiving
/// end drives Hyprland, so an X11 desktop cannot.
fn can_extend_here() -> bool {
    cfg!(not(target_os = "macos")) && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
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

/// Take the cursor and start feeding `peer`, as crossing its edge does.
fn enter(handover: &mut Handover, links: &Arc<Links>, handle: CaptureHandle, peer: &str) {
    handover.began(handle);
    links.send(peer, Msg::Enter);
    push_clipboard(links, peer);
}

/// Hand `peer` the keyboard and mouse without the cursor crossing its edge,
/// the way the toggle chord does. False when the capture cannot: no
/// permission, or its edge is not armed (switched off, paused, extended).
async fn begin_view(
    capture: &mut Option<InputCapture>,
    links: &Arc<Links>,
    handover: &mut Handover,
    (handle, peer): (CaptureHandle, &str),
) -> Result<bool> {
    let Some(capture) = capture.as_mut() else {
        return Ok(false);
    };
    if !capture.begin(handle).await? {
        return Ok(false);
    }
    enter(handover, links, handle, peer);
    Ok(true)
}

/// The handle of a device that has not been forgotten.
fn live(names: &[String], gone: &[bool], name: &str) -> Option<usize> {
    names.iter().enumerate().position(|(i, n)| n == name && !gone[i])
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
    view: tokio::sync::mpsc::UnboundedSender<view::Event>,
) -> Result<()> {
    // Handles are indices into these lists, so a forgotten device keeps its
    // slot (marked gone) until the next start: every other device's handle,
    // and the edge the capture backend keyed to it, stays put.
    let mut names: Vec<String> = config.peers.iter().map(|p| p.name.clone()).collect();
    let mut gone = vec![false; names.len()];
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
    // Only a server has a display to extend.
    let mut extend: Option<usize> = if server {
        crate::extend_target(&config).and_then(|t| names.iter().position(|n| *n == t.name))
    } else {
        None
    };
    // The device whose screen is shown here. Server only.
    let mut viewing: Option<usize> = None;
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
    let publish = |names: &[String], handover: &Handover, degraded: bool, shown: &mut (&'static str, Option<CaptureHandle>, bool)| {
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
        // A view lasts exactly as long as its device has the keyboard and
        // mouse: whatever brings them home (the chord, a release, a dead link,
        // a failed view) ends it too. Checked here because most arms below
        // `continue`.
        if let Some(i) = viewing.filter(|&i| handover.sending() != Some(i as CaptureHandle)) {
            info!(peer = %names[i], "input is home, view stopped");
            viewing = None;
            status.set(|s| s.viewing.clear());
            let _ = view.send(view::Event::Target(None));
        }
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
                                if let Some(emulation) = emulation.as_mut() {
                                    let _ = emulation.release_keys(handle).await;
                                }
                                handover.handed_back();
                                release(capture.as_mut()).await?;
                            }
                            Crossing::Stay => {
                                warn!(%peer, "cursor left this screen but the peer is not connected, keeping input here");
                                release(capture.as_mut()).await?;
                            }
                            Crossing::Send => {
                                info!(%peer, "cursor left this screen; {RELEASE_HINT} takes it back");
                                enter(&mut handover, &links, handle, peer);
                            }
                        },
                        CaptureEvent::Input(event) => {
                            if server && handover.forwarding(handle) {
                                links.send(peer, Msg::Input(event));
                            }
                        }
                        CaptureEvent::Toggle => {
                            if server && handover.forwarding(handle) {
                                info!(%peer, "toggle chord pressed, taking input back");
                                hand_back(&mut capture, &links, peer, &mut handover).await?;
                            }
                        }
                    }
                    let chord =
                        server && capture.as_ref().is_some_and(|c| c.keys_pressed(&RELEASE_CHORD));
                    if chord {
                        info!("release chord pressed, taking input back");
                        hand_back(&mut capture, &links, peer, &mut handover).await?;
                    }
                    publish(&names, &handover, degraded, &mut shown);
                }
                // The event tap dies for reasons outside this process: a
                // password field turning on secure input, a revoked grant. Put
                // input back on this machine rather than leaving the cursor
                // believed to be somewhere else.
                Some(Err(e)) => {
                    warn!("capture error, input is local again: {e}");
                    handover.take_back();
                    publish(&names, &handover, degraded, &mut shown);
                }
                None => {
                    warn!("capture ended");
                    break;
                }
            },
            change = link_state.recv() => {
                let Some((peer, up)) = change else { break };
                let _ = display.send(display::Event::Link(peer.clone(), up));
                let _ = view.send(view::Event::Link(peer.clone(), up));
                let Some(handle) = live(&names, &gone, &peer) else { continue };
                linked[handle] = up;
                if up && !server {
                    links.send(&peer, Msg::Caps { extend: can_extend_here(), view: view::can_share() });
                }
                // A client that was offline when it was moved in the Arrange
                // window still has the old side; say it again on every link.
                if up && server {
                    links.send(&peer, Msg::Arrange(config.peers[handle].position.opposite()));
                }
                if !up {
                    held_off[handle] = false;
                }
                let Some(capture) = capture.as_mut() else { continue };
                let want = edge_wanted(up, extend == Some(handle), enabled[handle] && !held_off[handle]);
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
                if let Control::Extend(target) = asked {
                    let target = target.and_then(|name| live(&names, &gone, &name));
                    if !server || target == extend {
                        continue;
                    }
                    extend = target;
                    let name = target.map(|i| names[i].clone());
                    info!(device = ?name, "extended display");
                    if let Err(e) = crate::set_extend(&config_path, name.as_deref()) {
                        warn!("cannot save the extended display: {e}");
                    }
                    status.set(|s| s.extending = name.clone().unwrap_or_default());
                    let _ = display.send(display::Event::Target(target.map(|i| config.peers[i].clone())));
                    // Extending to the device that has the cursor: bring it
                    // home first, or it stays there with its edge gone.
                    if let Some(handle) = handover.sending().filter(|h| Some(*h as usize) == extend) {
                        links.send(&names[handle as usize], Msg::Leave);
                        handover.take_back();
                        release(capture.as_mut()).await?;
                    }
                    if let Some(capture) = capture.as_mut() {
                        for (i, peer) in config.peers.iter().enumerate() {
                            let want = !gone[i] && edge_wanted(linked[i], extend == Some(i), enabled[i] && !held_off[i]);
                            if want != armed[i] {
                                armed[i] = want;
                                set_edge(capture, i, peer, want).await?;
                            }
                        }
                    }
                    publish(&names, &handover, degraded, &mut shown);
                    continue;
                }
                if let Control::View(name) = asked {
                    // One screen cannot both show the extended display and be
                    // shown here; the menu stops Extend first.
                    let viewable = |n: &str| live(&names, &gone, n).filter(|&i| linked[i] && extend != Some(i));
                    let target = match name.as_deref().map(viewable) {
                        _ if !server => continue,
                        Some(None) => {
                            warn!(peer = ?name, "cannot view it: not linked, or showing the extended display");
                            continue;
                        }
                        target => target.flatten(),
                    };
                    if target == viewing {
                        continue;
                    }
                    // Input comes home first; the check at the top of the loop
                    // then ends the old view, if this starts no new one.
                    if let Some(handle) = handover.sending() {
                        hand_back(&mut capture, &links, &names[handle as usize].clone(), &mut handover).await?;
                    }
                    if let Some(i) = target {
                        if begin_view(&mut capture, &links, &mut handover, (i as CaptureHandle, &names[i])).await? {
                            info!(peer = %names[i], "viewing its screen; {RELEASE_HINT} stops");
                            viewing = Some(i);
                            status.set(|s| s.viewing = names[i].clone());
                            let _ = view.send(view::Event::Target(Some(config.peers[i].clone())));
                        } else {
                            warn!(peer = %names[i], "cannot hand it the keyboard and mouse, so not viewing it");
                            let why = "Glide needs input permission, and the device must not be paused.";
                            crate::pair::notify(&format!("Glide cannot view {}", names[i]), why);
                        }
                    }
                    publish(&names, &handover, degraded, &mut shown);
                    continue;
                }
                if let Control::AddPeer(peer) = asked {
                    match live(&names, &gone, &peer.name) {
                        // Re-paired: new certificate and address, same place.
                        // Links already closed the link under the old key.
                        Some(i) => config.peers[i] = peer,
                        None => {
                            let i = names.len();
                            names.push(peer.name.clone());
                            gone.push(false);
                            armed.push(false);
                            linked.push(false);
                            enabled.push(true);
                            held_off.push(false);
                            if let Some(emulation) = emulation.as_mut() {
                                emulation.create(i as CaptureHandle).await;
                            }
                            info!(peer = %peer.name, side = ?peer.position, display = ?peer.display, "device added");
                            config.peers.push(peer);
                        }
                    }
                    continue;
                }
                if let Control::Forget(name) = asked {
                    let Some(i) = live(&names, &gone, &name) else { continue };
                    if handover.sending() == Some(i as CaptureHandle) {
                        hand_back(&mut capture, &links, &name, &mut handover).await?;
                    }
                    if armed[i] {
                        armed[i] = false;
                        if let Some(capture) = capture.as_mut() {
                            capture.destroy(i as CaptureHandle).await?;
                        }
                    }
                    if extend == Some(i) {
                        extend = None;
                        status.set(|s| s.extending.clear());
                        let _ = display.send(display::Event::Target(None));
                    }
                    gone[i] = true;
                    enabled[i] = false;
                    links.forget(&name);
                    status.remove_peer(&name);
                    if let Err(e) = crate::forget_peer(&config_path, &name) {
                        warn!(peer = %name, "cannot remove it from the config: {e}");
                    }
                    info!(peer = %name, "device forgotten");
                    publish(&names, &handover, degraded, &mut shown);
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
                    publish(&names, &handover, degraded, &mut shown);
                    continue;
                }
                if let Control::Pause(paused) = asked {
                    // Only a client has anything to refuse; the server's own
                    // switch is per client (SetEnabled).
                    if !server {
                        handover.driven = !paused;
                        for (i, name) in names.iter().enumerate().filter(|(i, _)| !gone[*i]) {
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
                        publish(&names, &handover, degraded, &mut shown);
                    }
                    continue;
                }
                if let Control::SetEnabled(peer, on) = asked {
                    let Some(device) = live(&names, &gone, &peer).filter(|_| server) else { continue };
                    for (i, on) in switches(&enabled, &gone, device, on) {
                        let peer = names[i].clone();
                        let handle = i as CaptureHandle;
                        if pause_takes_back(handover.sending(), handle, on) {
                            info!(%peer, "switched off while it had the cursor, taking input back");
                            hand_back(&mut capture, &links, &peer, &mut handover).await?;
                        }
                        enabled[i] = on;
                        config.peers[i].enabled = on;
                        links.set_off(&peer, !on);
                        if on {
                            // The dialer picks it up within a couple of seconds.
                            status.set_peer(&peer, |p| p.mode = "offline".into());
                        } else {
                            // Closing the link drops its edge and pauses an
                            // extended display, which resumes when it is back.
                            links.disconnect(&peer, crate::link::OFF);
                            status.set_peer(&peer, |p| p.mode = "off".into());
                        }
                        if let Err(e) = crate::set_enabled(&config_path, &peer, on) {
                            warn!(%peer, "cannot save the switch: {e}");
                        }
                        let want = edge_wanted(linked[i], extend == Some(i), on && !held_off[i]);
                        if let Some(capture) = capture.as_mut().filter(|_| want != armed[i]) {
                            armed[i] = want;
                            set_edge(capture, i, &config.peers[i], want).await?;
                        }
                        info!(%peer, on, "peer switched");
                    }
                    publish(&names, &handover, degraded, &mut shown);
                    continue;
                }
                let Control::SetPosition(name, side, on_display) = asked else {
                    // RetryBackends: permission may have just been granted, so
                    // build whichever backend this machine's role needs.
                    if capture.is_none() {
                        capture = open_capture().await;
                        if let Some(capture) = capture.as_mut() {
                            for (i, peer) in config.peers.iter().enumerate() {
                                if !armed[i] || gone[i] {
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
                    publish(&names, &handover, degraded, &mut shown);
                    continue;
                };
                let Some(i) = live(&names, &gone, &name) else { continue };
                let (handle, peer) = (i as CaptureHandle, &names[i]);
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
                let Some(handle) = live(&names, &gone, &peer).map(|i| i as CaptureHandle) else { continue };
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
                    // Sent on every link, so most of the time nothing moved.
                    Msg::Arrange(side) if config.peers[handle as usize].position == side => {}
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
                            status.set_peer(&peer, |p| p.paused = paused);
                            let want = edge_wanted(linked[i], extend == Some(i), enabled[i] && !paused);
                            if let Some(capture) = capture.as_mut().filter(|_| want != armed[i]) {
                                armed[i] = want;
                                set_edge(capture, i, &config.peers[i], want).await?;
                            }
                            info!(%peer, paused, "client paused itself");
                        } else {
                            status.set_peer(&peer, |p| p.paused = paused);
                            info!(%peer, paused, "server switched this machine");
                        }
                        publish(&names, &handover, degraded, &mut shown);
                    }
                    Msg::Caps { extend: can, view: can_view } => {
                        info!(%peer, can_extend = can, can_view, "peer described what it can do");
                        status.set_peer(&peer, |p| {
                            p.can_extend = can;
                            p.can_view = can_view;
                        });
                    }
                    Msg::Display(_) | Msg::DisplayReady(_) => {
                        let _ = display.send(display::Event::Msg(peer, msg));
                    }
                    Msg::View(on) if !server => {
                        let i = handle as usize;
                        let want = edge_wanted(linked[i], on, enabled[i] && !held_off[i]);
                        if let Some(capture) = capture.as_mut().filter(|_| want != armed[i]) {
                            armed[i] = want;
                            set_edge(capture, i, &config.peers[i], want).await?;
                        }
                        let _ = view.send(view::Event::Msg(peer, msg));
                    }
                    Msg::View(_) | Msg::ViewReady(_) => {
                        let _ = view.send(view::Event::Msg(peer, msg));
                    }
                    Msg::Ping(_) | Msg::Pong(_) => {}
                }
                publish(&names, &handover, degraded, &mut shown);
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
