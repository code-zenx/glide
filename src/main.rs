mod clipboard;
mod input;
mod link;
#[cfg(target_os = "macos")]
mod arrange;
#[cfg(target_os = "macos")]
mod menubar;
mod proto;
mod screens;
mod status;

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
        /// Keep printing, once a second
        #[arg(long)]
        watch: bool,
        /// Print the app's letter instead of words, for a bar that draws it as
        /// a badge in CSS
        #[arg(long)]
        icon: bool,
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

#[derive(Serialize, Deserialize, Clone)]
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

fn default_listen() -> SocketAddr {
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
pub fn set_position(path: &Path, side: Side, display: Option<usize>) -> Result<()> {
    let mut config = load(path)?;
    let peer = config.peers.first_mut().context("no peers to arrange")?;
    peer.position = side;
    peer.display = display;
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
) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    // The Wayland capture backend spawns non-Send tasks, so the session has to
    // run inside a LocalSet.
    tokio::task::LocalSet::new().block_on(&runtime, async move {
        let links = Arc::new(link::Links::default());
        let (to_session, from_links) = tokio::sync::mpsc::unbounded_channel();
        // Whether each peer is reachable, so the session only arms a screen
        // edge when there is a live link behind it.
        let (link_up, link_state) = tokio::sync::mpsc::unbounded_channel();
        let peers = config.peers.iter().map(|p| p.name.clone()).collect();
        tokio::spawn(clipboard::watch(links.clone(), peers));
        let network = tokio::spawn(link::run(
            config.clone(),
            identity,
            links.clone(),
            status.clone(),
            to_session,
            link_up,
        ));
        tokio::select! {
            result = input::session(config, config_path, links, status, from_links, link_state, control) => result,
            result = network => result?,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
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

    match &cmd {
        Cmd::Init { name, role } => {
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            let identity = link::Identity::load_or_create(&config_dir())?;
            let config =
                Config { name: name.clone(), role: *role, listen: default_listen(), peers: vec![] };
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
            if config.peers.is_empty() {
                bail!("no peers in {}", path.display());
            }
            let identity = link::Identity::load_or_create(&config_dir())?;
            let status = Arc::new(status::Status::default());
            status.offline(&config.peers[0].name);
            let (_control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            daemon(config, path, identity, status, control_rx)?;
        }
        Cmd::Status { json, watch, icon } => loop {
            let state = status::read();
            println!("{}", if *json { state.json(*icon) } else { state.summary() });
            if !watch {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        #[cfg(target_os = "macos")]
        Cmd::Snapshot { out, side } => {
            let config = load(&path)?;
            let peer = config.peers.first().context("no peers in the config")?;
            let side = match side.as_str() {
                "left" => Side::Left,
                "top" => Side::Top,
                "bottom" => Side::Bottom,
                _ => Side::Right,
            };
            let mtm = objc2_foundation::MainThreadMarker::new().context("not the main thread")?;
            // A stand-in for the peer's screens when the daemon is not running.
            let theirs = vec![screens::Screen {
                name: "HDMI-A-2".into(),
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            }];
            arrange::snapshot(mtm, screens::local(), theirs, peer.name.clone(), side, peer.display, out)?;
            println!("wrote {}", out.display());
        }
        #[cfg(target_os = "macos")]
        Cmd::App => {
            let config = load(&path)?;
            let peer = config.peers.first().context("no peers in the config")?;
            let (position, name, display) = (peer.position, peer.name.clone(), peer.display);
            let identity = link::Identity::load_or_create(&config_dir())?;
            let status = Arc::new(status::Status::default());
            status.offline(&name);
            let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            let background = (config, path.clone(), identity, status.clone());
            std::thread::spawn(move || {
                let (config, config_path, identity, status) = background;
                if let Err(e) = daemon(config, config_path, identity, status, control_rx) {
                    tracing::error!("daemon stopped: {e}");
                }
            });
            // AppKit owns the main thread from here.
            menubar::run(status, control_tx, path, position, display, name)?;
        }
    }
    Ok(())
}
