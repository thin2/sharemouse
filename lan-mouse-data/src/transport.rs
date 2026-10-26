//! TLS 1.3 accept/connect helpers and length-prefixed framing.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use rustls::{
    ClientConfig, ServerConfig,
    pki_types::{PrivateKeyDer, ServerName},
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector, client::TlsStream as ClientStream, server::TlsStream as ServerStream,
};

use crate::{
    crypto::{AuthorizedFingerprints, Identity, IdentityError, PinnedVerifier},
    protocol::{Frame, MAX_CHUNK_DATA, MAX_FRAME_SIZE, ProtocolError},
};

/// Time allowed for TCP, TLS, and the version handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("certificate error: {0}")]
    Identity(#[from] IdentityError),
    #[error("tls configuration error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("connection timed out")]
    Timeout,
    #[error("peer rejected the connection (unauthorized device?)")]
    Rejected,
}

/// A framed, authenticated data connection.
pub enum DataConnection {
    Client(ClientStream<TcpStream>),
    Server(ServerStream<TcpStream>),
}

impl DataConnection {
    pub(crate) fn into_stream(self) -> tokio_rustls::TlsStream<TcpStream> {
        match self {
            Self::Client(stream) => stream.into(),
            Self::Server(stream) => stream.into(),
        }
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Client(stream) => stream.get_ref().0.peer_addr(),
            Self::Server(stream) => stream.get_ref().0.peer_addr(),
        }
    }

    /// SHA-256 fingerprint of the peer certificate presented during the
    /// handshake. This is the same identifier used for device
    /// authorization, so it can key sessions and UI state.
    pub fn peer_fingerprint(&self) -> Option<String> {
        let certificate = match self {
            Self::Client(stream) => stream.get_ref().1.peer_certificates()?.first()?,
            Self::Server(stream) => stream.get_ref().1.peer_certificates()?.first()?,
        };
        Some(crate::crypto::certificate_fingerprint(certificate.as_ref()))
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let bytes = frame.encode()?;
        write_frame(self, &bytes).await
    }

    pub async fn recv(&mut self) -> Result<Frame, TransportError> {
        let bytes = read_frame(self).await?;
        Ok(Frame::decode(&bytes)?)
    }
}

/// Build the client configuration for the data channel.
pub fn client_config(
    identity: &Identity,
    authorized: &AuthorizedFingerprints,
) -> Result<Arc<ClientConfig>, TransportError> {
    let provider = rustls::crypto::ring::default_provider();
    let verifier = PinnedVerifier::new(authorized.clone(), Arc::new(provider.clone()), true);
    let key = PrivateKeyDer::try_from(identity.private_key.clone())
        .map_err(|_| TransportError::Rejected)?;
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![identity.certificate.clone()], key)?;
    Ok(Arc::new(config))
}

/// Build the server configuration for the data channel.
pub fn server_config(
    identity: &Identity,
    authorized: &AuthorizedFingerprints,
) -> Result<Arc<ServerConfig>, TransportError> {
    let provider = rustls::crypto::ring::default_provider();
    let verifier = PinnedVerifier::new(authorized.clone(), Arc::new(provider.clone()), true);
    let key = PrivateKeyDer::try_from(identity.private_key.clone())
        .map_err(|_| TransportError::Rejected)?;
    let config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(vec![identity.certificate.clone()], key)?;
    Ok(Arc::new(config))
}

/// Connect, complete the TLS handshake, and exchange version hellos.
pub async fn connect(
    addr: SocketAddr,
    identity: &Identity,
    authorized: &AuthorizedFingerprints,
) -> Result<DataConnection, TransportError> {
    let config = client_config(identity, authorized)?;
    let connector = TlsConnector::from(config);
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| TransportError::Timeout)??;
    tcp.set_nodelay(true)?;

    // The certificate is pinned by fingerprint, so the hostname is not
    // used for verification. A fixed value keeps SNI deterministic.
    let server_name = ServerName::try_from("lan-mouse").map_err(|_| TransportError::Rejected)?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| TransportError::Timeout)??;

    let mut connection = DataConnection::Client(stream);
    exchange_hello(&mut connection).await?;
    Ok(connection)
}

/// Accept one TCP connection and complete the authenticated handshake.
pub async fn accept(
    listener: &TcpListener,
    identity: &Identity,
    authorized: &AuthorizedFingerprints,
) -> Result<(DataConnection, SocketAddr), TransportError> {
    let (tcp, addr) = listener.accept().await?;
    tcp.set_nodelay(true)?;
    let config = server_config(identity, authorized)?;
    let acceptor = TlsAcceptor::from(config);
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, acceptor.accept(tcp))
        .await
        .map_err(|_| TransportError::Timeout)??;
    let mut connection = DataConnection::Server(stream);
    exchange_hello(&mut connection).await?;
    Ok((connection, addr))
}

async fn exchange_hello(connection: &mut DataConnection) -> Result<(), TransportError> {
    connection
        .send(&Frame::Control(crate::protocol::DataMessage::Hello {
            version: crate::protocol::PROTOCOL_VERSION,
        }))
        .await?;
    match tokio::time::timeout(CONNECT_TIMEOUT, connection.recv()).await {
        Ok(Ok(Frame::Control(crate::protocol::DataMessage::Hello { version })))
            if version == crate::protocol::PROTOCOL_VERSION =>
        {
            Ok(())
        }
        Ok(Ok(_)) => Err(TransportError::Protocol(
            ProtocolError::UnsupportedVersion {
                got: 0,
                expected: crate::protocol::PROTOCOL_VERSION,
            },
        )),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(TransportError::Timeout),
    }
}

/// Write one length-prefixed frame.
pub async fn write_frame(
    connection: &mut DataConnection,
    payload: &[u8],
) -> Result<(), TransportError> {
    match connection {
        DataConnection::Client(stream) => write_payload(stream, payload).await,
        DataConnection::Server(stream) => write_payload(stream, payload).await,
    }
}

pub(crate) async fn write_payload(
    stream: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
) -> Result<(), TransportError> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            len: payload.len(),
            max: MAX_FRAME_SIZE,
        }
        .into());
    }
    let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge {
        len: payload.len(),
        max: MAX_FRAME_SIZE,
    })?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame, rejecting oversized lengths before
/// allocating.
pub async fn read_frame(connection: &mut DataConnection) -> Result<Vec<u8>, TransportError> {
    match connection {
        DataConnection::Client(stream) => read_payload(stream).await,
        DataConnection::Server(stream) => read_payload(stream).await,
    }
}

pub(crate) async fn read_payload(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<Vec<u8>, TransportError> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        return Err(ProtocolError::Truncated.into());
    }
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            len,
            max: MAX_FRAME_SIZE,
        }
        .into());
    }
    let mut buffer = vec![0u8; len];
    stream.read_exact(&mut buffer).await?;
    Ok(buffer)
}

/// Maximum chunk size accepted by the framing layer.
pub const MAX_CHUNK: usize = MAX_CHUNK_DATA;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{AuthorizedFingerprints, Identity, certificate_fingerprint},
        protocol::{DataMessage, OfferedFile},
    };
    use std::collections::HashSet;

    fn allowed(fingerprints: &[String]) -> AuthorizedFingerprints {
        Arc::new(std::sync::RwLock::new(
            fingerprints.iter().cloned().collect::<HashSet<_>>(),
        ))
    }
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    fn identity(seed: u8) -> Identity {
        let names = [format!("device-{seed}")];
        let params = rcgen::CertificateParams::new(names).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let key = PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(
            key.serialize_der(),
        ));
        Identity {
            certificate: CertificateDer::from(cert.der().to_vec()),
            private_key: key.secret_der().to_vec(),
        }
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn authorized_peer_can_exchange_messages() {
        let server = identity(1);
        let client = identity(2);
        let authorized = allowed(&[certificate_fingerprint(&server.certificate)]);

        let (listener, addr) = listener().await;
        let server_identity = server.clone();
        let client_fingerprint = certificate_fingerprint(&client.certificate);
        let server_task = tokio::spawn(async move {
            accept(&listener, &server_identity, &allowed(&[client_fingerprint])).await
        });

        let mut connection = connect(addr, &client, &authorized).await.unwrap();
        connection
            .send(&Frame::Control(DataMessage::Message {
                id: "m-1".into(),
                text: "hello".into(),
            }))
            .await
            .unwrap();

        let (mut peer, _) = server_task.await.unwrap().unwrap();
        assert_eq!(
            peer.recv().await.unwrap(),
            Frame::Control(DataMessage::Message {
                id: "m-1".into(),
                text: "hello".into()
            })
        );
    }

    #[tokio::test]
    async fn unauthorized_client_is_rejected() {
        let server = identity(3);
        let stranger = identity(4);

        let (listener, addr) = listener().await;
        let server_identity = server.clone();
        let server_fingerprint = certificate_fingerprint(&server_identity.certificate);
        let server_task = tokio::spawn(async move {
            // Only the server's own fingerprint is allowed: the stranger is not.
            accept(&listener, &server_identity, &allowed(&[server_fingerprint])).await
        });

        let client_result = connect(
            addr,
            &stranger,
            &allowed(&[certificate_fingerprint(&server.certificate)]),
        )
        .await;
        assert!(client_result.is_err(), "unauthorized handshake must fail");
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn untrusted_server_is_rejected() {
        let server = identity(5);
        let client = identity(6);
        let stranger = identity(7);

        let (listener, addr) = listener().await;
        let server_identity = server.clone();
        let client_fingerprint = certificate_fingerprint(&client.certificate);
        let server_task = tokio::spawn(async move {
            accept(&listener, &server_identity, &allowed(&[client_fingerprint])).await
        });

        // Client pins a different server fingerprint.
        let client_result = connect(
            addr,
            &client,
            &allowed(&[certificate_fingerprint(&stranger.certificate)]),
        )
        .await;
        assert!(client_result.is_err(), "untrusted server must fail");
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn large_payload_roundtrip() {
        let server = identity(8);
        let client = identity(9);
        let authorized = allowed(&[certificate_fingerprint(&server.certificate)]);

        let (listener, addr) = listener().await;
        let server_identity = server.clone();
        let client_fingerprint = certificate_fingerprint(&client.certificate);
        let server_task = tokio::spawn(async move {
            accept(&listener, &server_identity, &allowed(&[client_fingerprint])).await
        });

        let mut connection = connect(addr, &client, &authorized).await.unwrap();
        let payload = vec![42u8; MAX_CHUNK];
        connection
            .send(&Frame::Chunk(crate::protocol::FileChunk {
                transfer_id: "t-1".into(),
                path: "payload.bin".into(),
                offset: 0,
                data: payload.clone(),
            }))
            .await
            .unwrap();

        let (mut peer, _) = server_task.await.unwrap().unwrap();
        assert_eq!(
            peer.recv().await.unwrap(),
            Frame::Chunk(crate::protocol::FileChunk {
                transfer_id: "t-1".into(),
                path: "payload.bin".into(),
                offset: 0,
                data: payload
            })
        );
    }

    #[tokio::test]
    async fn file_offer_roundtrip_over_connection() {
        let server = identity(10);
        let client = identity(11);
        let authorized = allowed(&[certificate_fingerprint(&server.certificate)]);

        let (listener, addr) = listener().await;
        let server_identity = server.clone();
        let client_fingerprint = certificate_fingerprint(&client.certificate);
        let server_task = tokio::spawn(async move {
            accept(&listener, &server_identity, &allowed(&[client_fingerprint])).await
        });

        let mut connection = connect(addr, &client, &authorized).await.unwrap();
        let offer = Frame::Control(DataMessage::FileOffer {
            transfer_id: "t-1".into(),
            files: vec![OfferedFile {
                path: "docs/report.pdf".into(),
                size: 10,
                is_dir: false,
                sha256: Some("0".repeat(64)),
            }],
        });
        connection.send(&offer).await.unwrap();

        let (mut peer, _) = server_task.await.unwrap().unwrap();
        assert_eq!(peer.recv().await.unwrap(), offer);
    }
}
