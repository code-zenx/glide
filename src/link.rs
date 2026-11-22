//! The encrypted link between two machines: a QUIC connection whose peer is
//! identified by a pinned certificate fingerprint, plus round-trip timing.
//!
//! Three channels share the connection, and keeping them apart is what keeps
//! the cursor smooth:
//!
//! - **datagrams** for pointer motion and pings, worthless once late;
//! - **one reliable stream** for keys, buttons and handover, all tiny;
//! - **a stream of its own per clipboard item**, because a sixteen-megabyte
//!   image sharing a lane with a key press delays the key press by the whole
//!   image.
//!
//! An extended display's video gets a **separate connection** altogether, told
//! apart by its ALPN. A connection has one congestion window and one send
//! buffer; sharing them with thirty megabits a second of video would put every
//! key press in the queue behind it. View Screen gets one of its own the same
//! way, with the roles reversed: the Mac dials, the peer sends.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use quinn::{Connection, Endpoint};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{debug, info, warn};

use crate::proto::Msg;
use crate::status::Status;
use crate::{Config, Peer};

mod links;
mod trust;
mod wire;
#[cfg(test)]
mod tests;

pub use links::Links;
pub use trust::{endpoint, pair_endpoint, Identity, Net};
pub(crate) use trust::peer_fingerprint;
#[cfg(test)]
pub(crate) use trust::loopback;
// Outside the link, only the Mac viewer reads frames and only the Linux
// sender writes them; tests do both.
#[cfg(any(test, target_os = "macos"))]
pub(crate) use wire::read_frame;
#[cfg(any(test, not(target_os = "macos")))]
pub(crate) use wire::write_frame;
use trust::{hex, parse_fingerprint};
use wire::{bulk, datagrams, frames, ping_loop, write_loop};

const ALPN: &[u8] = b"glide/0";
const VIDEO_ALPN: &[u8] = b"glide-video/0";
/// View Screen: the Mac dials the peer, and the peer streams its screen and
/// sound back. Bumped to /1 when sound gained Opus: a build that
/// cannot read it then fails at connect, not mid-stream.
const VIEW_ALPN: &[u8] = b"glide-view/1";
/// `glide join`: a machine that is not trusted yet, presenting an invite.
const PAIR_ALPN: &[u8] = b"glide-pair/0";
/// Close code for "this server switched you off". The client stops dialing
/// and shows it; the server dials again when switched back on.
pub const OFF: u32 = 1;
/// Close code for "this server forgot you".
pub const FORGOTTEN: u32 = 2;
/// Close code for "both of us dialed; the link the server dialed is kept".
const REPLACED: u32 = 3;
/// How often a client the server switched off checks whether it is back on,
/// in case the server cannot reach it (new DHCP lease, inbound port blocked).
const OFF_PROBE: Duration = Duration::from_secs(60);
const DIAL_EVERY: Duration = Duration::from_secs(2);
/// How much unacknowledged video may sit in the send buffer: a keyframe and a
/// little. More only lets latency pile up inside quinn, out of sight of the
/// frame dropping that is meant to stop exactly that. A View Screen peer sends
/// on a connection it accepted, so it sets this on the connection itself.
pub(crate) const VIDEO_SEND_WINDOW: u64 = 1 << 20;
const PING_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(10);
/// Enough samples for a meaningful p99 without keeping history forever.
const SAMPLE_WINDOW: usize = 200;
/// The reliable stream carries a key event, a handover or a screen list and
/// nothing else, so anything larger is a desynchronised stream, not a message.
const MAX_FRAME: usize = 1 << 16;
/// A clipboard item, which arrives on a stream of its own.
const MAX_BULK: usize = crate::clipboard::MAX_IMAGE + (1 << 16);

/// Listen for peers, dial the ones we are responsible for, and keep both going.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Config,
    config_path: PathBuf,
    net: Net,
    links: Arc<Links>,
    status: Arc<Status>,
    to_session: UnboundedSender<(String, Msg)>,
    link_state: UnboundedSender<(String, bool)>,
    video_in: UnboundedSender<(String, Connection)>,
    control: UnboundedSender<crate::input::Control>,
) -> Result<()> {
    let endpoint = net.endpoint;
    info!(listen = %config.listen, name = %config.name, role = ?config.role, "glide up");

    // Both machines dial each other; whichever connection lands first is the
    // one that gets used. One end being firewalled costs nothing this way.
    tokio::spawn(dial_loop(endpoint.clone(), links.clone(), status.clone(), to_session.clone(), link_state.clone()));

    let server = config.role == crate::Role::Server;
    while let Some(incoming) = endpoint.accept().await {
        let (links, to_session, config_path) = (links.clone(), to_session.clone(), config_path.clone());
        let (status, link_state, video_in, control) = (status.clone(), link_state.clone(), video_in.clone(), control.clone());
        let me = config.name.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => return warn!("rejected connection: {e}"),
            };
            if alpn(&conn).as_deref() == Some(PAIR_ALPN) {
                if server {
                    crate::pair::accept(conn, &me, &links, &config_path, &control, &status).await;
                } else {
                    conn.close(0u32.into(), b"not a server");
                }
                return;
            }
            // The pinned certificate already proved who this is; the name just
            // tells us which screen edge the events belong to. A certificate
            // let through only because an invite is open stops here.
            let Some(name) = identify(&conn, &links.peers()) else {
                return warn!(addr = %conn.remote_address(), "connected peer is not in the config");
            };
            // Both kinds of media connection go the same way; `is_view`
            // tells them apart where they arrive.
            if matches!(alpn(&conn).as_deref(), Some(VIDEO_ALPN | VIEW_ALPN)) {
                info!(peer = %name, view = is_view(&conn), "media connection");
                let _ = video_in.send((name, conn));
                return;
            }
            // Wherever it came from is where it can be reached now, which
            // keeps a peer on a DHCP lease reachable after the lease moves.
            if links.note_addr(&name, conn.remote_address()) {
                info!(peer = %name, addr = %conn.remote_address(), "peer has a new address");
                if let Err(e) = crate::set_peer_addrs(&config_path, &name, links.peer(&name).map(|p| p.addrs).unwrap_or_default()) {
                    warn!(peer = %name, "cannot save its address: {e}");
                }
            }
            info!(peer = %name, addr = %conn.remote_address(), "peer connected");
            if let Err(e) = serve(conn.clone(), &name, false, &links, &status, &to_session, &link_state).await {
                warn!(peer = %name, "link closed: {e}");
            }
            check_told_off(&conn, &name, &links, &status);
        });
    }
    endpoint.wait_idle().await;
    Ok(())
}

fn alpn(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|data| data.protocol)
}

#[cfg(test)]
pub(crate) fn is_video(conn: &Connection) -> bool {
    alpn(conn).as_deref() == Some(VIDEO_ALPN)
}

/// A View Screen connection, as opposed to an extended display's.
pub(crate) fn is_view(conn: &Connection) -> bool {
    alpn(conn).as_deref() == Some(VIEW_ALPN)
}

fn identify(conn: &Connection, peers: &[Peer]) -> Option<String> {
    let seen = peer_fingerprint(conn)?;
    peers
        .iter()
        .find(|p| parse_fingerprint(&p.fingerprint).is_ok_and(|f| hex(&f) == seen))
        .map(|p| p.name.clone())
}

/// A client whose server closed the link with [`OFF`] is switched off: it says
/// so and stops dialing until the server comes back for it.
fn check_told_off(conn: &Connection, peer: &str, links: &Links, status: &Status) {
    if links.server {
        return;
    }
    let off = matches!(conn.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(c)) if c.error_code == quinn::VarInt::from_u32(OFF));
    if off && !links.is_off(peer) {
        info!(%peer, "turned off by {peer}");
    }
    if off {
        links.set_off(peer, true);
        status.set_peer(peer, |p| p.mode = "off".into());
    }
}

/// One loop for every peer, reading the roster each round, so a paired device
/// is dialed within seconds and a forgotten or switched-off one stops being.
async fn dial_loop(
    endpoint: Endpoint,
    links: Arc<Links>,
    status: Arc<Status>,
    to_session: UnboundedSender<(String, Msg)>,
    link_state: UnboundedSender<(String, bool)>,
) {
    let busy: Arc<Mutex<HashSet<String>>> = Arc::default();
    // Say why once, loudly: an unreachable peer is usually a firewall or a
    // missing Local Network permission, and silence there is hard to debug.
    let complained: Arc<Mutex<HashSet<String>>> = Arc::default();
    let mut probed: HashMap<String, Instant> = HashMap::new();
    loop {
        for peer in links.peers() {
            // The peer may have dialed us first. Leave that link alone instead
            // of building a second one every couple of seconds.
            if links.connected(&peer.name) || busy.lock().unwrap().contains(&peer.name) {
                continue;
            }
            if links.is_off(&peer.name) {
                if links.server {
                    continue;
                }
                if probed.get(&peer.name).is_some_and(|t| t.elapsed() < OFF_PROBE) {
                    continue;
                }
                probed.insert(peer.name.clone(), Instant::now());
            }
            busy.lock().unwrap().insert(peer.name.clone());
            let (endpoint, links, status) = (endpoint.clone(), links.clone(), status.clone());
            let (to_session, link_state, busy, complained) = (to_session.clone(), link_state.clone(), busy.clone(), complained.clone());
            tokio::spawn(async move {
                match dial(&endpoint, &peer).await {
                    Ok(conn) => {
                        info!(peer = %peer.name, addr = %conn.remote_address(), "connected");
                        complained.lock().unwrap().remove(&peer.name);
                        if let Err(e) = serve(conn.clone(), &peer.name, true, &links, &status, &to_session, &link_state).await {
                            warn!(peer = %peer.name, "link closed: {e}");
                        }
                        check_told_off(&conn, &peer.name, &links, &status);
                    }
                    Err(e) if complained.lock().unwrap().insert(peer.name.clone()) => {
                        warn!(peer = %peer.name, "cannot reach: {e}");
                    }
                    Err(e) => debug!(peer = %peer.name, "cannot reach: {e}"),
                }
                busy.lock().unwrap().remove(&peer.name);
            });
        }
        tokio::time::sleep(DIAL_EVERY).await;
    }
}

/// Try every address the peer lists and keep the one that answers first, so a
/// wired link is used when it is up and Wi-Fi takes over when it is not.
async fn dial(endpoint: &Endpoint, peer: &Peer) -> Result<Connection> {
    let mut last = anyhow!("peer {} has no addresses", peer.name);
    for addr in &peer.addrs {
        match endpoint.connect(*addr, "glide")?.await {
            Ok(conn) => return Ok(conn),
            Err(e) => last = anyhow!("{addr}: {e}"),
        }
    }
    Err(last)
}

/// Run one connection until it dies.
async fn serve(
    conn: Connection,
    peer: &str,
    dialer: bool,
    links: &Links,
    status: &Status,
    to_session: &UnboundedSender<(String, Msg)>,
    link_state: &UnboundedSender<(String, bool)>,
) -> Result<()> {
    // Switched off on this server, or forgotten while the connection was
    // being made: say so and hang up.
    if links.server && links.is_off(peer) {
        conn.close(OFF.into(), b"turned off");
        return Ok(());
    }
    if links.peer(peer).is_none() {
        conn.close(FORGOTTEN.into(), b"forgotten");
        return Ok(());
    }
    // open_bi only reaches the peer once something is written, so the dialer
    // sends an empty frame to get the stream accepted on the other side.
    let (mut send, recv) = if dialer {
        let (mut send, recv) = conn.open_bi().await?;
        send.write_all(&0u32.to_be_bytes()).await?;
        (send, recv)
    } else {
        conn.accept_bi().await?
    };
    let _ = send.flush().await;

    let (tx, rx) = unbounded_channel::<Msg>();
    if !links.try_register(peer, tx.clone(), dialer == links.server) {
        debug!(%peer, "already linked, dropping the duplicate connection");
        return Ok(());
    }
    if let Some(replaced) = links.conns.lock().unwrap().insert(peer.to_string(), conn.clone()) {
        debug!(%peer, "both ends dialed; keeping the link the server dialed");
        replaced.close(REPLACED.into(), b"replaced");
    }
    if !links.server {
        // The server came back for us, so it has switched us on again.
        links.set_off(peer, false);
    }

    // The session arms its screen edge on this and drops it again below. An
    // edge armed with no link behind it grabs the cursor and sends it nowhere.
    let _ = link_state.send((peer.to_string(), true));

    let started = Instant::now();
    let writer = tokio::spawn(write_loop(conn.clone(), send, rx));
    let pinger = tokio::spawn(ping_loop(tx.clone(), started));
    status.set_peer(peer, |p| p.mode = "local".into());
    // Tell the peer what this machine has attached, so its arrange window can
    // draw real screens instead of a placeholder.
    let _ = tx.send(Msg::Screens(crate::screens::local()));

    // Each reader runs its own loop, so a half-read frame is never abandoned
    // part way: `RecvStream::read_exact` is not cancel-safe, and dropping it
    // mid-frame swallows bytes and desynchronises the stream for good. The
    // first one to fail ends the link, and cancelling the others then costs
    // nothing because everything is being torn down anyway.
    let result = tokio::select! {
        result = frames(recv, peer, to_session) => result,
        result = bulk(&conn, peer, to_session) => result,
        result = datagrams(&conn, peer, started, &tx, status, to_session) => result,
    };

    pinger.abort();
    writer.abort();
    // A link replaced by the one the server dialed leaves the peer connected.
    if !links.unregister(peer, &tx) {
        return result;
    }
    status.offline(peer);
    if links.is_off(peer) {
        status.set_peer(peer, |p| p.mode = "off".into());
    }
    let _ = link_state.send((peer.to_string(), false));
    // Whoever had the cursor, they no longer do; let the session put input back
    // where it belongs rather than leaving it pointed at a dead link.
    let _ = to_session.send((peer.to_string(), Msg::Leave));
    result
}
