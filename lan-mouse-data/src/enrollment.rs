//! Restricted enrollment transport. No input, files or clipboard messages are
//! accepted here. Both users must approve the certificate fingerprints before
//! the caller may install a group or use the normal pinned data transport.
use crate::{Identity, crypto::PinnedVerifier};
use rustls::pki_types::{PrivateKeyDer, ServerName};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

pub const PORT: u16 = 4244;
pub const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub fingerprint: String,
    pub ip: Ipv4Addr,
    pub input_port: u16,
    pub data_port: u16,
}
impl Peer {
    pub fn valid(&self) -> bool {
        valid_fingerprint(&self.fingerprint)
            && !self.ip.is_unspecified()
            && !self.ip.is_multicast()
            && !self.ip.is_broadcast()
            && self.input_port != 0
            && self.data_port != 0
    }
}
pub fn valid_fingerprint(value: &str) -> bool {
    value.len() == 95
        && value.split(':').count() == 32
        && value.split(':').all(|part| {
            part.len() == 2
                && part
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub leader: String,
    pub revision: u64,
    pub peers: Vec<Peer>,
}
impl Group {
    pub fn valid(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.peers.len() > 3 {
            return Err("The connection group is full (maximum 3 devices, including the host)");
        }
        if self.peers.is_empty()
            || self.revision == 0
            || !self.peers.iter().all(Peer::valid)
            || !self.peers.iter().any(|p| p.fingerprint == self.leader)
        {
            return Err("Invalid connection group identity, address or port");
        }
        if self
            .peers
            .iter()
            .map(|p| &p.fingerprint)
            .collect::<HashSet<_>>()
            .len()
            != self.peers.len()
        {
            return Err("The connection group contains duplicate device identities");
        }
        if self
            .peers
            .iter()
            .map(|p| (p.ip, p.data_port))
            .collect::<HashSet<_>>()
            .len()
            != self.peers.len()
            || self
                .peers
                .iter()
                .map(|p| (p.ip, p.input_port))
                .collect::<HashSet<_>>()
                .len()
                != self.peers.len()
        {
            return Err(
                "Devices have the same IP address and port; use the host address on the joining device's network (for a Windows hotspot, use its hotspot address)",
            );
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub enum Packet {
    Hello {
        version: u16,
        input_port: u16,
        data_port: u16,
    },
    Decision(bool),
    Group(Group),
    Error(String),
}
pub struct Connection {
    stream: TlsStream<TcpStream>,
    pub fingerprint: String,
    pub remote: SocketAddr,
    pub local: SocketAddr,
}
impl Connection {
    fn new(stream: TlsStream<TcpStream>) -> io::Result<Self> {
        let (tcp, tls) = stream.get_ref();
        let certificate = tls
            .peer_certificates()
            .and_then(|c| c.first())
            .ok_or_else(invalid)?;
        let fingerprint = crate::certificate_fingerprint(certificate.as_ref());
        let remote = tcp.peer_addr()?;
        let local = tcp.local_addr()?;
        Ok(Self {
            stream,
            fingerprint,
            remote,
            local,
        })
    }
    pub async fn send(&mut self, packet: &Packet) -> io::Result<()> {
        let bytes = serde_json::to_vec(packet)?;
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        self.stream.write_u32(bytes.len() as u32).await?;
        self.stream.write_all(&bytes).await
    }
    pub async fn recv(&mut self) -> io::Result<Packet> {
        let len = self.stream.read_u32().await? as usize;
        if len > 4096 {
            return Err(invalid());
        }
        let mut bytes = vec![0; len];
        self.stream.read_exact(&mut bytes).await?;
        serde_json::from_slice(&bytes).map_err(Into::into)
    }
    pub async fn hello(&mut self, input_port: u16, data_port: u16) -> io::Result<Peer> {
        self.send(&Packet::Hello {
            version: crate::PROTOCOL_VERSION,
            input_port,
            data_port,
        })
        .await?;
        let Packet::Hello {
            version,
            input_port,
            data_port,
        } = self.recv().await?
        else {
            return Err(invalid());
        };
        let std::net::IpAddr::V4(ip) = self.remote.ip() else {
            return Err(invalid());
        };
        let peer = Peer {
            fingerprint: self.fingerprint.clone(),
            ip,
            input_port,
            data_port,
        };
        if version != crate::PROTOCOL_VERSION || !peer.valid() {
            return Err(invalid());
        }
        Ok(peer)
    }
    pub async fn confirm(&mut self, approved: bool) -> io::Result<()> {
        self.send(&Packet::Decision(approved)).await?;
        if !approved || !matches!(self.recv().await?, Packet::Decision(true)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pairing declined",
            ));
        }
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid enrollment message")
}
fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

pub async fn connect(addr: SocketAddr, identity: &Identity) -> io::Result<Connection> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(error)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier::pairing(provider)))
        .with_client_auth_cert(
            vec![identity.certificate.clone()],
            PrivateKeyDer::try_from(identity.private_key.clone()).map_err(error)?,
        )
        .map_err(error)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        let tcp = TcpStream::connect(addr).await?;
        let tls = TlsConnector::from(Arc::new(config))
            .connect(
                ServerName::try_from("sharemouse-enrollment").map_err(error)?,
                tcp,
            )
            .await?;
        Connection::new(tls.into())
    })
    .await
    .map_err(error)?
}
pub async fn accept(tcp: TcpStream, identity: &Identity) -> io::Result<Connection> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(error)?
        .with_client_cert_verifier(Arc::new(PinnedVerifier::pairing(provider)))
        .with_single_cert(
            vec![identity.certificate.clone()],
            PrivateKeyDer::try_from(identity.private_key.clone()).map_err(error)?,
        )
        .map_err(error)?;
    let tls = tokio::time::timeout(
        Duration::from_secs(10),
        TlsAcceptor::from(Arc::new(config)).accept(tcp),
    )
    .await
    .map_err(error)??;
    Connection::new(tls.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["enrollment-test".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        Identity {
            certificate: cert.der().clone(),
            private_key: key.serialize_der(),
        }
    }

    async fn connections() -> (Connection, Connection) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = identity();
        let b = identity();
        let (client, server) = tokio::join!(connect(listener.local_addr().unwrap(), &a), async {
            accept(listener.accept().await.unwrap().0, &b).await
        },);
        let client = client.unwrap();
        let server = server.unwrap();
        assert_eq!(client.fingerprint, b.fingerprint());
        assert_eq!(server.fingerprint, a.fingerprint());
        (client, server)
    }

    #[tokio::test]
    async fn both_certificates_and_both_decisions_are_required() {
        let (mut a, mut b) = connections().await;
        let (peer_b, peer_a) = tokio::join!(a.hello(4242, 4243), b.hello(5242, 5243));
        assert_eq!(peer_b.unwrap().input_port, 5242);
        assert_eq!(peer_a.unwrap().ip, Ipv4Addr::LOCALHOST);
        let (a, b) = tokio::join!(a.confirm(true), b.confirm(false));
        assert!(a.is_err());
        assert!(b.is_err());
        let (mut a, mut b) = connections().await;
        let (a, b) = tokio::join!(a.confirm(true), b.confirm(true));
        assert!(a.is_ok() && b.is_ok());
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_body_allocation() {
        let (mut a, mut b) = connections().await;
        a.stream.write_u32(u32::MAX).await.unwrap();
        assert_eq!(
            b.recv().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn group_limits_and_endpoint_uniqueness() {
        let peers: Vec<_> = (1..=4)
            .map(|n| Peer {
                fingerprint: format!("{n:02x}:").repeat(31) + &format!("{n:02x}"),
                ip: Ipv4Addr::new(192, 168, 1, n),
                input_port: 4242,
                data_port: 4243,
            })
            .collect();
        let mut group = Group {
            leader: peers[0].fingerprint.clone(),
            revision: 1,
            peers: peers[..3].to_vec(),
        };
        assert!(group.valid());
        group.peers.push(peers[3].clone());
        assert!(!group.valid());
        assert!(group.validate().unwrap_err().contains("maximum 3 devices"));
        group.peers.pop();
        group.peers[2].ip = group.peers[1].ip;
        assert!(!group.valid());
        assert!(group
            .validate()
            .unwrap_err()
            .contains("same IP address and port"));
        group.peers[2].data_port += 1;
        assert!(group
            .validate()
            .unwrap_err()
            .contains("same IP address and port"));
        group.peers[2].input_port += 1;
        assert!(group.validate().is_ok());
        group.peers[2].data_port = group.peers[1].data_port;
        assert!(group
            .validate()
            .unwrap_err()
            .contains("same IP address and port"));
        group.peers[2] = peers[2].clone();
        group.peers[2].fingerprint = group.peers[1].fingerprint.clone();
        assert!(!group.valid());
        assert!(group
            .validate()
            .unwrap_err()
            .contains("duplicate device identities"));
        group.peers[2] = peers[2].clone();
        group.leader = peers[3].fingerprint.clone();
        assert!(!group.valid());
    }
}
