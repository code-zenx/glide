//! An extended display: the server makes a virtual screen, and the client shows
//! it fullscreen on one of its own monitors.
//!
//! The server decides everything — whether the display is on, which workspace
//! the client shows it on, how many bits it gets. The client answers with the
//! mode of its monitor and does as it is told.
//!
//! Two modes, switched from the macOS menu bar:
//!
//! - **Control**: today's KVM. No video code runs at all.
//! - **Extend**: the client is an extra monitor. The KVM edge is disarmed, so
//!   the cursor walks onto the virtual display instead of into the client.
//!
//! Video never shares a connection with input. It rides a connection of its
//! own (see `link.rs`), and the sender keeps at most one frame waiting: when it
//! falls behind it throws frames away and asks the encoder for a keyframe,
//! trading one stuttered frame for latency that never builds up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use quinn::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::proto::{Msg, ScreenMode, Show};

/// A frame can be a keyframe of a busy 4K screen; anything past this is a
/// desynchronised stream, not a frame.
const MAX_FRAME: usize = 16 << 20;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The KVM: the cursor crosses into the peer and drives it.
    #[default]
    Control,
    /// The peer's monitor is an extra screen for this machine.
    Extend,
}

/// `[display]` in the server's config. A client ignores it: it is told.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub mode: Mode,
    /// Which of the client's workspaces shows the display.
    #[serde(default = "default_workspace")]
    pub workspace: u8,
    #[serde(default = "default_bitrate")]
    pub bitrate_mbps: u32,
    /// Offer Retina modes on the virtual display. The stream stays at the
    /// client's pixels either way; HiDPI only changes what macOS draws.
    /// Off by default: on a 1080p client, drawing at 2x and shrinking every
    /// frame back down leaves text softer than drawing it 1:1.
    #[serde(default)]
    pub hidpi: bool,
}

fn default_workspace() -> u8 {
    9
}

fn default_bitrate() -> u32 {
    30
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Control,
            workspace: default_workspace(),
            bitrate_mbps: default_bitrate(),
            hidpi: false,
        }
    }
}

/// Everything the display side of the daemon hears about, funnelled through
/// the input session, which already sees every link change and message.
pub enum Event {
    /// The menu bar switched modes.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Mode(Mode),
    Link(#[cfg_attr(not(target_os = "macos"), allow(dead_code))] String, bool),
    Msg(String, Msg),
    /// A peer opened a video connection to us.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Video(String, Connection),
}

/// One encoded frame, as Annex-B NAL units.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub key: bool,
    pub data: Vec<u8>,
}

/// Where the encoder leaves a frame for the network to pick up. Holds one.
///
/// A second frame arriving before the first went out means the network is
/// behind. Both are dropped — the second refers to the first, so sending it
/// alone would smear the picture — and everything after is dropped too until
/// the keyframe the encoder is asked for arrives. Latency can therefore never
/// grow past about a frame.
#[derive(Default)]
pub struct Slot {
    state: Mutex<SlotState>,
    ready: Notify,
    want_key: AtomicBool,
}

#[derive(Default)]
struct SlotState {
    frame: Option<Frame>,
    waiting_for_key: bool,
}

impl Slot {
    /// Called from the encoder's thread.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn push(&self, frame: Frame) {
        let mut state = self.state.lock().unwrap();
        if state.frame.is_some() {
            state.frame = None;
            state.waiting_for_key = true;
            self.want_key.store(true, Ordering::Relaxed);
        }
        if state.waiting_for_key && !frame.key {
            return;
        }
        state.waiting_for_key = false;
        state.frame = Some(frame);
        drop(state);
        self.ready.notify_one();
    }

    /// The next frame to send, waiting for one if there is none.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub async fn pop(&self) -> Frame {
        loop {
            if let Some(frame) = self.state.lock().unwrap().frame.take() {
                return frame;
            }
            self.ready.notified().await;
        }
    }

    /// A new receiver knows nothing: start it from a keyframe.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        state.frame = None;
        state.waiting_for_key = true;
        self.want_key.store(true, Ordering::Relaxed);
    }

    /// Whether the encoder should make its next frame a keyframe. Clears the
    /// request.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn take_key_request(&self) -> bool {
        self.want_key.swap(false, Ordering::Relaxed)
    }
}

/// `[u32 length][u8 keyframe][NAL units]`, on the video connection's one stream.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<()> {
    let len = u32::try_from(frame.data.len() + 1)?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&[frame.key as u8]).await?;
    send.write_all(&frame.data).await?;
    Ok(())
}

/// Not cancel-safe: only awaited by a loop that has nothing to race it with.
#[cfg_attr(target_os = "macos", allow(dead_code))]
async fn read_frame(recv: &mut RecvStream) -> Result<Frame> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(anyhow!("video frame of {len} bytes"));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    let key = buf[0] != 0;
    buf.remove(0);
    Ok(Frame { key, data: buf })
}

/// What the server does next, decided apart from the frameworks so the rules
/// can be tested without a screen.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Step {
    Ask(Show),
    Start(ScreenMode),
    Stop,
    /// Stop in this long unless the link comes back first.
    StopLater,
}

/// A link that blips must not throw every window off the virtual display, so a
/// running display outlives its link by this long.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Host {
    mode: Mode,
    linked: bool,
    workspace: u8,
    running: Option<ScreenMode>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Host {
    fn ask(&self) -> Step {
        Step::Ask(match self.mode {
            Mode::Extend => Show::On { workspace: self.workspace },
            Mode::Control => Show::Off,
        })
    }

    fn mode(&mut self, mode: Mode) -> Vec<Step> {
        self.mode = mode;
        let mut steps = Vec::new();
        if mode == Mode::Control && self.running.take().is_some() {
            steps.push(Step::Stop);
        }
        if self.linked {
            steps.push(self.ask());
        }
        steps
    }

    fn link(&mut self, up: bool) -> Vec<Step> {
        self.linked = up;
        match (up, self.mode, self.running) {
            (true, Mode::Extend, _) => vec![self.ask()],
            (false, _, Some(_)) => vec![Step::StopLater],
            _ => vec![],
        }
    }

    fn ready(&mut self, answer: Result<ScreenMode, String>) -> Vec<Step> {
        match answer {
            _ if self.mode != Mode::Extend => vec![],
            // Same monitor as before: the display and its windows stay put,
            // and the sender reconnects on its own.
            Ok(screen) if self.running == Some(screen) => vec![],
            Ok(screen) => {
                let mut steps = if self.running.is_some() { vec![Step::Stop] } else { vec![] };
                self.running = Some(screen);
                steps.push(Step::Start(screen));
                steps
            }
            Err(_) => self.running.take().map(|_| vec![Step::Stop]).unwrap_or_default(),
        }
    }

    /// The grace period ran out. Only stops if the link is still down.
    fn grace_over(&mut self) -> Vec<Step> {
        if self.linked || self.running.take().is_none() {
            return vec![];
        }
        vec![Step::Stop]
    }
}

/// The server's side: owns the virtual display and streams it.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub async fn host(
    settings: Settings,
    config_path: std::path::PathBuf,
    net: crate::link::Net,
    peer: crate::Peer,
    links: Arc<crate::link::Links>,
    status: Arc<crate::status::Status>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    use tracing::{info, warn};

    let mut host = Host { mode: settings.mode, linked: false, workspace: settings.workspace, running: None };
    let bitrate = settings.bitrate_mbps.saturating_mul(1_000_000);
    let mut running: Option<(crate::display_mac::Capture, tokio::task::JoinHandle<()>)> = None;
    let mut grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let say = |text: String| status.set(|s| s.display = text);
    say(String::new());

    loop {
        let steps = tokio::select! {
            event = events.recv() => match event {
                None => break,
                Some(Event::Mode(mode)) => host.mode(mode),
                Some(Event::Link(name, up)) if name == peer.name => {
                    tracing::debug!(peer = %name, up, "display sees the link");
                    if up { grace = None; }
                    host.link(up)
                }
                Some(Event::Msg(name, Msg::DisplayReady(answer))) if name == peer.name => {
                    if let Err(why) = &answer {
                        warn!(peer = %name, "cannot show the display: {why}");
                        say(format!("⚠ {name}: {why}"));
                    }
                    host.ready(answer)
                }
                Some(_) => vec![],
            },
            _ = async { grace.as_mut().unwrap().await }, if grace.is_some() => {
                grace = None;
                host.grace_over()
            }
        };
        for step in steps {
            match step {
                Step::Ask(show) => {
                    info!(peer = %peer.name, ?show, "asking the peer about the display");
                    links.send(&peer.name, Msg::Display(show));
                }
                Step::StopLater => grace = Some(Box::pin(tokio::time::sleep(GRACE))),
                Step::Stop => {
                    if let Some((capture, sender)) = running.take() {
                        sender.abort();
                        drop(capture);
                        info!("extended display off");
                    }
                    if !status.snapshot().display.starts_with('⚠') {
                        say(String::new());
                    }
                }
                Step::Start(screen) => {
                    let slot = Arc::new(Slot::default());
                    // Read now, not at startup: the Arrange window may have
                    // moved the peer since.
                    let at = crate::load(&config_path)
                        .ok()
                        .and_then(|c| c.peers.into_iter().find(|p| p.name == peer.name))
                        .unwrap_or_else(|| peer.clone());
                    let place = crate::display_mac::Place { side: at.position, display: at.display };
                    match crate::display_mac::Capture::start(screen, place, settings.hidpi, bitrate, slot.clone()) {
                        Ok(capture) => {
                            info!(w = screen.w, h = screen.h, hz = screen.hz, "extended display on");
                            say(format!("Extended: {}×{} at {} Hz", screen.w, screen.h, screen.hz));
                            let sender = tokio::spawn(stream(net.clone(), peer.clone(), slot));
                            running = Some((capture, sender));
                        }
                        Err(e) => {
                            warn!("cannot start the extended display: {e:#}");
                            say(format!("⚠ {e}"));
                            host.running = None;
                            links.send(&peer.name, Msg::Display(Show::Off));
                        }
                    }
                }
            }
        }
    }
}

/// Keep a video connection to the peer up and feed it frames. Redials on its
/// own, so a link that comes back within the grace period just carries on.
#[cfg(target_os = "macos")]
async fn stream(net: crate::link::Net, peer: crate::Peer, slot: Arc<Slot>) {
    loop {
        match net.dial_video(&peer).await {
            Ok(conn) => {
                let sent = async {
                    let mut send = conn.open_uni().await?;
                    slot.restart();
                    let mut count = 0u64;
                    loop {
                        let frame = slot.pop().await;
                        count += 1;
                        if count == 1 || count.is_multiple_of(600) {
                            tracing::info!(count, key = frame.key, bytes = frame.data.len(), "video frames sent");
                        }
                        write_frame(&mut send, &frame).await?;
                    }
                };
                let result: Result<()> = sent.await;
                if let Err(e) = result {
                    tracing::debug!(peer = %peer.name, "video stream ended: {e}");
                }
            }
            Err(e) => tracing::debug!(peer = %peer.name, "cannot open video: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// The client's side: shows whatever the server streams, where it is told to.
#[cfg(not(target_os = "macos"))]
pub async fn show(
    links: Arc<crate::link::Links>,
    status: Arc<crate::status::Status>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    use crate::display_linux::{Player, Screen};
    use tracing::{info, warn};

    // Which workspace to go back to, set while the display is up.
    let mut screen: Option<Screen> = None;
    let mut player: Option<(Arc<Player>, tokio::task::JoinHandle<()>)> = None;
    let stop = |screen: &mut Option<Screen>, player: &mut Option<(Arc<Player>, tokio::task::JoinHandle<()>)>| {
        if let Some((_, receiver)) = player.take() {
            receiver.abort();
        }
        if let Some(screen) = screen.take() {
            screen.restore();
            info!("extended display off");
        }
        status.set(|s| s.display = String::new());
    };

    while let Some(event) = events.recv().await {
        match event {
            Event::Msg(peer, Msg::Display(Show::On { workspace })) => {
                info!(%peer, workspace, "asked to show the extended display");
                let answer = Screen::prepare(workspace);
                let reply = answer.as_ref().map(|s| s.mode).map_err(|e| e.to_string());
                if let Err(why) = &reply {
                    warn!(%peer, "cannot show the extended display: {why}");
                }
                links.send(&peer, Msg::DisplayReady(reply));
                if let Ok(prepared) = answer {
                    // A repeat request keeps the workspace to go back to from
                    // the first one, not the display's own workspace.
                    if screen.is_none() {
                        screen = Some(prepared);
                    }
                }
            }
            Event::Msg(_, Msg::Display(Show::Off)) | Event::Link(_, false) => stop(&mut screen, &mut player),
            Event::Video(peer, conn) => {
                let Some(at) = screen.as_ref() else {
                    warn!(%peer, "video arrived without being asked for, ignoring it");
                    continue;
                };
                if let Some((_, old)) = player.take() {
                    old.abort();
                }
                match Player::start(&at.output) {
                    Ok(started) => {
                        at.enter();
                        let started = Arc::new(started);
                        let feeding = started.clone();
                        let receiver = tokio::spawn(async move {
                            let result: Result<()> = async {
                                let mut recv = conn.accept_uni().await?;
                                let mut count = 0u64;
                                loop {
                                    let frame = read_frame(&mut recv).await?;
                                    count += 1;
                                    if count == 1 || count.is_multiple_of(600) {
                                        info!(count, key = frame.key, bytes = frame.data.len(), "video frames received");
                                    }
                                    feeding.push(frame.data);
                                }
                            }
                            .await;
                            if let Err(e) = result {
                                tracing::debug!("video stream ended: {e}");
                            }
                        });
                        status.set(|s| s.display = format!("Showing {peer}"));
                        info!(%peer, "showing the extended display");
                        player = Some((started, receiver));
                    }
                    Err(e) => warn!(%peer, "cannot start the video pipeline: {e:#}"),
                }
            }
            _ => {}
        }
    }
    stop(&mut screen, &mut player);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two real endpoints over loopback: the video connection must be told
    /// apart from an input link, and frames must come out as they went in,
    /// boundaries and keyframe flags included.
    #[tokio::test(flavor = "multi_thread")]
    async fn frames_cross_a_video_connection_intact() -> Result<()> {
        use crate::link::{endpoint, Identity};
        use crate::{Config, Peer, Role, Side};

        let dir = std::env::temp_dir().join(format!("glide-video-{}", std::process::id()));
        let (id_mac, id_linux) = (Identity::load_or_create(&dir.join("mac"))?, Identity::load_or_create(&dir.join("linux"))?);
        let config = |peer: &Identity, addrs| Config {
            name: "me".into(),
            role: Role::Server,
            listen: "127.0.0.1:0".parse().unwrap(),
            display: Settings::default(),
            peers: vec![Peer { name: "peer".into(), addrs, fingerprint: peer.fingerprint(), position: Side::Right, display: None }],
        };
        let receiver = endpoint(&config(&id_mac, vec![]), &id_linux)?;
        let addr = receiver.endpoint.local_addr()?;
        let sender = endpoint(&config(&id_linux, vec![addr]), &id_mac)?;
        let peer = config(&id_linux, vec![addr]).peers.remove(0);

        let frames: Vec<Frame> = (0..50u8)
            .map(|i| Frame { key: i % 10 == 0, data: vec![i; 1 + i as usize * 4000] })
            .collect();
        let sent = frames.iter().map(|f| Frame { key: f.key, data: f.data.clone() }).collect::<Vec<_>>();
        let (accepted, dialed) = tokio::join!(
            async { receiver.endpoint.accept().await.unwrap().await.unwrap() },
            sender.dial_video(&peer),
        );
        let dialed = dialed?;
        assert!(crate::link::is_video(&accepted), "the accept loop must route this to the display");
        tokio::spawn(async move {
            let mut send = dialed.open_uni().await.unwrap();
            for frame in &sent {
                write_frame(&mut send, frame).await.unwrap();
            }
            send.finish().unwrap();
            dialed.closed().await;
        });
        let mut recv = accepted.accept_uni().await?;
        for expected in &frames {
            assert_eq!(&read_frame(&mut recv).await?, expected);
        }
        accepted.close(0u32.into(), b"done");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    const FHD: ScreenMode = ScreenMode { w: 1920, h: 1080, hz: 120 };

    fn frame(key: bool, n: u8) -> Frame {
        Frame { key, data: vec![n] }
    }

    fn host(mode: Mode) -> Host {
        Host { mode, linked: false, workspace: 9, running: None }
    }

    #[tokio::test]
    async fn a_prompt_sender_gets_every_frame() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        assert_eq!(slot.pop().await, frame(true, 1));
        slot.push(frame(false, 2));
        assert_eq!(slot.pop().await, frame(false, 2));
        assert!(!slot.take_key_request());
    }

    /// The rule that keeps latency flat: a sender a frame behind loses both
    /// frames and everything up to the next keyframe, and asks for one.
    #[tokio::test]
    async fn a_slow_sender_drops_to_the_next_keyframe() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        slot.push(frame(false, 2)); // 1 was never picked up
        assert!(slot.take_key_request(), "the encoder must be asked for a keyframe");
        slot.push(frame(false, 3));
        assert!(slot.state.lock().unwrap().frame.is_none(), "a frame that refers to a dropped one must not go out");
        slot.push(frame(true, 4));
        assert_eq!(slot.pop().await, frame(true, 4));
        slot.push(frame(false, 5));
        assert_eq!(slot.pop().await, frame(false, 5), "back to normal after the keyframe");
    }

    #[tokio::test]
    async fn a_keyframe_behind_a_stale_frame_still_goes_out() {
        let slot = Slot::default();
        slot.push(frame(false, 1));
        slot.push(frame(true, 2));
        assert_eq!(slot.pop().await, frame(true, 2));
    }

    #[test]
    fn a_new_receiver_starts_from_a_keyframe() {
        let slot = Slot::default();
        slot.push(frame(true, 1));
        slot.restart();
        assert!(slot.take_key_request());
        slot.push(frame(false, 2));
        assert!(slot.state.lock().unwrap().frame.is_none());
    }

    #[test]
    fn extend_asks_the_client_then_starts_at_its_mode() {
        let mut h = host(Mode::Control);
        assert_eq!(h.link(true), vec![], "control mode asks for nothing on connect");
        assert_eq!(h.mode(Mode::Extend), vec![Step::Ask(Show::On { workspace: 9 })]);
        assert_eq!(h.ready(Ok(FHD)), vec![Step::Start(FHD)]);
        assert_eq!(h.mode(Mode::Control), vec![Step::Stop, Step::Ask(Show::Off)]);
    }

    #[test]
    fn switching_modes_offline_waits_for_the_link() {
        let mut h = host(Mode::Control);
        assert_eq!(h.mode(Mode::Extend), vec![], "nobody to ask yet");
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })]);
    }

    /// A blip must not tear down the display and scatter its windows.
    #[test]
    fn a_short_link_drop_keeps_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        assert_eq!(h.link(false), vec![Step::StopLater]);
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })]);
        assert_eq!(h.ready(Ok(FHD)), vec![], "same monitor: nothing to rebuild");
        assert_eq!(h.grace_over(), vec![], "a stale timer must not stop a live display");
    }

    #[test]
    fn a_long_link_drop_removes_the_display_and_comes_back_on_reconnect() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        h.link(false);
        assert_eq!(h.grace_over(), vec![Step::Stop]);
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })], "the saved mode is applied again");
        assert_eq!(h.ready(Ok(FHD)), vec![Step::Start(FHD)]);
    }

    #[test]
    fn a_new_monitor_mode_rebuilds_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        let slower = ScreenMode { hz: 60, ..FHD };
        assert_eq!(h.ready(Ok(slower)), vec![Step::Stop, Step::Start(slower)]);
    }

    #[test]
    fn a_client_that_cannot_show_it_stops_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        assert_eq!(h.ready(Err("install gst-plugin-va".into())), vec![]);
        h.ready(Ok(FHD));
        assert_eq!(h.ready(Err("gone".into())), vec![Step::Stop]);
        assert_eq!(host(Mode::Control).ready(Ok(FHD)), vec![], "an answer to a stale request starts nothing");
    }
}
