//! The server's side of an extended display: the task that owns the virtual
//! display, and the stream that carries it to the peer.

use std::sync::Arc;

use anyhow::Result;

use super::rules::{Host, Step, GRACE};
use super::{send_frames, Event, Mode, Settings, Slot};
use crate::proto::{Msg, Show};

/// The server's side: owns the virtual display and streams it.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub async fn host(
    settings: Settings,
    config_path: std::path::PathBuf,
    net: crate::link::Net,
    mut peer: Option<crate::Peer>,
    links: Arc<crate::link::Links>,
    status: Arc<crate::status::Status>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    use tracing::{info, warn};

    let mode = if peer.is_some() { Mode::Extend } else { Mode::Control };
    let mut host = Host { mode, linked: false, workspace: settings.workspace, running: None };
    // A new target is switched to in two steps: the old one is stopped and
    // told so under its own name, then the new one is asked.
    let mut next: Option<Option<crate::Peer>> = None;
    let named = |peer: &Option<crate::Peer>, name: &str| peer.as_ref().is_some_and(|p| p.name == name);
    let bitrate = settings.bitrate_mbps.saturating_mul(1_000_000);
    let mut running: Option<(crate::display_mac::Capture, tokio::task::JoinHandle<()>)> = None;
    let mut grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let say = |text: String| status.set(|s| s.display = text);
    say(String::new());

    loop {
        let mut steps = tokio::select! {
            event = events.recv() => match event {
                None => break,
                Some(Event::Target(target)) => {
                    if target.as_ref().map(|t| &t.name) == peer.as_ref().map(|p| &p.name) {
                        vec![]
                    } else {
                        next = Some(target);
                        host.mode(Mode::Control)
                    }
                }
                Some(Event::Link(name, up)) if named(&peer, &name) => {
                    tracing::debug!(peer = %name, up, "display sees the link");
                    if up { grace = None; }
                    host.link(up)
                }
                Some(Event::Msg(name, Msg::DisplayReady(answer))) if named(&peer, &name) => {
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
        loop {
        for step in steps {
            let target = peer.clone().unwrap_or_else(|| crate::Peer {
                name: String::new(),
                addrs: vec![],
                fingerprint: String::new(),
                position: crate::Side::Top,
                display: None,
                enabled: false,
                control: false,
            });
            match step {
                Step::Ask(_) | Step::Start(_) if peer.is_none() => {}
                Step::Ask(show) => {
                    info!(peer = %target.name, ?show, "asking the peer about the display");
                    links.send(&target.name, Msg::Display(show));
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
                        .and_then(|c| c.peers.into_iter().find(|p| p.name == target.name))
                        .unwrap_or_else(|| target.clone());
                    let place = crate::display_mac::Place { side: at.position, display: at.display };
                    match crate::display_mac::Capture::start(screen, place, settings.hidpi, bitrate, slot.clone()) {
                        Ok(capture) => {
                            info!(w = screen.w, h = screen.h, hz = screen.hz, "extended display on");
                            say(format!("Extended: {}×{} at {} Hz", screen.w, screen.h, screen.hz));
                            let sender = tokio::spawn(stream(net.clone(), target.clone(), slot));
                            running = Some((capture, sender));
                        }
                        Err(e) => {
                            warn!("cannot start the extended display: {e:#}");
                            say(format!("⚠ {e}"));
                            host.running = None;
                            links.send(&target.name, Msg::Display(Show::Off));
                        }
                    }
                }
            }
        }
        // The old target is stopped; now switch and ask the new one.
        let Some(target) = next.take() else { break };
        peer = target;
        grace = None;
        host.linked = peer.as_ref().is_some_and(|p| links.connected(&p.name));
        if !status.snapshot().display.starts_with('⚠') || peer.is_none() {
            say(String::new());
        }
        steps = if peer.is_some() { host.mode(Mode::Extend) } else { vec![] };
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
                    send_frames(&mut send, &slot).await
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
