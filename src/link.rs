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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use crate::proto::{Msg, Route};
use crate::status::Status;
use crate::{Config, Peer};

const ALPN: &[u8] = b"glide/0";
const VIDEO_ALPN: &[u8] = b"glide-video/0";
/// View Screen: the Mac dials the peer, and the peer streams its screen and
/// sound back.
const VIEW_ALPN: &[u8] = b"glide-view/0";
/// `glide join`: a machine that is not trusted yet, presenting an invite.
const PAIR_ALPN: &[u8] = b"glide-pair/0";
/// Close code for "this server switched you off". The client stops dialing
/// and shows it; the server dials again when switched back on.
pub const OFF: u32 = 1;
/// Close code for "this server forgot you".
pub const FORGOTTEN: u32 = 2;
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

/// Every peer this machine knows, and the ones it can currently talk to.
#[derive(Default)]
pub struct Links {
    peers: Mutex<HashMap<String, UnboundedSender<Msg>>>,
    conns: Mutex<HashMap<String, Connection>>,
    /// The configured peers, kept current as devices are paired and forgotten,
    /// so dialing, certificate checks and the clipboard all see the same list.
    roster: Mutex<Vec<Peer>>,
    /// Off. On a server: switched off by its owner, never dialed, refused.
    /// On a client: the server said so; dialed only once a minute to check.
    off: Mutex<HashSet<String>>,
    server: bool,
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
    fn note_addr(&self, name: &str, addr: SocketAddr) -> bool {
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

    fn trusts(&self, seen: &[u8; 32]) -> bool {
        self.roster.lock().unwrap().iter().any(|p| parse_fingerprint(&p.fingerprint).is_ok_and(|f| f == *seen))
    }

    /// Send to one peer. Silently does nothing when that peer is not connected:
    /// input for a machine that just dropped off has nowhere useful to go.
    pub fn send(&self, peer: &str, msg: Msg) {
        if let Some(tx) = self.peers.lock().unwrap().get(peer) {
            let _ = tx.send(msg);
        }
    }

    /// Claim the link to this peer. False when one already exists: both
    /// machines dial each other, and the loser of that race hangs up.
    fn try_register(&self, peer: &str, tx: UnboundedSender<Msg>) -> bool {
        let mut peers = self.peers.lock().unwrap();
        if peers.get(peer).is_some_and(|existing| !existing.is_closed()) {
            return false;
        }
        peers.insert(peer.to_string(), tx);
        true
    }

    /// Whether a live link to this peer already exists. The session asks before
    /// letting its capture take the cursor: grabbing it with nowhere to send it
    /// swallows the keyboard and mouse and leaves no way to reach the menu bar.
    pub fn connected(&self, peer: &str) -> bool {
        self.peers.lock().unwrap().get(peer).is_some_and(|tx| !tx.is_closed())
    }

    fn unregister(&self, peer: &str) {
        self.peers.lock().unwrap().remove(peer);
        self.conns.lock().unwrap().remove(peer);
    }
}

pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl Identity {
    /// Load this machine's certificate, generating one on first run.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        if cert_path.exists() && key_path.exists() {
            let cert = CertificateDer::from(std::fs::read(&cert_path)?);
            let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(std::fs::read(&key_path)?));
            return Ok(Self { cert, key });
        }
        let generated = rcgen::generate_simple_self_signed(vec!["glide".to_string()])?;
        let cert = generated.cert.der().to_vec();
        let key = generated.key_pair.serialize_der();
        std::fs::create_dir_all(dir)?;
        std::fs::write(&cert_path, &cert)?;
        write_private(&key_path, &key)?;
        Ok(Self {
            cert: CertificateDer::from(cert),
            key: PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key)),
        })
    }

    pub fn fingerprint(&self) -> String {
        hex(&Sha256::digest(&self.cert))
    }

    fn clone_parts(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (vec![self.cert.clone()], self.key.clone_key())
    }
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_fingerprint(text: &str) -> Result<[u8; 32]> {
    let clean: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() != 64 {
        return Err(anyhow!("fingerprint must be 64 hex characters, got {}", clean.len()));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

/// Accepts exactly the certificates of the peers in the roster, in both
/// directions: a peer proves who it is by its key, not by a CA. The one
/// exception is a machine connecting *to* us while an invite is open: it may
/// finish the handshake, but anything other than the pairing ALPN still drops
/// it in `identify`.
struct Pinned {
    links: Arc<Links>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl std::fmt::Debug for Pinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pinned")
    }
}

impl Pinned {
    fn check(&self, cert: &CertificateDer<'_>, incoming: bool) -> Result<(), rustls::Error> {
        let seen: [u8; 32] = Sha256::digest(cert).into();
        if self.links.trusts(&seen) || (incoming && crate::pair::open()) {
            Ok(())
        } else {
            Err(rustls::Error::General(format!("unknown peer certificate {}", hex(&seen))))
        }
    }

    fn tls12(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn tls13(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity, false)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity, true)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The endpoint, plus what it takes to dial a peer for video.
#[derive(Clone)]
pub struct Net {
    pub endpoint: Endpoint,
    video: quinn::ClientConfig,
    view: quinn::ClientConfig,
}

impl Net {
    /// A connection of its own for an extended display, to the first address
    /// that answers. Pinned exactly like the input link.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub async fn dial_video(&self, peer: &Peer) -> Result<Connection> {
        self.dial_with(&self.video, peer).await
    }

    /// The same, for View Screen: the peer sends on it, the Mac only reads.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub async fn dial_view(&self, peer: &Peer) -> Result<Connection> {
        self.dial_with(&self.view, peer).await
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    async fn dial_with(&self, config: &quinn::ClientConfig, peer: &Peer) -> Result<Connection> {
        let mut last = anyhow!("peer {} has no addresses", peer.name);
        for addr in &peer.addrs {
            match self
                .endpoint
                .connect_with(config.clone(), *addr, "glide")?
                .await
            {
                Ok(conn) => return Ok(conn),
                Err(e) => last = anyhow!("{addr}: {e}"),
            }
        }
        Err(last)
    }
}

pub fn endpoint(config: &Config, identity: &Identity, links: Arc<Links>) -> Result<Net> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // A typo in a fingerprint should stop the daemon loudly, not just make
    // that peer silently unreachable.
    for p in &config.peers {
        parse_fingerprint(&p.fingerprint).with_context(|| format!("peer {}", p.name))?;
    }
    let pinned = Arc::new(Pinned { links, provider: provider.clone() });

    let (certs, key) = identity.clone_parts();
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(pinned.clone())
        .with_single_cert(certs, key)?;
    server_crypto.alpn_protocols = vec![
        ALPN.to_vec(),
        VIDEO_ALPN.to_vec(),
        VIEW_ALPN.to_vec(),
        PAIR_ALPN.to_vec(),
    ];

    let (certs, key) = identity.clone_parts();
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(pinned)
        .with_client_auth_cert(certs, key)?;
    let (mut video_crypto, mut view_crypto) = (client_crypto.clone(), client_crypto.clone());
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];
    video_crypto.alpn_protocols = vec![VIDEO_ALPN.to_vec()];
    view_crypto.alpn_protocols = vec![VIEW_ALPN.to_vec()];

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    let mut video_transport = quinn::TransportConfig::default();
    video_transport.keep_alive_interval(Some(Duration::from_secs(2)));
    video_transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    video_transport.send_window(VIDEO_SEND_WINDOW);
    let (transport, video_transport) = (Arc::new(transport), Arc::new(video_transport));

    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?));
    server_config.transport_config(transport.clone());

    let mut endpoint = Endpoint::server(server_config, config.listen)?;
    let mut client_config =
        quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)?));
    client_config.transport_config(transport);
    endpoint.set_default_client_config(client_config);
    let mut video = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(video_crypto)?,
    ));
    video.transport_config(video_transport.clone());
    let mut view = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(view_crypto)?,
    ));
    view.transport_config(video_transport);
    Ok(Net {
        endpoint,
        video,
        view,
    })
}

/// Two endpoints on loopback that trust each other, for tests of the media
/// connections: the one that listens, the one that dials, and the listener as
/// the dialer's peer.
#[cfg(test)]
pub(crate) fn loopback(dir: &Path) -> Result<(Net, Net, Peer)> {
    use crate::{Role, Side};

    let (id_a, id_b) = (
        Identity::load_or_create(&dir.join("a"))?,
        Identity::load_or_create(&dir.join("b"))?,
    );
    let config = |peer: &Identity, addrs| Config {
        name: "me".into(),
        role: Role::Server,
        listen: "127.0.0.1:0".parse().unwrap(),
        display: Default::default(),
        peers: vec![Peer {
            name: "peer".into(),
            addrs,
            fingerprint: peer.fingerprint(),
            position: Side::Right,
            display: None,
            enabled: true,
            control: false,
        }],
    };
    let trusting = |c: &Config| Arc::new(Links::new(c.peers.clone(), false));
    let (here, there) = (config(&id_a, vec![]), config(&id_b, vec![]));
    let listener = endpoint(&here, &id_b, trusting(&here))?;
    let dialer = endpoint(&there, &id_a, trusting(&there))?;
    let peer = config(&id_b, vec![listener.endpoint.local_addr()?])
        .peers
        .remove(0);
    Ok((listener, dialer, peer))
}

/// What `glide join` dials with: its own certificate, the server pinned to the
/// fingerprint in the invite, the pairing ALPN, and a throwaway port so a
/// daemon already running here keeps its own.
pub fn pair_endpoint(identity: &Identity, server: Peer) -> Result<Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pinned = Arc::new(Pinned { links: Arc::new(Links::new(vec![server], false)), provider: provider.clone() });
    let (certs, key) = identity.clone_parts();
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(pinned)
        .with_client_auth_cert(certs, key)?;
    crypto.alpn_protocols = vec![PAIR_ALPN.to_vec()];
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    let mut config = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?));
    config.transport_config(Arc::new(transport));
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// The certificate fingerprint a connected peer presented.
pub(crate) fn peer_fingerprint(conn: &Connection) -> Option<String> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    Some(hex(&Sha256::digest(certs.first()?)))
}

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
    if !links.try_register(peer, tx.clone()) {
        debug!(%peer, "already linked, dropping the duplicate connection");
        return Ok(());
    }
    links.conns.lock().unwrap().insert(peer.to_string(), conn.clone());
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

    status.offline(peer);
    if links.is_off(peer) {
        status.set_peer(peer, |p| p.mode = "off".into());
    }
    links.unregister(peer);
    let _ = link_state.send((peer.to_string(), false));
    pinger.abort();
    writer.abort();
    // Whoever had the cursor, they no longer do; let the session put input back
    // where it belongs rather than leaving it pointed at a dead link.
    let _ = to_session.send((peer.to_string(), Msg::Leave));
    result
}

/// Put every outgoing message on the channel it belongs to. Clipboard items go
/// out on streams of their own, spawned, so a large one never delays the key
/// press queued behind it.
async fn write_loop(conn: Connection, mut send: SendStream, mut rx: UnboundedReceiver<Msg>) {
    while let Some(msg) = rx.recv().await {
        let bytes = msg.encode();
        let sent = match msg.route() {
            Route::Bulk => {
                send_bulk(conn.clone(), bytes);
                Ok(())
            }
            Route::Datagram => conn.send_datagram(Bytes::from(bytes)).map_err(Into::into),
            Route::Stream => write_frame(&mut send, &bytes).await,
        };
        if let Err(e) = sent {
            debug!("send failed: {e}");
            return;
        }
    }
}

/// One clipboard item, one stream. Dropping the handle after `finish` still
/// sends what is buffered, so this task ends as soon as the bytes are handed
/// to the connection.
fn send_bulk(conn: Connection, bytes: Vec<u8>) {
    tokio::spawn(async move {
        match conn.open_uni().await {
            Ok(mut stream) => {
                if let Err(e) = stream.write_all(&bytes).await {
                    debug!("clipboard send failed: {e}");
                } else {
                    let _ = stream.finish();
                }
            }
            Err(e) => debug!("cannot open a clipboard stream: {e}"),
        }
    });
}

/// Keys, buttons and handover, length-prefixed on the one reliable stream.
async fn frames(
    mut recv: RecvStream,
    peer: &str,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    loop {
        let frame = read_frame(&mut recv).await?;
        if frame.is_empty() {
            continue; // the stream-opening frame
        }
        match Msg::decode(&frame) {
            Ok(msg) => {
                let _ = to_session.send((peer.to_string(), msg));
            }
            Err(e) => debug!(%peer, "bad frame: {e}"),
        }
    }
}

/// A clipboard item per stream: the stream ending is the length, so nothing
/// has to be framed and nothing else waits on it.
async fn bulk(
    conn: &Connection,
    peer: &str,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    loop {
        let mut recv = conn.accept_uni().await?;
        let (peer, to_session) = (peer.to_string(), to_session.clone());
        tokio::spawn(async move {
            match recv.read_to_end(MAX_BULK).await {
                Ok(bytes) => match Msg::decode(&bytes) {
                    Ok(msg) => {
                        let _ = to_session.send((peer, msg));
                    }
                    Err(e) => debug!(%peer, "bad clipboard message: {e}"),
                },
                Err(e) => debug!(%peer, "clipboard stream failed: {e}"),
            }
        });
    }
}

/// Motion and pings, plus the running latency summary.
async fn datagrams(
    conn: &Connection,
    peer: &str,
    started: Instant,
    tx: &UnboundedSender<Msg>,
    status: &Status,
    to_session: &UnboundedSender<(String, Msg)>,
) -> Result<()> {
    let mut samples = Samples::new();
    let mut report = tokio::time::interval(STATS_INTERVAL);
    report.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            datagram = conn.read_datagram() => {
                match Msg::decode(&datagram?) {
                    Ok(Msg::Ping(t)) => { let _ = tx.send(Msg::Pong(t)); }
                    Ok(Msg::Pong(t)) => samples.push(round_trip(t, started)),
                    Ok(msg) => { let _ = to_session.send((peer.to_string(), msg)); }
                    Err(e) => debug!(%peer, "bad datagram: {e}"),
                }
            }
            _ = report.tick() => {
                if let Some(line) = samples.summary() {
                    info!(%peer, "{line}");
                    status.set_peer(peer, |p| p.rtt_ms = samples.p50());
                }
            }
        }
    }
}

/// `[u32 length][bytes]`. View Screen's sound uses it too.
pub(crate) async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len()).context("frame too large")?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Not cancel-safe: only ever awaited by a loop with nothing else to race it
/// against, [`frames`] or View Screen's sound.
pub(crate) async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(anyhow!("frame of {len} bytes is too large"));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

fn round_trip(sent: u64, started: Instant) -> f64 {
    let now = started.elapsed().as_nanos() as u64;
    now.saturating_sub(sent) as f64 / 1_000_000.0
}

async fn ping_loop(tx: UnboundedSender<Msg>, started: Instant) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    loop {
        ping.tick().await;
        if tx.send(Msg::Ping(started.elapsed().as_nanos() as u64)).is_err() {
            return;
        }
    }
}

struct Samples {
    values: Vec<f64>,
}

impl Samples {
    const fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn push(&mut self, ms: f64) {
        if self.values.len() == SAMPLE_WINDOW {
            self.values.remove(0);
        }
        self.values.push(ms);
    }

    /// The typical round trip, for the status file.
    fn p50(&self) -> Option<f64> {
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        sorted.get(sorted.len().checked_sub(1)? / 2).copied()
    }

    fn summary(&self) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        Some(format!(
            "rtt p50 {:.2}ms  p99 {:.2}ms  min {:.2}ms  max {:.2}ms  n={}",
            at(0.5),
            at(0.99),
            sorted[0],
            sorted[sorted.len() - 1],
            sorted.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trip() {
        let hexed = hex(&[0xab; 32]);
        assert_eq!(parse_fingerprint(&hexed).unwrap(), [0xab; 32]);
        assert!(parse_fingerprint("abcd").is_err());
    }

    #[test]
    fn round_trip_counts_from_the_send_time() {
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(round_trip(0, started) >= 5.0);
    }

    #[test]
    fn percentiles_and_window() {
        let mut s = Samples::new();
        assert!(s.summary().is_none());
        for i in 0..SAMPLE_WINDOW + 50 {
            s.push(i as f64);
        }
        assert_eq!(s.values.len(), SAMPLE_WINDOW);
        assert_eq!(s.values[0], 50.0);
        assert!(s.summary().unwrap().contains("p50"));
    }

    #[test]
    fn links_only_send_to_connected_peers() {
        let links = Links::default();
        links.send("ghost", Msg::Enter); // must not panic
        let (tx, mut rx) = unbounded_channel();
        assert!(links.try_register("mybox", tx));
        links.send("mybox", Msg::Enter);
        assert_eq!(rx.try_recv().unwrap(), Msg::Enter);
        let (second, _second_rx) = unbounded_channel();
        assert!(!links.try_register("mybox", second), "a second link must be refused");
        links.unregister("mybox");
        links.send("mybox", Msg::Leave);
        assert!(rx.try_recv().is_err());
    }

    /// A clipboard image must never be able to claim the reliable stream, or a
    /// key press waits behind sixteen megabytes.
    #[test]
    fn the_clipboard_never_shares_a_lane_with_input() {
        use crate::clipboard::Clip;
        use input_event::{Event, KeyboardEvent, PointerEvent};

        let image = Msg::Clipboard(Clip::Image { mime: "image/png".into(), bytes: vec![0; 64] });
        assert!(matches!(image.route(), Route::Bulk));
        assert!(matches!(Msg::Clipboard(Clip::Text("hi".into())).route(), Route::Bulk));

        let key = Msg::Input(Event::Keyboard(KeyboardEvent::Key { time: 0, key: 1, state: 1 }));
        assert!(matches!(key.route(), Route::Stream));
        assert!(matches!(Msg::Enter.route(), Route::Stream));

        let motion = Msg::Input(Event::Pointer(PointerEvent::Motion { time: 0, dx: 1.0, dy: 0.0 }));
        assert!(matches!(motion.route(), Route::Datagram));
        assert!(matches!(Msg::Ping(0).route(), Route::Datagram));
    }

    /// Off means off: a server refuses a switched-off device's link with a
    /// reason, and the device learns it is off instead of retrying blindly.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_switched_off_device_is_refused_and_told_why() -> Result<()> {
        use crate::{Role, Side};
        let dir = std::env::temp_dir().join(format!("glide-off-{}", std::process::id()));
        let (id_server, id_client) = (Identity::load_or_create(&dir.join("s"))?, Identity::load_or_create(&dir.join("c"))?);
        let config = |peer: &Identity, role, enabled| Config {
            name: "me".into(),
            role,
            listen: "127.0.0.1:0".parse().unwrap(),
            display: Default::default(),
            peers: vec![Peer { name: "peer".into(), addrs: vec![], fingerprint: peer.fingerprint(), position: Side::Right, display: None, enabled, control: false }],
        };
        let (server_cfg, client_cfg) = (config(&id_client, Role::Server, false), config(&id_server, Role::Client, true));
        let server_links = Arc::new(Links::new(server_cfg.peers.clone(), true));
        let client_links = Arc::new(Links::new(client_cfg.peers.clone(), false));
        assert!(server_links.is_off("peer"), "enabled = false in the config reads as off");
        let server = endpoint(&server_cfg, &id_server, server_links.clone())?.endpoint;
        let client = endpoint(&client_cfg, &id_client, client_links.clone())?.endpoint;
        let status = Status::at(dir.join("status"));
        let (to_session, _inbox) = unbounded_channel();
        let (linked, _states) = unbounded_channel();

        let (accepted, dialed) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async { client.connect(server.local_addr().unwrap(), "glide").unwrap().await.unwrap() },
        );
        serve(accepted, "peer", false, &server_links, &status, &to_session, &linked).await?;
        assert!(!server_links.connected("peer"), "the server never registers it");
        let _ = serve(dialed.clone(), "peer", true, &client_links, &status, &to_session, &linked).await;
        check_told_off(&dialed, "peer", &client_links, &status);
        assert!(client_links.is_off("peer"), "the client knows it was switched off");
        assert_eq!(status.snapshot().peers[0].mode, "off");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The bug this whole split exists for. A clipboard image spans thousands
    /// of packets, so while it was framed on the input stream every ping that
    /// landed mid-frame cancelled a `read_exact` that had already swallowed
    /// bytes, desynchronising the stream for the rest of the link — and until
    /// it finished, every key press waited behind the whole image.
    ///
    /// Two real endpoints over loopback, because nothing smaller would have
    /// caught it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_big_clipboard_item_neither_corrupts_nor_delays_input() -> Result<()> {
        use crate::clipboard::Clip;
        use crate::{Role, Side};
        use input_event::{Event, KeyboardEvent};

        const KEYS: u32 = 200;
        let dir = std::env::temp_dir().join(format!("glide-link-{}", std::process::id()));
        let (here, there) = (dir.join("here"), dir.join("there"));
        let (id_here, id_there) = (Identity::load_or_create(&here)?, Identity::load_or_create(&there)?);

        let config = |peer: &Identity, name: &str| Config {
            name: name.to_string(),
            role: Role::Server,
            listen: "127.0.0.1:0".parse().unwrap(),
            display: Default::default(),
            peers: vec![Peer {
                name: "peer".into(),
                addrs: vec![],
                fingerprint: peer.fingerprint(),
                position: Side::Right,
                display: None,
                enabled: true,
                control: false,
            }],
        };
        let (cfg_here, cfg_there) = (config(&id_there, "here"), config(&id_here, "there"));
        let out = Arc::new(Links::new(cfg_here.peers.clone(), false));
        let links = Arc::new(Links::new(cfg_there.peers.clone(), false));
        let sender = endpoint(&cfg_here, &id_here, out.clone())?.endpoint;
        let receiver = endpoint(&cfg_there, &id_there, links.clone())?.endpoint;
        let addr = receiver.local_addr()?;

        let (to_session, mut inbox) = unbounded_channel();
        let (unused, _drop) = unbounded_channel();
        let (linked, mut link_state) = unbounded_channel();
        let status = Arc::new(Status::at(dir.join("status")));

        // Both connections are built here rather than inside the tasks, so the
        // test keeps a handle it can close: aborting a `serve` leaves the
        // writer and pinger it spawned holding the connection open.
        let (accepted, dialed) = tokio::join!(
            async { receiver.accept().await.unwrap().await.unwrap() },
            async { sender.connect(addr, "glide").unwrap().await.unwrap() },
        );
        let hang_up = dialed.clone();
        let accepting = tokio::spawn({
            let (links, status, linked) = (links.clone(), status.clone(), linked.clone());
            async move {
                let _ = serve(accepted, "peer", false, &links, &status, &to_session, &linked).await;
            }
        });
        let dialing = tokio::spawn({
            let (out, status, linked) = (out.clone(), status.clone(), linked.clone());
            async move {
                let _ = serve(dialed, "peer", true, &out, &status, &unused, &linked).await;
            }
        });
        while !out.connected("peer") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Two megabytes of clipboard, then a burst of key presses right behind
        // it — the order that used to wedge the link.
        let bytes: Vec<u8> = (0..2_000_000u32).map(|i| i as u8).collect();
        let image = Clip::Image { mime: "image/png".into(), bytes: bytes.clone() };
        out.send("peer", Msg::Clipboard(image.clone()));
        for time in 0..KEYS {
            out.send("peer", Msg::Input(Event::Keyboard(KeyboardEvent::Key { time, key: 30, state: 1 })));
        }

        let (mut keys, mut clip) = (0u32, None);
        let collect = async {
            while keys < KEYS || clip.is_none() {
                match inbox.recv().await.expect("link died").1 {
                    Msg::Input(Event::Keyboard(KeyboardEvent::Key { time, .. })) => {
                        assert_eq!(time, keys, "a key arrived out of order or was lost");
                        keys += 1;
                    }
                    Msg::Clipboard(got) => {
                        // The lanes are independent, so keys sent after the
                        // image still overtake it. Sharing one stream put this
                        // at exactly zero; how far past zero is a race between
                        // two sockets and not worth pinning down.
                        assert!(keys > 0, "every key queued behind the clipboard");
                        clip = Some(got);
                    }
                    _ => {} // the screen list each side sends on connect
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), collect).await.expect("timed out");
        assert_eq!(clip, Some(image), "the clipboard arrived changed");

        // The session arms its screen edge on the first of these and drops it
        // again on the second. Without the second, an edge outlives the link
        // that justified it and starts grabbing the cursor for nobody.
        assert_eq!(link_state.recv().await, Some(("peer".to_string(), true)));
        hang_up.close(0u32.into(), b"test over");
        let down = async {
            while link_state.recv().await != Some(("peer".to_string(), false)) {}
        };
        tokio::time::timeout(Duration::from_secs(20), down).await.expect("no link-down signal");

        accepting.abort();
        dialing.abort();
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
