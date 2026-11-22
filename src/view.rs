//! View Screen: a Linux machine's screen and sound in a fullscreen window on
//! the Mac, with the Mac's keyboard and mouse driving it while the user is in
//! that window — Moonlight and Sunshine, inside glide.
//!
//! The Mac asks with `Msg::View(true)`; the peer answers `ViewReady` with the
//! mode of the screen it will send. The Mac then dials a connection of its
//! own (ALPN `glide-view/0`, see `link.rs`), and the peer opens two streams
//! back on it: video, framed exactly like an extended display's
//! (`display::send_frames`), and sound, as `[u32 len][packet]` frames of Opus,
//! or of raw PCM where the peer has no Opus encoder. Each stream starts with
//! one byte saying which it is, so neither end depends on the order the other
//! opened them in, and the Mac knows how to decode the sound.
//!
//! The window's own input is `view_input_mac.rs`'s business. The session
//! (`input.rs`) decides when a view starts and ends: from the menu, `glide ctl`,
//! the window's close button, or the device going away. Input coming home does
//! not end it.

use std::sync::Arc;

use anyhow::{bail, Result};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use crate::link::Links;
use crate::proto::Msg;

const VIDEO: u8 = b'v';
/// Sound as raw 16-bit stereo PCM at 48 kHz.
const SOUND: u8 = b'a';
/// Sound as Opus, one packet per frame.
const OPUS: u8 = b'o';

/// Everything the view side hears about, from the session and the network.
pub enum Event {
    /// Show this device's screen now, or none.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Target(Option<crate::Peer>),
    /// Only Linux reads it, and only the flag.
    #[allow(dead_code)]
    Link(String, bool),
    Msg(String, Msg),
    /// The Mac dialed this machine for its screen.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Conn(String, Connection),
}

/// Whether this machine can send its screen. Only a Linux client can.
pub fn can_share() -> bool {
    #[cfg(target_os = "macos")]
    {
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::view_linux::can_capture()
    }
}

/// The peer's end: a stream back to the Mac, saying what it carries.
#[cfg_attr(target_os = "macos", allow(dead_code))]
async fn open_stream(conn: &Connection, kind: u8) -> Result<SendStream> {
    let mut send = conn.open_uni().await?;
    send.write_all(&[kind]).await?;
    Ok(send)
}

/// The Mac's end: the peer's two streams, as (video, (sound, whether the
/// sound is Opus rather than PCM)).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) async fn accept_streams(conn: &Connection) -> Result<(RecvStream, (RecvStream, bool))> {
    let (mut video, mut sound) = (None, None);
    while video.is_none() || sound.is_none() {
        let mut recv = conn.accept_uni().await?;
        let mut kind = [0u8];
        recv.read_exact(&mut kind).await?;
        match kind[0] {
            VIDEO => video = Some(recv),
            SOUND => sound = Some((recv, false)),
            OPUS => sound = Some((recv, true)),
            other => bail!("unknown view stream {other}"),
        }
    }
    Ok(video
        .zip(sound)
        .expect("the loop ends only once both are here"))
}

/// What the Mac does next, decided apart from the frameworks so the rules can
/// be tested without a screen.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq)]
enum Step {
    /// Tell this device to start (true) or stop sending its screen.
    Ask(String, bool),
    Start(crate::proto::ScreenMode),
    Stop,
    /// The device cannot be viewed, for this reason.
    Fail(String),
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct Viewer {
    target: Option<String>,
    running: bool,
}

#[cfg(any(target_os = "macos", test))]
impl Viewer {
    fn target(&mut self, name: Option<String>) -> Vec<Step> {
        if name == self.target {
            return vec![];
        }
        let mut steps = if std::mem::take(&mut self.running) {
            vec![Step::Stop]
        } else {
            vec![]
        };
        steps.extend(self.target.take().map(|old| Step::Ask(old, false)));
        steps.extend(name.clone().map(|new| Step::Ask(new, true)));
        self.target = name;
        steps
    }

    /// An answer from a device not asked, or asked again, starts nothing.
    fn ready(&mut self, from: &str, answer: Result<crate::proto::ScreenMode, String>) -> Vec<Step> {
        if self.target.as_deref() != Some(from) || self.running {
            return vec![];
        }
        match answer {
            Ok(mode) => {
                self.running = true;
                vec![Step::Start(mode)]
            }
            Err(why) => vec![Step::Fail(why)],
        }
    }
}

/// The Mac's side: asks the device for its screen and shows it.
#[cfg(target_os = "macos")]
pub async fn watch(
    net: crate::link::Net,
    links: Arc<Links>,
    control: tokio::sync::mpsc::UnboundedSender<crate::input::Control>,
    mut events: UnboundedReceiver<Event>,
) {
    let mut viewer = Viewer::default();
    let mut watching = Watching {
        net,
        links,
        control,
        peer: None,
        task: None,
    };
    while let Some(event) = events.recv().await {
        let steps = match event {
            Event::Target(target) => {
                let steps = viewer.target(target.as_ref().map(|p| p.name.clone()));
                watching.peer = target;
                steps
            }
            Event::Msg(name, Msg::ViewReady(answer)) => viewer.ready(&name, answer),
            _ => vec![],
        };
        steps.into_iter().for_each(|step| watching.apply(step));
    }
    watching.apply(Step::Stop);
}

/// What carries out the viewer's steps: where to dial, whom to tell, and the
/// task showing the picture.
#[cfg(target_os = "macos")]
struct Watching {
    net: crate::link::Net,
    links: Arc<Links>,
    control: tokio::sync::mpsc::UnboundedSender<crate::input::Control>,
    peer: Option<crate::Peer>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl Watching {
    fn apply(&mut self, step: Step) {
        let name = self.peer.as_ref().map_or(String::new(), |p| p.name.clone());
        match step {
            Step::Ask(to, on) => self.links.send(&to, Msg::View(on)),
            Step::Stop => {
                if let Some(task) = self.task.take() {
                    task.abort();
                    info!(peer = %name, "view closed");
                }
            }
            Step::Start(mode) => {
                info!(peer = %name, w = mode.w, h = mode.h, hz = mode.hz, "viewing");
                if let Some(peer) = self.peer.clone() {
                    let links = self.links.clone();
                    let follow =
                        crate::view_mac::follow(self.net.clone(), links, peer, self.control.clone());
                    self.task = Some(tokio::spawn(follow));
                }
            }
            Step::Fail(why) => {
                warn!(peer = %name, "cannot view it: {why}");
                crate::pair::notify(&format!("Glide cannot view {name}"), &why);
                let _ = self.control.send(crate::input::Control::View(None));
            }
        }
    }
}

/// What runs while the Mac watches: the encoders and the streams they feed.
#[cfg(not(target_os = "macos"))]
struct Sharing {
    tasks: [tokio::task::JoinHandle<()>; 2],
    _video: crate::view_linux::Encoder,
    _sound: Option<crate::view_linux::Sound>,
}

#[cfg(not(target_os = "macos"))]
impl Drop for Sharing {
    fn drop(&mut self) {
        self.tasks.iter().for_each(|task| task.abort());
    }
}

/// The peer's side: sends this machine's screen and sound when asked.
#[cfg(not(target_os = "macos"))]
pub async fn share(links: Arc<Links>, mut events: UnboundedReceiver<Event>) {
    use crate::view_linux::Screen;

    let mut screen: Option<Screen> = None;
    let mut sharing: Option<Sharing> = None;
    while let Some(event) = events.recv().await {
        match event {
            Event::Msg(peer, Msg::View(true)) => {
                // The old one goes first: it may hold the headless monitor
                // and the null sink the new one is about to make.
                sharing = None;
                drop(screen.take());
                let prepared = tokio::task::spawn_blocking(Screen::prepare)
                    .await
                    .unwrap_or_else(|e| Err(e.into()));
                let reply = prepared
                    .as_ref()
                    .map(|s| s.mode)
                    .map_err(|e| format!("{e:#}"));
                match &reply {
                    Ok(_) => info!(%peer, "asked for this screen"),
                    Err(why) => warn!(%peer, "cannot share this screen: {why}"),
                }
                links.send(&peer, Msg::ViewReady(reply));
                screen = prepared.ok();
            }
            Event::Msg(_, Msg::View(false)) | Event::Link(_, false) => {
                if sharing.take().is_some() {
                    info!("screen sharing off");
                }
                screen = None;
            }
            Event::Conn(peer, conn) => {
                sharing = None;
                match screen.as_ref().map(|at| start(at, conn)) {
                    Some(Ok(started)) => {
                        info!(%peer, "sharing this screen");
                        sharing = Some(started);
                    }
                    Some(Err(e)) => warn!(%peer, "cannot start sharing this screen: {e:#}"),
                    None => warn!(%peer, "screen connection without being asked, ignoring it"),
                }
            }
            _ => {}
        }
    }
}

/// A full queue means the network is behind: the newest sound waits for
/// nothing older than about this many chunks.
#[cfg(not(target_os = "macos"))]
const SOUND_QUEUE: usize = 8;

#[cfg(not(target_os = "macos"))]
fn start(screen: &crate::view_linux::Screen, conn: Connection) -> Result<Sharing> {
    use crate::view_linux::{Encoder, Sound};

    conn.set_send_window(crate::link::VIDEO_SEND_WINDOW);
    let slot = Arc::new(crate::display::Slot::default());
    let video = Encoder::start(screen, slot.clone())?;
    let (chunks, queued) = tokio::sync::mpsc::channel(SOUND_QUEUE);
    // Without sound the view still works; the sound stream then just ends.
    let sound = Sound::start(chunks)
        .inspect_err(|e| warn!("sharing the screen without sound: {e:#}"))
        .ok();
    let tag = if sound.as_ref().is_some_and(|s| s.opus) { OPUS } else { SOUND };
    let tasks = [
        tokio::spawn(send_video(conn.clone(), slot)),
        tokio::spawn(send_sound(conn, queued, tag)),
    ];
    Ok(Sharing {
        tasks,
        _video: video,
        _sound: sound,
    })
}

#[cfg(not(target_os = "macos"))]
async fn send_video(conn: Connection, slot: Arc<crate::display::Slot>) {
    let sent: Result<()> = async {
        let mut send = open_stream(&conn, VIDEO).await?;
        crate::display::send_frames(&mut send, &slot).await
    }
    .await;
    if let Err(e) = sent {
        tracing::debug!("screen stream ended: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
async fn send_sound(conn: Connection, mut queued: tokio::sync::mpsc::Receiver<Vec<u8>>, tag: u8) {
    let sent: Result<()> = async {
        let mut send = open_stream(&conn, tag).await?;
        // Sound is tiny next to video: it goes first whenever both wait.
        send.set_priority(1)?;
        while let Some(chunk) = queued.recv().await {
            crate::link::write_frame(&mut send, &chunk).await?;
        }
        send.finish()?;
        Ok(())
    }
    .await;
    if let Err(e) = sent {
        tracing::debug!("sound stream ended: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Frame;
    use crate::proto::ScreenMode;

    const FHD: ScreenMode = ScreenMode {
        w: 1920,
        h: 1080,
        hz: 60,
    };

    #[test]
    fn viewing_asks_the_device_then_starts_at_its_answer() {
        let mut v = Viewer::default();
        assert_eq!(
            v.target(Some("anorak".into())),
            vec![Step::Ask("anorak".into(), true)]
        );
        assert_eq!(v.ready("anorak", Ok(FHD)), vec![Step::Start(FHD)]);
        assert_eq!(
            v.ready("anorak", Ok(FHD)),
            vec![],
            "a repeat answer starts nothing twice"
        );
        assert_eq!(
            v.target(None),
            vec![Step::Stop, Step::Ask("anorak".into(), false)]
        );
        assert_eq!(v.target(None), vec![], "nothing left to stop");
    }

    #[test]
    fn switching_devices_stops_the_old_one_first() {
        let mut v = Viewer::default();
        v.target(Some("anorak".into()));
        v.ready("anorak", Ok(FHD));
        let steps = v.target(Some("nullbyte".into()));
        assert_eq!(
            steps,
            vec![
                Step::Stop,
                Step::Ask("anorak".into(), false),
                Step::Ask("nullbyte".into(), true)
            ]
        );
        assert_eq!(
            v.ready("anorak", Ok(FHD)),
            vec![],
            "the old device's late answer is ignored"
        );
        assert_eq!(v.ready("nullbyte", Ok(FHD)), vec![Step::Start(FHD)]);
    }

    #[test]
    fn a_device_that_cannot_send_is_a_failure_not_a_start() {
        let mut v = Viewer::default();
        v.target(Some("nullbyte".into()));
        let why = "install gst-plugins-good".to_string();
        assert_eq!(v.ready("nullbyte", Err(why.clone())), vec![Step::Fail(why)]);
        assert_eq!(
            v.target(Some("nullbyte".into())),
            vec![],
            "already the target"
        );
        assert_eq!(
            Viewer::default().ready("anorak", Ok(FHD)),
            vec![],
            "nobody asked"
        );
    }

    /// Two real endpoints over loopback: the view connection must be told
    /// apart from the others, and its two streams apart by their tags, not
    /// by the order they were opened in; the sound's tag says how to decode it.
    #[tokio::test(flavor = "multi_thread")]
    async fn screen_and_sound_cross_a_view_connection_apart() -> Result<()> {
        for (tag, opus) in [(SOUND, false), (OPUS, true)] {
            assert_eq!(cross_view_connection(tag).await?, opus, "sound tag {tag}");
        }
        Ok(())
    }

    /// Whether the Mac took the sound for Opus.
    async fn cross_view_connection(sound_tag: u8) -> Result<bool> {
        let name = format!("glide-view-{}-{sound_tag}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        let (linux, mac, peer) = crate::link::loopback(&dir)?;
        let (accepted, dialed) = tokio::join!(
            async { linux.endpoint.accept().await.unwrap().await.unwrap() },
            mac.dial_view(&peer)
        );
        let dialed = dialed?;
        assert!(
            crate::link::is_view(&accepted),
            "the accept loop must route this to the view side"
        );

        let frames: Vec<Frame> = (0..20u8)
            .map(|i| Frame {
                key: i % 5 == 0,
                data: vec![i; 1 + i as usize * 3000],
            })
            .collect();
        let sent: Vec<Frame> = frames
            .iter()
            .map(|f| Frame {
                key: f.key,
                data: f.data.clone(),
            })
            .collect();
        let sending = tokio::spawn(async move {
            let mut sound = open_stream(&accepted, sound_tag).await.unwrap();
            let mut video = open_stream(&accepted, VIDEO).await.unwrap();
            crate::link::write_frame(&mut sound, &[1, 2, 3, 4])
                .await
                .unwrap();
            for frame in &sent {
                crate::display::write_frame(&mut video, frame)
                    .await
                    .unwrap();
            }
            let (_, _) = (sound.finish(), video.finish());
            accepted.closed().await;
        });
        let (mut video, (mut sound, opus)) = accept_streams(&dialed).await?;
        assert_eq!(crate::link::read_frame(&mut sound).await?, [1, 2, 3, 4]);
        let mut got = Vec::new();
        let ended = crate::display::recv_frames(&mut video, |frame| got.push(frame)).await;
        assert!(ended.is_err(), "the stream ends when the sender finishes");
        assert_eq!(got, frames);
        dialed.close(0u32.into(), b"done");
        sending.await?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(opus)
    }
}
