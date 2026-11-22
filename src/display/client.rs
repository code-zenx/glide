//! The client's side of an extended display: show what the server streams,
//! fullscreen on the workspace it names.

use std::sync::Arc;

use anyhow::Result;
use quinn::Connection;
use tracing::{info, warn};

use super::{recv_frames, Event};
use crate::display_linux::{Player, Screen};
use crate::proto::{Msg, Show};

/// The client's side: shows whatever the server streams, where it is told to.
#[cfg(not(target_os = "macos"))]
pub async fn show(
    links: Arc<crate::link::Links>,
    status: Arc<crate::status::Status>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    // Which workspace to go back to, set while the display is up.
    let mut screen: Option<Screen> = None;
    let mut player: Option<(Arc<Player>, tokio::task::JoinHandle<()>)> = None;

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
            Event::Msg(_, Msg::Display(Show::Off)) | Event::Link(_, false) => stop(&mut screen, &mut player, &status),
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
                        let receiver = tokio::spawn(receive(conn, feeding));
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
    stop(&mut screen, &mut player, &status);
}

/// Stop showing the display: drop its stream and give the workspace back.
fn stop(
    screen: &mut Option<Screen>,
    player: &mut Option<(Arc<Player>, tokio::task::JoinHandle<()>)>,
    status: &crate::status::Status,
) {
    if let Some((_, receiver)) = player.take() {
        receiver.abort();
    }
    if let Some(screen) = screen.take() {
        screen.restore();
        info!("extended display off");
    }
    status.set(|s| s.display = String::new());
}

/// Feed the player every frame the server sends on this connection.
async fn receive(conn: Connection, feeding: Arc<Player>) {
    let result: Result<()> = async {
        let mut recv = conn.accept_uni().await?;
        recv_frames(&mut recv, |frame| feeding.push(frame.data)).await
    }
    .await;
    if let Err(e) = result {
        tracing::debug!("video stream ended: {e}");
    }
}
