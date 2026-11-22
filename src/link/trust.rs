//! Who this machine is and whom it trusts: its certificate, the pinned
//! verifier, and the QUIC endpoints built on them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use quinn::{Connection, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

use super::{Links, ALPN, PAIR_ALPN, VIDEO_ALPN, VIDEO_SEND_WINDOW, VIEW_ALPN};
use crate::{Config, Peer};

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

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn parse_fingerprint(text: &str) -> Result<[u8; 32]> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trip() {
        let hexed = hex(&[0xab; 32]);
        assert_eq!(parse_fingerprint(&hexed).unwrap(), [0xab; 32]);
        assert!(parse_fingerprint("abcd").is_err());
    }
}
