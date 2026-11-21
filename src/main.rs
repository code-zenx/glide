#[cfg(target_os = "macos")]
mod audio_mac;
#[cfg(not(target_os = "macos"))]
mod capture_wayland;
mod clipboard;
mod ctl;
mod display;
#[cfg(not(target_os = "macos"))]
mod display_linux;
#[cfg(target_os = "macos")]
mod display_mac;
#[cfg(target_os = "macos")]
mod h264;
mod input;
mod link;
#[cfg(target_os = "macos")]
mod arrange;
#[cfg(target_os = "macos")]
mod menubar;
mod pair;
mod proto;
mod screens;
mod status;
mod view;
#[cfg(target_os = "macos")]
mod view_input_mac;
#[cfg(not(target_os = "macos"))]
mod view_linux;
#[cfg(target_os = "macos")]
mod view_mac;

use std::net::SocketAddr;
use std::sync::Arc;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "glide", about = "Share one keyboard and mouse across machines")]
struct Cli {
    /// Config file (default: ~/.config/glide/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// What a bare `glide` does: the menu bar app on macOS, the daemon elsewhere.
fn default_command() -> Cmd {
    #[cfg(target_os = "macos")]
    {
        Cmd::App
    }
    #[cfg(not(target_os = "macos"))]
    {
        Cmd::Run
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the config and this machine's certificate
    Init {
        /// Name of this machine, as peers refer to it
        #[arg(long)]
        name: String,
        /// `server` if this machine's keyboard and mouse drive the other one,
        /// `client` if it is the one being driven
        #[arg(long)]
        role: Role,
    },
    /// Print this machine's certificate fingerprint
    Fingerprint,
    /// Connect to the configured peers and report latency
    Run,
    /// Print what the daemon is doing, for a status bar or a quick look
    Status {
        /// One JSON object per line, the shape waybar expects
        #[arg(long)]
        json: bool,
        /// Keep printing, within a tenth of a second of each change
        #[arg(long)]
        watch: bool,
        /// Print the app's letter instead of words, for a bar that draws it as
        /// a badge in CSS
        #[arg(long)]
        icon: bool,
    },
    /// Make a one-time invite (server): prints the `glide join …` command to
    /// run on the new machine. Valid for ten minutes.
    Invite,
    /// Pair this machine with a server, using the command its invite printed
    Join {
        token: String,
        /// This machine's name on the server (default: the hostname)
        #[arg(long)]
        name: Option<String>,
    },
    /// Steer the running daemon: pause, resume, release, peer on|off|forget
    /// <name>, extend <name>|off, view <name>|off, invite
    Ctl {
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Run the daemon with a menu bar item: what the app bundle launches
    #[cfg(target_os = "macos")]
    App,
    /// Draw the arrange window to a PNG, for looking at the design
    #[cfg(target_os = "macos")]
    Snapshot {
        out: PathBuf,
        /// Pretend the peer sits on this side
        #[arg(long, default_value = "right")]
        side: String,
    },
}

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
}

fn yes() -> bool {
    true
}

fn is_yes(on: &bool) -> bool {
    *on
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

fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| config_dir().join("config.toml"))
}

fn config_dir() -> PathBuf {
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
    let mut config = load(path)?;
    let peer = config
        .peers
        .iter_mut()
        .find(|p| p.name == peer_name)
        .with_context(|| format!("no peer named {peer_name}"))?;
    peer.enabled = on;
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


/// Everything except the user interface: the link, the input session and the
/// clipboard watcher, run until something gives out.
fn daemon(
    config: Config,
    config_path: PathBuf,
    identity: link::Identity,
    status: Arc<status::Status>,
    control: tokio::sync::mpsc::UnboundedReceiver<input::Control>,
    control_tx: tokio::sync::mpsc::UnboundedSender<input::Control>,
) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    // The Wayland capture backend spawns non-Send tasks, so the session has to
    // run inside a LocalSet.
    tokio::task::LocalSet::new().block_on(&runtime, async move {
        let server = config.role == Role::Server;
        let links = Arc::new(link::Links::new(config.peers.clone(), server));
        let (to_session, from_links) = tokio::sync::mpsc::unbounded_channel();
        // Whether each peer is reachable, so the session only arms a screen
        // edge when there is a live link behind it.
        let (link_up, link_state) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(clipboard::watch(links.clone()));
        let me = server.then(|| pair::Me { name: config.name.clone(), port: config.listen.port(), fingerprint: identity.fingerprint() });
        let (ctl_links, ctl_control) = (links.clone(), control_tx.clone());
        tokio::spawn(async move {
            if let Err(e) = ctl::serve(ctl_control, ctl_links, me).await {
                tracing::warn!("no control socket: {e}");
            }
        });
        let net = link::endpoint(&config, &identity, links.clone())?;
        let (to_display, display_events) = tokio::sync::mpsc::unbounded_channel();
        let (to_view, view_events) = tokio::sync::mpsc::unbounded_channel();
        // Media connections a peer opens to us: an extended display's goes to
        // the display side, a View Screen one to the view side.
        let (video_in, mut videos) = tokio::sync::mpsc::unbounded_channel();
        let (forward, forward_view) = (to_display.clone(), to_view.clone());
        tokio::spawn(async move {
            while let Some((peer, conn)) = videos.recv().await {
                if link::is_view(&conn) {
                    let _ = forward_view.send(view::Event::Conn(peer, conn));
                } else {
                    let _ = forward.send(display::Event::Video(peer, conn));
                }
            }
        });
        #[cfg(target_os = "macos")]
        tokio::spawn(display::host(
            config.display.clone(),
            config_path.clone(),
            net.clone(),
            extend_target(&config),
            links.clone(),
            status.clone(),
            display_events,
        ));
        #[cfg(not(target_os = "macos"))]
        tokio::spawn(display::show(links.clone(), status.clone(), display_events));
        #[cfg(target_os = "macos")]
        tokio::spawn(view::watch(net.clone(), links.clone(), control_tx.clone(), view_events));
        #[cfg(not(target_os = "macos"))]
        tokio::spawn(view::share(links.clone(), view_events));
        let network = tokio::spawn(link::run(
            config.clone(),
            config_path.clone(),
            net,
            links.clone(),
            status.clone(),
            to_session,
            link_up,
            video_in,
            control_tx,
        ));
        tokio::select! {
            result = input::session(config, config_path, links, status, from_links, link_state, control, to_display, to_view) => result,
            result = network => result?,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}

/// Every configured peer starts offline, or off if it is switched off here.
fn mark_all(status: &status::Status, config: &Config) {
    let server = config.role == Role::Server;
    for peer in &config.peers {
        status.offline(&peer.name);
        if server && !peer.enabled {
            status.set_peer(&peer.name, |p| p.mode = "off".into());
        }
    }
    let extending = extend_target(config).map(|p| p.name).unwrap_or_default();
    status.set(|s| s.extending = extending);
}

fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!("no config at {}, run `glide init --name <name> --role <server|client>`", path.display())
    })?;
    // `role` is the one field with no default: guessing it wrong would either
    // hand the other machine control of this one or share nothing at all.
    toml::from_str(&text).with_context(|| {
        format!("{}: role is \"server\" to share this machine's keyboard and mouse, \"client\" to be driven", path.display())
    })
}

/// The app has no terminal to print to, so it keeps a log where a person can
/// actually find it. Everything else logs to stderr.
fn start_logging(to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "glide=info".into());
    let log = to_file
        .then(|| std::env::var("HOME").ok())
        .flatten()
        .map(|home| Path::new(&home).join("Library/Logs/Glide.log"))
        .and_then(|path| std::fs::File::create(path).ok());
    match log {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

fn main() -> Result<()> {
    // Finder hands a launched app a `-psn_0_123` argument that clap knows
    // nothing about; drop it rather than exiting with a usage error.
    let args = std::env::args().filter(|a| !a.starts_with("-psn_"));
    let cli = Cli::parse_from(args);
    let path = config_path(&cli);
    let cmd = cli.cmd.unwrap_or_else(default_command);
    #[cfg(target_os = "macos")]
    start_logging(matches!(cmd, Cmd::App));
    #[cfg(not(target_os = "macos"))]
    start_logging(false);
    // A panic in a background task otherwise goes to stderr, which the app
    // does not have: the task just stops, and nothing says why.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));

    match &cmd {
        Cmd::Init { name, role } => {
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            let identity = link::Identity::load_or_create(&config_dir())?;
            let config =
                Config {
                    name: name.clone(),
                    role: *role,
                    listen: default_listen(),
                    display: display::Settings::default(),
                    peers: vec![],
                };
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, toml::to_string_pretty(&config)?)?;
            println!("wrote {}", path.display());
            println!("fingerprint: {}", identity.fingerprint());
            println!("\nAdd the other machine to that file:\n");
            println!("[[peer]]\nname = \"other\"\naddrs = [\"192.168.0.3:4242\"]\nposition = \"right\"\nfingerprint = \"<its fingerprint>\"");
            if *role == Role::Client {
                println!("\nThis machine is a client: it types what the server sends and shares");
                println!("no keyboard or mouse of its own. `position` is only read on the server.");
            }
        }
        Cmd::Fingerprint => {
            println!("{}", link::Identity::load_or_create(&config_dir())?.fingerprint());
        }
        Cmd::Run => {
            let config = load(&path)?;
            if config.peers.is_empty() && config.role == Role::Client {
                bail!("no server in {}: pair with one using the `glide join …` command its invite prints", path.display());
            }
            let identity = link::Identity::load_or_create(&config_dir())?;
            let status = Arc::new(status::Status::default());
            mark_all(&status, &config);
            let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            daemon(config, path, identity, status, control_rx, control_tx)?;
        }
        Cmd::Ctl { words } => println!("{}", ctl::send(words)?),
        Cmd::Invite => println!("{}", ctl::send(&["invite".to_string()])?),
        Cmd::Join { token, name } => {
            tokio::runtime::Runtime::new()?.block_on(pair::join(token, name.clone(), &path, &config_dir()))?;
        }
        Cmd::Status { json, watch, icon } => {
            let mut last = String::new();
            loop {
                let state = status::read();
                let line = if *json { state.json(*icon) } else { state.summary() };
                if line != last {
                    println!("{line}");
                    last = line;
                }
                if !watch {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        #[cfg(target_os = "macos")]
        Cmd::Snapshot { out, side } => {
            let config = load(&path)?;
            anyhow::ensure!(!config.peers.is_empty(), "no peers in the config");
            let side = match side.as_str() {
                "left" => Side::Left,
                "top" => Side::Top,
                "bottom" => Side::Bottom,
                _ => Side::Right,
            };
            let mtm = objc2_foundation::MainThreadMarker::new().context("not the main thread")?;
            // Stand-ins for the peers' screens when the daemon is not running;
            // `side` moves the first peer, the rest sit where the config says.
            let peers = config
                .peers
                .iter()
                .filter(|p| p.enabled)
                .enumerate()
                .map(|(i, p)| {
                    let theirs = vec![screens::Screen { name: "HDMI-A-1".into(), x: 0, y: 0, w: 1920, h: 1080 }];
                    arrange::PeerView::new(p.name.clone(), theirs, if i == 0 { side } else { p.position }, p.display)
                })
                .collect();
            arrange::snapshot(mtm, screens::local(), peers, out)?;
            println!("wrote {}", out.display());
        }
        #[cfg(target_os = "macos")]
        Cmd::App => {
            let config = load(&path)?;
            let identity = link::Identity::load_or_create(&config_dir())?;
            let me = pair::Me { name: config.name.clone(), port: config.listen.port(), fingerprint: identity.fingerprint() };
            let status = Arc::new(status::Status::default());
            mark_all(&status, &config);
            let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            let background = (config, path.clone(), identity, status.clone(), control_tx.clone());
            std::thread::spawn(move || {
                let (config, config_path, identity, status, control_tx) = background;
                if let Err(e) = daemon(config, config_path, identity, status, control_rx, control_tx) {
                    tracing::error!("daemon stopped: {e}");
                }
            });
            // AppKit owns the main thread from here.
            menubar::run(status, control_tx, path, me)?;
        }
    }
    Ok(())
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

    fn screen(x: i32, y: i32, w: u32, h: u32) -> screens::Screen {
        screens::Screen { name: "s".into(), x, y, w, h }
    }

    #[test]
    fn a_new_device_lands_on_a_free_outer_edge() {
        // The LG and the portrait Dell to its right: the LG's right edge is the
        // Dell, so it is never offered.
        let screens = [screen(0, 0, 2560, 1080), screen(2560, 0, 1080, 1920)];
        let peer = |side, display| Peer { name: "p".into(), addrs: vec![], fingerprint: String::new(), position: side, display, enabled: true };
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
