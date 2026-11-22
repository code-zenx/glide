#[cfg(target_os = "macos")]
mod audio_mac;
#[cfg(not(target_os = "macos"))]
mod capture_wayland;
mod cli;
mod clipboard;
#[cfg(not(target_os = "macos"))]
mod clipboard_wayland;
mod config;
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

use std::sync::Arc;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{bail, Result};
use clap::Parser;

use cli::{config_path, default_command, Cli, Cmd};
#[cfg(target_os = "macos")]
pub use config::arrange_peers;
pub use config::{
    add_peer, extend_target, forget_peer, set_enabled, set_extend, set_peer_addrs, set_peer_side, set_position,
    update_peer, Config, Peer, Role, Side,
};
pub(crate) use config::{config_dir, default_listen, load};

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
        tokio::spawn(serve_ctl(control_tx.clone(), links.clone(), me));
        let net = link::endpoint(&config, &identity, links.clone())?;
        let (to_display, display_events) = tokio::sync::mpsc::unbounded_channel();
        let (to_view, view_events) = tokio::sync::mpsc::unbounded_channel();
        let (video_in, videos) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(route_media(videos, to_display.clone(), to_view.clone()));
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
        tokio::spawn(link::leave_on_stop(net.endpoint.clone()));
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
        }
    })
}

/// The control socket a status bar or `glide ctl` steers the daemon through.
/// The daemon runs on without it, so a failure is only logged.
async fn serve_ctl(
    ctl_control: tokio::sync::mpsc::UnboundedSender<input::Control>,
    ctl_links: Arc<link::Links>,
    me: Option<pair::Me>,
) {
    if let Err(e) = ctl::serve(ctl_control, ctl_links, me).await {
        tracing::warn!("no control socket: {e}");
    }
}

// Media connections a peer opens to us: an extended display's goes to
// the display side, a View Screen one to the view side.
async fn route_media(
    mut videos: tokio::sync::mpsc::UnboundedReceiver<(String, quinn::Connection)>,
    forward: tokio::sync::mpsc::UnboundedSender<display::Event>,
    forward_view: tokio::sync::mpsc::UnboundedSender<view::Event>,
) {
    while let Some((peer, conn)) = videos.recv().await {
        if link::is_view(&conn) {
            let _ = forward_view.send(view::Event::Conn(peer, conn));
        } else {
            let _ = forward.send(display::Event::Video(peer, conn));
        }
    }
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
        Cmd::Init { name, role } => init(path, name, role)?,
        Cmd::Fingerprint => {
            println!("{}", link::Identity::load_or_create(&config_dir())?.fingerprint());
        }
        Cmd::Run => run(path)?,
        Cmd::Ctl { words } => println!("{}", ctl::send(words)?),
        Cmd::Invite => println!("{}", ctl::send(&["invite".to_string()])?),
        Cmd::Join { token, name } => {
            tokio::runtime::Runtime::new()?.block_on(pair::join(token, name.clone(), &path, &config_dir()))?;
        }
        Cmd::Status { json, watch, icon } => print_status(json, watch, icon),
        #[cfg(target_os = "macos")]
        Cmd::Snapshot { out, side } => snapshot(path, out, side)?,
        #[cfg(target_os = "macos")]
        Cmd::App => run_app(path)?,
    }
    Ok(())
}

/// `glide init`: write a fresh config and make this machine's certificate.
fn init(path: PathBuf, name: &str, role: &Role) -> Result<()> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let identity = link::Identity::load_or_create(&config_dir())?;
    let config =
        Config {
            name: name.to_string(),
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
    Ok(())
}

/// `glide run`: the daemon on its own, without the menu bar.
fn run(path: PathBuf) -> Result<()> {
    let config = load(&path)?;
    if config.peers.is_empty() && config.role == Role::Client {
        bail!("no server in {}: pair with one using the `glide join …` command its invite prints", path.display());
    }
    let identity = link::Identity::load_or_create(&config_dir())?;
    let status = Arc::new(status::Status::default());
    mark_all(&status, &config);
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    daemon(config, path, identity, status, control_rx, control_tx)?;
    Ok(())
}

/// `glide status`: print what the daemon is doing, once or on every change.
fn print_status(json: &bool, watch: &bool, icon: &bool) {
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

/// `glide snapshot`: draw the arrange window to a PNG.
#[cfg(target_os = "macos")]
fn snapshot(path: PathBuf, out: &Path, side: &str) -> Result<()> {
    let config = load(&path)?;
    anyhow::ensure!(!config.peers.is_empty(), "no peers in the config");
    let side = match side {
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
    Ok(())
}

/// `glide app`: the daemon on a thread of its own, the menu bar on the main
/// thread.
#[cfg(target_os = "macos")]
fn run_app(path: PathBuf) -> Result<()> {
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
    Ok(())
}
