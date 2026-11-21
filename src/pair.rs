//! Adding a device with one paste. The server makes an invite — its name,
//! addresses, certificate fingerprint and a one-time secret — and shows it as
//! a `glide join …` command. Run on the new machine, that command dials the
//! server with the invite's fingerprint pinned and presents the secret; the
//! server checks it and adds the newcomer's own certificate to its peers.
//!
//! The secret is what keeps a stranger on the LAN out: it travels only over a
//! channel the owner controls (their clipboard, their ssh session), works
//! once, and expires after ten minutes. A newcomer's fingerprint is taken from
//! its TLS certificate, never from anything it says.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::link::Links;
use crate::status::Status;
use crate::{Peer, Side};

const LIFETIME: Duration = Duration::from_secs(10 * 60);
const PREFIX: &str = "glide1";
/// Separates the invite's fields. Shell-safe, and in no hostname, address or
/// hex string, so the whole invite pastes as one word.
const SEP: char = '+';
const MAX_MESSAGE: usize = 4096;

struct Invite {
    secret: [u8; 16],
    expires: Instant,
}

/// One invite at a time; a new one replaces the old, using one consumes it.
static INVITE: Mutex<Option<Invite>> = Mutex::new(None);

/// Whether an unknown certificate may finish a handshake right now.
pub fn open() -> bool {
    INVITE.lock().unwrap().as_ref().is_some_and(|i| Instant::now() < i.expires)
}

/// What a server needs to describe itself in an invite.
#[derive(Clone)]
pub struct Me {
    pub name: String,
    pub port: u16,
    pub fingerprint: String,
}

/// The parts of a `glide join` token.
#[derive(Debug, PartialEq)]
pub struct Token {
    pub server: String,
    pub addrs: Vec<SocketAddr>,
    pub fingerprint: String,
    pub secret: [u8; 16],
}

impl Token {
    fn encode(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(|a| a.to_string()).collect();
        format!("{PREFIX}{SEP}{}{SEP}{}{SEP}{}{SEP}{}", self.server, addrs.join(","), self.fingerprint, hex(&self.secret))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.trim().split(SEP).collect();
        let [prefix, server, addrs, fingerprint, secret] = parts[..] else {
            bail!("that is not a glide invite");
        };
        ensure!(prefix == PREFIX, "this invite is for a different glide version ({prefix})");
        let addrs = addrs.split(',').map(|a| a.parse().with_context(|| format!("bad address {a}"))).collect::<Result<Vec<_>>>()?;
        ensure!(!addrs.is_empty(), "the invite has no address");
        let bytes = unhex(secret).context("bad secret in the invite")?;
        Ok(Self {
            server: server.to_string(),
            addrs,
            fingerprint: fingerprint.to_string(),
            secret: bytes.try_into().ok().context("bad secret length in the invite")?,
        })
    }
}

/// Make a fresh invite, valid for ten minutes, and return the command to run
/// on the new machine.
pub fn invite(me: &Me) -> Result<String> {
    let mut secret = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut secret)?;
    let addrs: Vec<SocketAddr> = local_ipv4s().into_iter().map(|ip| SocketAddr::new(ip.into(), me.port)).collect();
    ensure!(!addrs.is_empty(), "this machine has no network address to invite to");
    *INVITE.lock().unwrap() = Some(Invite { secret, expires: Instant::now() + LIFETIME });
    let token = Token { server: me.name.clone(), addrs, fingerprint: me.fingerprint.clone(), secret };
    info!("invite made, valid for ten minutes");
    Ok(format!("glide join {}", token.encode()))
}

/// Use the invite if this is its secret: right secret, not expired, not used.
fn redeem(secret: &[u8]) -> Result<()> {
    let mut invite = INVITE.lock().unwrap();
    let Some(current) = invite.as_ref() else { bail!("no invite is open, make a new one") };
    ensure!(Instant::now() < current.expires, "the invite expired, make a new one");
    // Every byte compared, whatever the first mismatch, so timing says nothing.
    let same = secret.len() == current.secret.len() && secret.iter().zip(current.secret).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0;
    ensure!(same, "wrong invite");
    *invite = None;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Request {
    secret: String,
    name: String,
    /// The port the newcomer's daemon listens on; the pairing connection
    /// itself comes from a throwaway one.
    port: u16,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    error: String,
    /// The side the server sits on, as the newcomer sees it.
    #[serde(default)]
    position: Option<Side>,
}

/// The server's half: one pairing connection, start to finish.
pub async fn accept(
    conn: quinn::Connection,
    me: &str,
    links: &Links,
    config_path: &Path,
    control: &UnboundedSender<crate::input::Control>,
    status: &Status,
) {
    let from = conn.remote_address();
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let reply = match pair(&conn, &mut recv, me, links, config_path, control, status).await {
        Ok(peer) => {
            info!(peer = %peer.name, %from, side = ?peer.position, "device paired");
            notify("Glide", &format!("{} added", peer.name));
            Reply { ok: true, error: String::new(), position: Some(peer.position.opposite()) }
        }
        Err(e) => {
            warn!(%from, "pairing refused: {e}");
            Reply { ok: false, error: e.to_string(), position: None }
        }
    };
    let _ = send.write_all(&serde_json::to_vec(&reply).unwrap_or_default()).await;
    let _ = send.finish();
    // Let the reply arrive before the connection goes.
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
}

async fn pair(
    conn: &quinn::Connection,
    recv: &mut quinn::RecvStream,
    me: &str,
    links: &Links,
    config_path: &Path,
    control: &UnboundedSender<crate::input::Control>,
    status: &Status,
) -> Result<Peer> {
    let bytes = recv.read_to_end(MAX_MESSAGE).await?;
    let request: Request = serde_json::from_slice(&bytes).context("malformed request")?;
    redeem(&unhex(&request.secret).context("malformed secret")?)?;
    let name = request.name.trim().to_string();
    ensure!(valid_name(&name), "the name {name:?} cannot be used");
    ensure!(name != me, "that is this machine's own name");
    let fingerprint = crate::link::peer_fingerprint(conn).context("no certificate")?;
    let addr = SocketAddr::new(conn.remote_address().ip(), request.port);
    let peer = crate::add_peer(config_path, &name, &fingerprint, addr, &crate::screens::local())?;
    links.upsert(peer.clone());
    status.set_peer(&name, |p| p.mode = "offline".into());
    let _ = control.send(crate::input::Control::AddPeer(peer.clone()));
    Ok(peer)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.contains(SEP) && name.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
}

/// The new machine's half: `glide join <token>`.
pub async fn join(token: &str, name: Option<String>, config_path: &Path, config_dir: &Path) -> Result<()> {
    let (me, server) = pair_with(token, name, config_path, config_dir).await?;
    println!("Paired {me} with {server}.");
    restart_service();
    Ok(())
}

/// Pair and write the config; returns this machine's name and the server's.
async fn pair_with(token: &str, name: Option<String>, config_path: &Path, config_dir: &Path) -> Result<(String, String)> {
    let token = Token::parse(token)?;
    let identity = crate::link::Identity::load_or_create(config_dir)?;
    let mut config = match std::fs::read_to_string(config_path) {
        Ok(_) => crate::load(config_path)?,
        Err(_) => crate::Config {
            name: name.clone().unwrap_or_else(hostname),
            role: crate::Role::Client,
            listen: crate::default_listen(),
            display: Default::default(),
            peers: vec![],
        },
    };
    ensure!(config.role == crate::Role::Client, "this machine is a server; only a client can join one");
    ensure!(valid_name(&config.name), "this machine's name {:?} cannot be used; pass --name", config.name);

    let mut server = Peer {
        name: token.server.clone(),
        addrs: token.addrs.clone(),
        fingerprint: token.fingerprint.clone(),
        position: Side::Right,
        display: None,
        enabled: true,
        control: false,
    };
    let endpoint = crate::link::pair_endpoint(&identity, server.clone())?;
    let mut last = anyhow::anyhow!("no address answered");
    let mut reached = None;
    for addr in &token.addrs {
        let attempt = async { anyhow::Ok(endpoint.connect(*addr, "glide")?.await?) };
        match tokio::time::timeout(Duration::from_secs(5), attempt).await {
            Ok(Ok(conn)) => {
                reached = Some((*addr, conn));
                break;
            }
            Ok(Err(e)) => last = e.context(format!("{addr}")),
            Err(_) => last = anyhow::anyhow!("{addr}: no answer"),
        }
    }
    let Some((addr, conn)) = reached else { return Err(last.context(format!("cannot reach {}", token.server))) };
    let (mut send, mut recv) = conn.open_bi().await?;
    let request = Request { secret: hex(&token.secret), name: config.name.clone(), port: config.listen.port() };
    send.write_all(&serde_json::to_vec(&request)?).await?;
    send.finish()?;
    let reply: Reply = serde_json::from_slice(&recv.read_to_end(MAX_MESSAGE).await?).context("the server's answer made no sense")?;
    conn.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    ensure!(reply.ok, "{} refused: {}", token.server, reply.error);

    // The address that answered goes first, so the daemon tries it first.
    server.addrs.retain(|a| *a != addr);
    server.addrs.insert(0, addr);
    server.position = reply.position.unwrap_or(Side::Right);
    config.peers.retain(|p| p.name != server.name);
    config.peers.push(server);
    std::fs::create_dir_all(config_path.parent().context("config path has no folder")?)?;
    std::fs::write(config_path, toml::to_string_pretty(&config)?)?;
    Ok((config.name, token.server))
}

/// A running client daemon reads its config only at start.
fn restart_service() {
    use std::process::Command;
    let active = Command::new("systemctl").args(["--user", "is-active", "--quiet", "glide.service"]).status();
    if active.is_ok_and(|s| s.success()) {
        let restarted = Command::new("systemctl").args(["--user", "restart", "glide.service"]).status();
        if restarted.is_ok_and(|s| s.success()) {
            println!("Restarted glide.service.");
            return;
        }
    }
    println!("Start glide to connect: `glide run` (or `systemctl --user restart glide.service`).");
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| h.trim().split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "glide-client".into())
}

/// Every IPv4 address this machine has, LAN first, Tailscale's 100.64/10 and
/// the like after. ponytail: no IPv6; add it when a peer has only that.
fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs hands back a list we only read, then free once.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut at = list;
        while !at.is_null() {
            let ifa = &*at;
            let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
            let loopback = ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
            if up && !loopback && !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_link_local() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
            at = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out.sort_by_key(|ip| !ip.is_private());
    out
}

/// A notification in the corner of the screen. ponytail: osascript, because
/// UserNotifications wants an entitlement an ad-hoc app does not have.
pub fn notify(title: &str, text: &str) {
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("display notification \"{}\" with title \"{}\"", quote(text), quote(title));
        let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (title, text);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_survives_the_paste() {
        let token = Token {
            server: "goldengate".into(),
            addrs: vec!["192.168.0.2:4242".parse().unwrap(), "100.64.0.9:4242".parse().unwrap()],
            fingerprint: "ab".repeat(32),
            secret: [7; 16],
        };
        let text = token.encode();
        assert!(!text.contains(char::is_whitespace), "one word: {text}");
        assert_eq!(Token::parse(&format!("  {text}\n")).unwrap(), token);
        assert!(Token::parse("glide2+a+1.2.3.4:1+ab+00").is_err(), "other versions are refused");
        assert!(Token::parse("hello").is_err());
    }

    /// One test, because the invite is process-wide state: the rules of an
    /// invite, then a real pairing over loopback, then the same invite again.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_invite_pairs_once_and_only_with_its_secret() -> Result<()> {
        use std::sync::Arc;

        let me = Me { name: "goldengate".into(), port: 4242, fingerprint: "ab".repeat(32) };
        let command = invite(&me).unwrap();
        let token = Token::parse(command.strip_prefix("glide join ").unwrap()).unwrap();
        assert!(open());
        assert!(redeem(&[0; 16]).is_err(), "a wrong secret is refused");
        assert!(open(), "a wrong guess does not use the invite up");
        redeem(&token.secret).unwrap();
        assert!(!open(), "used once, closed");
        assert!(redeem(&token.secret).is_err(), "and refused the second time");
        invite(&me).unwrap();
        INVITE.lock().unwrap().as_mut().unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(!open());
        let secret = INVITE.lock().unwrap().as_ref().unwrap().secret;
        assert!(redeem(&secret).is_err(), "an expired invite is refused");

        // A server with no devices yet, on loopback.
        let dir = std::env::temp_dir().join(format!("glide-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server_id = crate::link::Identity::load_or_create(&dir.join("server"))?;
        let server_cfg = dir.join("server/config.toml");
        let config = crate::Config {
            name: "goldengate".into(),
            role: crate::Role::Server,
            listen: "127.0.0.1:0".parse()?,
            display: Default::default(),
            peers: vec![],
        };
        std::fs::write(&server_cfg, toml::to_string_pretty(&config)?)?;
        let links = Arc::new(Links::new(vec![], true));
        let net = crate::link::endpoint(&config, &server_id, links.clone())?;
        let addr = net.endpoint.local_addr()?;
        let status = Arc::new(Status::at(dir.join("status")));
        let (control, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn({
            let (endpoint, links, server_cfg, status) = (net.endpoint.clone(), links.clone(), server_cfg.clone(), status.clone());
            async move {
                while let Some(incoming) = endpoint.accept().await {
                    if let Ok(conn) = incoming.await {
                        accept(conn, "goldengate", &links, &server_cfg, &control, &status).await;
                    }
                }
            }
        });

        let me = Me { name: "goldengate".into(), port: addr.port(), fingerprint: server_id.fingerprint() };
        let command = invite(&me)?;
        let mut token = Token::parse(command.strip_prefix("glide join ").unwrap())?;
        token.addrs = vec![addr];
        let text = token.encode();
        let client_cfg = dir.join("client/config.toml");
        let (name, server) = pair_with(&text, Some("newbox".into()), &client_cfg, &dir.join("client")).await?;
        assert_eq!((name.as_str(), server.as_str()), ("newbox", "goldengate"));

        let client_id = crate::link::Identity::load_or_create(&dir.join("client"))?;
        let saved = crate::load(&server_cfg)?;
        let added = saved.peers.iter().find(|p| p.name == "newbox").expect("the server saved the device");
        assert_eq!(added.fingerprint, client_id.fingerprint(), "the key comes from its certificate");
        assert!(links.peer("newbox").is_some(), "the running daemon trusts it at once");
        assert!(matches!(asked.recv().await, Some(crate::input::Control::AddPeer(p)) if p.name == "newbox"));
        let mine = crate::load(&client_cfg)?;
        assert_eq!(mine.role, crate::Role::Client);
        let theirs = &mine.peers[0];
        assert_eq!((theirs.name.as_str(), theirs.fingerprint.as_str()), ("goldengate", server_id.fingerprint().as_str()));
        assert_eq!(theirs.position, added.position.opposite(), "both ends agree which edges face each other");
        assert_eq!(theirs.addrs[0], addr);

        let again = pair_with(&text, Some("intruder".into()), &dir.join("other/config.toml"), &dir.join("other")).await;
        assert!(again.is_err(), "an invite works once");
        assert!(crate::load(&server_cfg)?.peers.iter().all(|p| p.name != "intruder"));

        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn names_that_would_break_the_config_are_refused() {
        assert!(valid_name("nullbyte"));
        assert!(valid_name("anton-2.lan"));
        assert!(!valid_name(""));
        assert!(!valid_name("a+b"));
        assert!(!valid_name("a\"b"));
        assert!(!valid_name(&"x".repeat(65)));
    }
}
