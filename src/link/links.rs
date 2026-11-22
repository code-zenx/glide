//! The live links: which peers are known, which are connected, and which
//! are switched off.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Mutex;

use quinn::Connection;
use tokio::sync::mpsc::UnboundedSender;

use super::trust::parse_fingerprint;
use super::{FORGOTTEN, OFF};
use crate::proto::Msg;
use crate::Peer;

/// A peer's live link: where its messages go, and whether the server dialed it.
struct Live {
    tx: UnboundedSender<Msg>,
    by_server: bool,
}

/// Every peer this machine knows, and the ones it can currently talk to.
#[derive(Default)]
pub struct Links {
    peers: Mutex<HashMap<String, Live>>,
    pub(super) conns: Mutex<HashMap<String, Connection>>,
    /// The configured peers, kept current as devices are paired and forgotten,
    /// so dialing, certificate checks and the clipboard all see the same list.
    roster: Mutex<Vec<Peer>>,
    /// Off. On a server: switched off by its owner, never dialed, refused.
    /// On a client: the server said so; dialed only once a minute to check.
    off: Mutex<HashSet<String>>,
    pub(super) server: bool,
}

impl Links {
    pub fn new(peers: Vec<Peer>, server: bool) -> Self {
        let off = peers.iter().filter(|p| server && !p.enabled).map(|p| p.name.clone()).collect();
        Self { roster: Mutex::new(peers), off: Mutex::new(off), server, ..Default::default() }
    }

    pub fn set_off(&self, peer: &str, off: bool) {
        let mut set = self.off.lock().unwrap();
        if off {
            set.insert(peer.to_string());
        } else {
            set.remove(peer);
        }
    }

    pub fn is_off(&self, peer: &str) -> bool {
        self.off.lock().unwrap().contains(peer)
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.roster.lock().unwrap().clone()
    }

    pub fn peer(&self, name: &str) -> Option<Peer> {
        self.roster.lock().unwrap().iter().find(|p| p.name == name).cloned()
    }

    /// A newly paired device, or a re-paired one replacing its old entry. A
    /// link under the old certificate is closed: it no longer proves anything.
    pub fn upsert(&self, peer: Peer) {
        let mut roster = self.roster.lock().unwrap();
        match roster.iter_mut().find(|p| p.name == peer.name) {
            Some(old) => {
                let rekeyed = old.fingerprint != peer.fingerprint;
                *old = peer.clone();
                drop(roster);
                if rekeyed {
                    self.disconnect(&peer.name, FORGOTTEN);
                }
            }
            None => roster.push(peer),
        }
    }

    pub fn forget(&self, name: &str) {
        self.roster.lock().unwrap().retain(|p| p.name != name);
        self.set_off(name, false);
        self.disconnect(name, FORGOTTEN);
    }

    /// Close the link to a peer now, telling it why.
    pub fn disconnect(&self, name: &str, code: u32) {
        if let Some(conn) = self.conns.lock().unwrap().remove(name) {
            conn.close(code.into(), if code == OFF { b"turned off" } else { b"forgotten" });
        }
    }

    /// Put the address a peer just connected from first in its list. True when
    /// that changed anything, so the caller can save it.
    pub(super) fn note_addr(&self, name: &str, addr: SocketAddr) -> bool {
        let mut roster = self.roster.lock().unwrap();
        let Some(peer) = roster.iter_mut().find(|p| p.name == name) else { return false };
        if peer.addrs.first() == Some(&addr) {
            return false;
        }
        peer.addrs.retain(|a| *a != addr);
        peer.addrs.insert(0, addr);
        peer.addrs.truncate(4);
        true
    }

    pub(super) fn trusts(&self, seen: &[u8; 32]) -> bool {
        self.roster.lock().unwrap().iter().any(|p| parse_fingerprint(&p.fingerprint).is_ok_and(|f| f == *seen))
    }

    /// Send to one peer. Silently does nothing when that peer is not connected:
    /// input for a machine that just dropped off has nowhere useful to go.
    pub fn send(&self, peer: &str, msg: Msg) {
        if let Some(live) = self.peers.lock().unwrap().get(peer) {
            let _ = live.tx.send(msg);
        }
    }

    /// Claim the link to this peer. Both machines dial each other, so two
    /// links can come up at once; the one the server dialed wins on both ends.
    /// Were each end to keep its own, each would hang up the other's and both
    /// would die. Any other new link replaces the held one: neither end dials
    /// a peer it is linked to, so the held one is a dead machine's leftover
    /// waiting out its idle timeout. False when this link lost and must hang up.
    pub(super) fn try_register(&self, peer: &str, tx: UnboundedSender<Msg>, by_server: bool) -> bool {
        let mut peers = self.peers.lock().unwrap();
        let held = peers.get(peer).filter(|live| !live.tx.is_closed());
        if held.is_some_and(|live| live.by_server && !by_server) {
            return false;
        }
        peers.insert(peer.to_string(), Live { tx, by_server });
        true
    }

    /// Whether a live link to this peer already exists. The session asks before
    /// letting its capture take the cursor: grabbing it with nowhere to send it
    /// swallows the keyboard and mouse and leaves no way to reach the menu bar.
    pub fn connected(&self, peer: &str) -> bool {
        self.peers.lock().unwrap().get(peer).is_some_and(|live| !live.tx.is_closed())
    }

    /// Drop this link, unless a link the server dialed has replaced it. True
    /// when it was still this peer's link, so the peer is now gone.
    pub(super) fn unregister(&self, peer: &str, tx: &UnboundedSender<Msg>) -> bool {
        let mut peers = self.peers.lock().unwrap();
        if !peers.get(peer).is_some_and(|live| live.tx.same_channel(tx)) {
            return false;
        }
        peers.remove(peer);
        self.conns.lock().unwrap().remove(peer);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn links_only_send_to_connected_peers() {
        let links = Links::default();
        links.send("ghost", Msg::Enter); // must not panic
        let (tx, mut rx) = unbounded_channel();
        assert!(links.try_register("mybox", tx.clone(), true));
        links.send("mybox", Msg::Enter);
        assert_eq!(rx.try_recv().unwrap(), Msg::Enter);
        let (second, _second_rx) = unbounded_channel();
        assert!(!links.try_register("mybox", second, false), "a client's dial never displaces the server's link");
        assert!(links.unregister("mybox", &tx));
        links.send("mybox", Msg::Leave);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn when_both_dial_the_link_the_server_dialed_wins() {
        let links = Links::default();
        let (own, _own_rx) = unbounded_channel();
        assert!(links.try_register("mac", own.clone(), false), "this end's own dial comes up first");
        let (servers, mut servers_rx) = unbounded_channel();
        assert!(links.try_register("mac", servers.clone(), true), "the server's dial replaces it");
        let (late, _late_rx) = unbounded_channel();
        assert!(!links.try_register("mac", late, false), "and is never replaced by a non-server dial");
        assert!(!links.unregister("mac", &own), "the replaced link's teardown leaves the peer linked");
        links.send("mac", Msg::Enter);
        assert_eq!(servers_rx.try_recv().unwrap(), Msg::Enter);
        assert!(links.unregister("mac", &servers));
    }

    /// Neither end dials a peer it is linked to, so a second link of the same
    /// kind means the first is dead: its machine restarted, and the old link
    /// lingers until the idle timeout. The new one must not wait for that.
    #[test]
    fn a_restarted_peer_replaces_its_dead_link() {
        let links = Links::default();
        let (old_server, _old_rx) = unbounded_channel();
        assert!(links.try_register("mac", old_server, true));
        let (new_server, mut new_rx) = unbounded_channel();
        assert!(links.try_register("mac", new_server, true), "the restarted server's dial");
        links.send("mac", Msg::Enter);
        assert_eq!(new_rx.try_recv().unwrap(), Msg::Enter);

        let (old_client, _old_rx) = unbounded_channel();
        assert!(links.try_register("anorak", old_client, false));
        let (new_client, _new_rx) = unbounded_channel();
        assert!(links.try_register("anorak", new_client, false), "the restarted client's dial");
    }
}
