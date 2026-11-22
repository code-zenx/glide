//! The config file: who this machine is, which peers it trusts and where
//! their screens sit, and the small edits the menu bar and the daemon save
//! back to it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::{display, screens};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    /// Name of this machine
    pub name: String,
    /// Which end of the link this machine is
    pub role: Role,
    /// Address to listen on
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The extended display. Only a server's is read: the client is told.
    #[serde(default)]
    pub display: display::Settings,
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}

/// Input flows one way, as it does in Barrier: the machine with the keyboard
/// and mouse is the server, and the machine it drives is the client. Nothing
/// types on a server, and a client's own screen edge only ever hands control
/// back, so neither end can take the other over by surprise.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// This machine's keyboard and mouse are the ones being shared.
    Server,
    /// This machine is driven by the server, and shares no input of its own.
    Client,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Peer {
    pub name: String,
    /// Every address this peer may be reachable on: wired first, then wireless
    pub addrs: Vec<SocketAddr>,
    /// The peer's certificate fingerprint, from `glide fingerprint` on that machine
    pub fingerprint: String,
    /// Which edge of this screen the peer's screen sits on
    pub position: Side,
    /// Which of this machine's displays that edge belongs to, counting from 1
    /// as the Arrange window labels them. Left out, the edge belongs to the
    /// whole desktop, which is upstream lan-mouse's behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<usize>,
    /// Switched off from the server's menu: the server neither dials it nor
    /// accepts it, and tells it so, so nothing at all crosses.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub enabled: bool,
    /// The server's cursor may cross into this peer. Off by default: a peer
    /// that is on shares the clipboard only, until Control is chosen.
    #[serde(default, skip_serializing_if = "is_no")]
    pub control: bool,
}

fn yes() -> bool {
    true
}

fn is_yes(on: &bool) -> bool {
    *on
}

fn is_no(on: &bool) -> bool {
    !*on
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    /// If a peer sits above me, I sit below it. Both machines have to agree, or
    /// the cursor goes out one edge and can only come back through another.
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
}

pub(crate) fn default_listen() -> SocketAddr {
    "0.0.0.0:4242".parse().unwrap()
}

pub(crate) fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME is not set");
    Path::new(&home).join(".config/glide")
}

/// Move a peer's screen to another edge and remember it. The menu bar calls
/// this, so the arrangement survives a restart.
pub fn set_position(path: &Path, peer_name: &str, side: Side, display: Option<usize>) -> Result<()> {
    let mut config = load(path)?;
    let peer = config
        .peers
        .iter_mut()
        .find(|p| p.name == peer_name)
        .with_context(|| format!("no peer named {peer_name}"))?;
    peer.position = side;
    peer.display = display;
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

/// Every device that is switched on, with its place, for the arrange window.
/// An Off device has no edge, so it has nothing to arrange.
#[cfg(target_os = "macos")]
pub fn arrange_peers(path: &Path) -> Result<Vec<(String, Side, Option<usize>)>> {
    Ok(load(path)?.peers.into_iter().filter(|p| p.enabled).map(|p| (p.name, p.position, p.display)).collect())
}

/// The device the extended display goes to, if any. An old config's
/// `mode = "extend"` meant the first peer.
pub fn extend_target(config: &Config) -> Option<Peer> {
    match (&config.display.extend, config.display.mode) {
        (Some(name), _) => config.peers.iter().find(|p| &p.name == name).cloned(),
        (None, display::Mode::Extend) => config.peers.first().cloned(),
        (None, display::Mode::Control) => None,
    }
}

/// Remember which device shows the extended display, so it comes back after a
/// restart.
pub fn set_extend(path: &Path, target: Option<&str>) -> Result<()> {
    let mut config = load(path)?;
    config.display.extend = target.map(str::to_string);
    config.display.mode = if target.is_some() { display::Mode::Extend } else { display::Mode::Control };
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

/// A paired device: added on the first free edge, or, when one by that name
/// exists already (a reinstalled machine), given its new certificate and
/// address and left where it was.
pub fn add_peer(path: &Path, name: &str, fingerprint: &str, addr: SocketAddr, screens: &[screens::Screen]) -> Result<Peer> {
    let mut config = load(path)?;
    let peer = match config.peers.iter_mut().find(|p| p.name == name) {
        Some(peer) => {
            peer.fingerprint = fingerprint.to_string();
            peer.addrs.retain(|a| *a != addr);
            peer.addrs.insert(0, addr);
            peer.clone()
        }
        None => {
            let (position, display) = free_edge(&config.peers, screens);
            let peer = Peer {
                name: name.to_string(),
                addrs: vec![addr],
                fingerprint: fingerprint.to_string(),
                position,
                display,
                enabled: true,
                control: false,
            };
            config.peers.push(peer.clone());
            peer
        }
    };
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(peer)
}

/// The first outer edge of a display that no device sits on: displays in
/// order, and on each left, right, bottom, top. An edge another display of
/// this machine touches is not outer — the cursor goes to that display there.
fn free_edge(peers: &[Peer], screens: &[screens::Screen]) -> (Side, Option<usize>) {
    let taken = |side: Side, display: usize| peers.iter().any(|p| p.position == side && p.display.unwrap_or(1) == display);
    for display in 1..=screens.len().max(1) {
        for side in [Side::Left, Side::Right, Side::Bottom, Side::Top] {
            if outer(screens, display - 1, side) && !taken(side, display) {
                return (side, Some(display));
            }
        }
    }
    // ponytail: every outer edge is taken, so two devices share one; the
    // Arrange window moves either.
    (Side::Top, Some(1))
}

fn outer(screens: &[screens::Screen], i: usize, side: Side) -> bool {
    let Some(s) = screens.get(i) else { return true };
    let (sx, sy, sw, sh) = (s.x, s.y, s.w as i32, s.h as i32);
    !screens.iter().enumerate().any(|(j, o)| {
        let (ox, oy, ow, oh) = (o.x, o.y, o.w as i32, o.h as i32);
        let along_y = oy < sy + sh && sy < oy + oh;
        let along_x = ox < sx + sw && sx < ox + ow;
        j != i
            && match side {
                Side::Left => ox + ow == sx && along_y,
                Side::Right => sx + sw == ox && along_y,
                Side::Top => oy + oh == sy && along_x,
                Side::Bottom => sy + sh == oy && along_x,
            }
    })
}

/// Drop a device from the config: its certificate is no longer trusted.
pub fn forget_peer(path: &Path, peer_name: &str) -> Result<()> {
    let mut config = load(path)?;
    config.peers.retain(|p| p.name != peer_name);
    if config.display.extend.as_deref() == Some(peer_name) {
        config.display.extend = None;
        config.display.mode = display::Mode::Control;
    }
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

/// Remember the addresses a peer was last reached on.
pub fn set_peer_addrs(path: &Path, peer_name: &str, addrs: Vec<SocketAddr>) -> Result<()> {
    let mut config = load(path)?;
    let peer = config
        .peers
        .iter_mut()
        .find(|p| p.name == peer_name)
        .with_context(|| format!("no peer named {peer_name}"))?;
    peer.addrs = addrs;
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

/// Remember whether a peer is switched on, so a pause survives a restart.
pub fn set_enabled(path: &Path, peer_name: &str, on: bool) -> Result<()> {
    update_peer(path, peer_name, |peer| peer.enabled = on)
}

/// Change one peer's entry in the config file and save it.
pub fn update_peer(path: &Path, peer_name: &str, change: impl FnOnce(&mut Peer)) -> Result<()> {
    let mut config = load(path)?;
    let peer = config
        .peers
        .iter_mut()
        .find(|p| p.name == peer_name)
        .with_context(|| format!("no peer named {peer_name}"))?;
    change(peer);
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

/// Remember where a peer says it sits, leaving which display alone.
pub fn set_peer_side(path: &Path, peer_name: &str, side: Side) -> Result<()> {
    let mut config = load(path)?;
    let peer = config
        .peers
        .iter_mut()
        .find(|p| p.name == peer_name)
        .with_context(|| format!("no peer named {peer_name}"))?;
    peer.position = side;
    std::fs::write(path, toml::to_string_pretty(&config)?)?;
    Ok(())
}

pub(crate) fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!("no config at {}, run `glide init --name <name> --role <server|client>`", path.display())
    })?;
    // `role` is the one field with no default: guessing it wrong would either
    // hand the other machine control of this one or share nothing at all.
    toml::from_str(&text).with_context(|| {
        format!("{}: role is \"server\" to share this machine's keyboard and mouse, \"client\" to be driven", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arranging_one_peer_leaves_the_others_alone() {
        let path = std::env::temp_dir().join(format!("glide-config-test-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "name = \"goldengate\"\nrole = \"server\"\n\
             [[peer]]\nname = \"anorak\"\naddrs = [\"192.168.2.3:4242\"]\nposition = \"top\"\nfingerprint = \"aa\"\n\
             [[peer]]\nname = \"anton\"\naddrs = [\"192.168.0.5:4242\"]\nposition = \"left\"\nfingerprint = \"bb\"\n",
        )
        .unwrap();
        set_position(&path, "anton", Side::Right, Some(2)).unwrap();
        let config = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((config.peers[0].position, config.peers[0].display), (Side::Top, None));
        assert_eq!((config.peers[1].position, config.peers[1].display), (Side::Right, Some(2)));
        assert!(set_position(&path, "nobody", Side::Left, None).is_err());
    }

    #[test]
    fn a_pause_is_remembered_and_defaults_to_on() {
        let path = std::env::temp_dir().join(format!("glide-config-pause-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "name = \"goldengate\"\nrole = \"server\"\n\
             [[peer]]\nname = \"anton\"\naddrs = [\"192.168.0.5:4242\"]\nposition = \"left\"\nfingerprint = \"bb\"\n",
        )
        .unwrap();
        assert!(load(&path).unwrap().peers[0].enabled, "a config without the field means on");
        set_enabled(&path, "anton", false).unwrap();
        assert!(!load(&path).unwrap().peers[0].enabled);
        set_enabled(&path, "anton", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!text.contains("enabled"), "on is the default and stays out of the file: {text}");
    }

    #[test]
    fn control_defaults_to_off_and_is_remembered() {
        let path = std::env::temp_dir().join(format!("glide-config-control-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "name = \"goldengate\"\nrole = \"server\"\n\
             [[peer]]\nname = \"anton\"\naddrs = [\"192.168.0.5:4242\"]\nposition = \"left\"\nfingerprint = \"bb\"\n",
        )
        .unwrap();
        assert!(!load(&path).unwrap().peers[0].control, "a config without the field means clipboard only");
        update_peer(&path, "anton", |p| p.control = true).unwrap();
        assert!(load(&path).unwrap().peers[0].control);
        update_peer(&path, "anton", |p| p.control = false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!text.contains("\ncontrol ="), "off is the default and stays out of the file: {text}");
    }

    fn screen(x: i32, y: i32, w: u32, h: u32) -> screens::Screen {
        screens::Screen { name: "s".into(), x, y, w, h }
    }

    #[test]
    fn a_new_device_lands_on_a_free_outer_edge() {
        // The LG and the portrait Dell to its right: the LG's right edge is the
        // Dell, so it is never offered.
        let screens = [screen(0, 0, 2560, 1080), screen(2560, 0, 1080, 1920)];
        let peer = |side, display| Peer { name: "p".into(), addrs: vec![], fingerprint: String::new(), position: side, display, enabled: true, control: false };
        assert_eq!(free_edge(&[], &screens), (Side::Left, Some(1)));
        let taken = [peer(Side::Top, Some(1)), peer(Side::Left, Some(1))];
        assert_eq!(free_edge(&taken, &screens), (Side::Bottom, Some(1)));
        let more = [peer(Side::Top, Some(1)), peer(Side::Left, Some(1)), peer(Side::Bottom, Some(1))];
        assert_eq!(free_edge(&more, &screens), (Side::Right, Some(2)), "the Dell's outer right edge next");
        assert_eq!(free_edge(&[], &[]), (Side::Left, Some(1)), "no screens known: any edge counts");
    }

    #[test]
    fn pairing_adds_or_rekeys_and_forgetting_removes() {
        let path = std::env::temp_dir().join(format!("glide-config-pair-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "name = \"goldengate\"\nrole = \"server\"\n[display]\nmode = \"extend\"\n\
             [[peer]]\nname = \"anorak\"\naddrs = [\"192.168.2.3:4242\"]\nposition = \"top\"\ndisplay = 1\nfingerprint = \"aa\"\n",
        )
        .unwrap();
        assert_eq!(extend_target(&load(&path).unwrap()).map(|p| p.name), Some("anorak".into()), "old mode = extend means the first peer");
        let screens = [screen(0, 0, 2560, 1080)];
        let added = add_peer(&path, "nullbyte", "bb", "192.168.0.7:4242".parse().unwrap(), &screens).unwrap();
        assert_eq!((added.position, added.display), (Side::Left, Some(1)));
        let rekeyed = add_peer(&path, "anorak", "cc", "192.168.0.9:4242".parse().unwrap(), &screens).unwrap();
        assert_eq!((rekeyed.fingerprint.as_str(), rekeyed.position), ("cc", Side::Top), "same name: new key, same place");
        assert_eq!(rekeyed.addrs[0], "192.168.0.9:4242".parse().unwrap());
        set_extend(&path, Some("nullbyte")).unwrap();
        forget_peer(&path, "nullbyte").unwrap();
        let config = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(config.peers.len(), 1);
        assert_eq!(extend_target(&config).map(|p| p.name), None, "forgetting the extended device stops extending");
    }
}
