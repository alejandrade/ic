//! Public interface for a TLS-secured stream
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use async_trait::async_trait;
use core::fmt;
use ic_protobuf::registry::crypto::v1::X509PublicKeyCert;
use ic_types::registry::RegistryClientError;
use ic_types::{NodeId, RegistryVersion};
use openssl::hash::MessageDigest;
use openssl::x509::X509;
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize)]
/// An X.509 certificate
pub struct TlsPublicKeyCert {
    #[serde(skip_serializing)]
    cert: X509,
    // rename, to match previous serializations (which used X509PublicKeyCert)
    #[serde(rename = "certificate_der")]
    der_cached: Vec<u8>,
    #[serde(skip_serializing)]
    hash_cached: Vec<u8>,
}

impl TlsPublicKeyCert {
    /// Creates a certificate from ASN.1 DER encoding
    pub fn new_from_der(cert_der: Vec<u8>) -> Result<Self, TlsPublicKeyCertCreationError> {
        let cert = X509::from_der(&cert_der).map_err(|e| TlsPublicKeyCertCreationError {
            internal_error: format!("Error parsing DER: {}", e),
        })?;

        Ok(Self {
            hash_cached: Self::hash(&cert)?,
            cert,
            der_cached: cert_der,
        })
    }

    /// Creates a certificate from an existing OpenSSL struct
    pub fn new_from_x509(cert: X509) -> Result<Self, TlsPublicKeyCertCreationError> {
        let der_cached = cert.to_der().map_err(|e| TlsPublicKeyCertCreationError {
            internal_error: format!("Error encoding DER: {}", e),
        })?;

        Ok(Self {
            hash_cached: Self::hash(&cert)?,
            cert,
            der_cached,
        })
    }

    /// Returns the certificate in DER format
    pub fn as_der(&self) -> &Vec<u8> {
        &self.der_cached
    }

    /// Returns the certificate as an OpenSSL struct
    pub fn as_x509(&self) -> &X509 {
        &self.cert
    }

    /// Returns the certificate in protobuf format
    pub fn to_proto(&self) -> X509PublicKeyCert {
        X509PublicKeyCert {
            certificate_der: self.der_cached.clone(),
        }
    }

    fn hash(cert: &X509) -> Result<Vec<u8>, TlsPublicKeyCertCreationError> {
        let hash = cert
            .digest(MessageDigest::sha256())
            .map_err(|e| TlsPublicKeyCertCreationError {
                internal_error: format!("Error hashing certificate: {}", e),
            })?
            .iter()
            .cloned()
            .collect();
        Ok(hash)
    }
}

impl PartialEq for TlsPublicKeyCert {
    /// Equality is determined by comparison of the SHA256 hash byte arrays.
    fn eq(&self, rhs: &Self) -> bool {
        self.hash_cached == rhs.hash_cached
    }
}

impl Eq for TlsPublicKeyCert {}

impl Hash for TlsPublicKeyCert {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash_cached.hash(state)
    }
}

impl PartialOrd for TlsPublicKeyCert {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.hash_cached.partial_cmp(&other.hash_cached)
    }
}

impl Ord for TlsPublicKeyCert {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash_cached.cmp(&other.hash_cached)
    }
}

impl<'de> Deserialize<'de> for TlsPublicKeyCert {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de;

        // Only the `certificate_der` field is serialized for `TlsPublicKeyCert`.
        #[derive(Deserialize)]
        struct CertHelper {
            certificate_der: Vec<u8>,
        }

        let helper: CertHelper = Deserialize::deserialize(deserializer)?;
        TlsPublicKeyCert::new_from_der(helper.certificate_der).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Errors encountered during creation of a `TlsPublicKeyCert`.
pub struct TlsPublicKeyCertCreationError {
    pub internal_error: String,
}

impl Display for TlsPublicKeyCertCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for TlsPublicKeyCertCreationError {}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Errors from a TLS handshake performed as the server. Please refer to the
/// `TlsHandshake` method for detailed error variant descriptions.
pub enum TlsServerHandshakeError {
    RegistryError(RegistryClientError),
    CertificateNotInRegistry {
        node_id: NodeId,
        registry_version: RegistryVersion,
    },
    MalformedSelfCertificate {
        internal_error: String,
    },
    MalformedClientCertificate(MalformedPeerCertificateError),
    HandshakeError {
        internal_error: String,
    },
}

impl Display for TlsServerHandshakeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for TlsServerHandshakeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The certificate offered by the peer is malformed.
pub struct MalformedPeerCertificateError {
    pub internal_error: String,
}

impl MalformedPeerCertificateError {
    pub fn new(internal_error: &str) -> Self {
        Self {
            internal_error: internal_error.to_string(),
        }
    }
}

impl From<MalformedPeerCertificateError> for TlsServerHandshakeError {
    fn from(malformed_peer_cert_error: MalformedPeerCertificateError) -> Self {
        TlsServerHandshakeError::MalformedClientCertificate(malformed_peer_cert_error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Errors from a TLS handshake performed as the client. Please refer to the
/// `TlsHandshake` method for detailed error variant descriptions.
pub enum TlsClientHandshakeError {
    RegistryError(RegistryClientError),
    CertificateNotInRegistry {
        node_id: NodeId,
        registry_version: RegistryVersion,
    },
    MalformedSelfCertificate {
        internal_error: String,
    },
    MalformedServerCertificate(MalformedPeerCertificateError),
    HandshakeError {
        internal_error: String,
    },
}

impl Display for TlsClientHandshakeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for TlsClientHandshakeError {}

impl From<MalformedPeerCertificateError> for TlsClientHandshakeError {
    fn from(malformed_peer_cert_error: MalformedPeerCertificateError) -> Self {
        TlsClientHandshakeError::MalformedServerCertificate(malformed_peer_cert_error)
    }
}

/// A stream over a secure connection protected by TLS.
///
/// Implementing streams are expected to behave like a `BufWriter`. This means
/// that data written with `poll_write` are not guaranteed to be written to the
/// underlying (TCP) stream and one must call `poll_flush` at appropriate
/// times, such as when a period of `poll_write` writes is complete and there
/// is no more data to write. See also [tokio-rustls' documentation] on [Why do
/// I need to call poll_flush?] and the documentation of `tokio::io::BufWriter`
/// and `std::io::BufWriter`.
///
/// [tokio-rustls' documentation]: https://docs.rs/tokio-rustls/latest/tokio_rustls/
/// [Why do I need to call poll_flush?]: https://docs.rs/tokio-rustls/latest/tokio_rustls/#why-do-i-need-to-call-poll_flush
pub trait TlsStream: AsyncRead + AsyncWrite + Send + Unpin {}

#[async_trait]
/// Implementors provide methods for transforming TCP streams into TLS stream.
///
/// The TLS streams are returned as trait objects over a trait that does not
/// allow for extracting the secret keys of the underlying TLS session. This
/// is done because directly returning the underlying structs may allow for
/// extraction of the secret session keys.
pub trait TlsHandshake {
    /// Transforms a TCP stream into a TLS stream by first performing a TLS
    /// server handshake and then verifying that the authenticated peer is an
    /// allowed client.
    ///
    /// For the handshake, the server uses the following configuration:
    /// * Minimum protocol version: TLS 1.3
    /// * Supported signature algorithms: ed25519
    /// * Allowed cipher suites: TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384
    /// * Client authentication: mandatory, with ed25519 certificate
    /// * Maximum number of intermediate CA certificates: 1
    ///
    /// To determine whether the peer (that successfully performed the
    /// handshake) is an allowed client, the following steps are taken:
    /// 1. Determine the peer's node ID N_claimed from the _subject name_ of
    ///    the certificate C_handshake that the peer presented during the
    ///    handshake (and for which the peer therefore knows the private key).
    ///    Return an error if N_claimed is not contained in the nodes
    ///    in `allowed_clients`.
    /// 2. Determine the certificate C_registry by querying the registry for the
    ///    TLS certificate of node with ID N_claimed, and if C_registry is equal
    ///    to C_handshake, then the peer successfully authenticated as node
    ///    N_claimed.
    ///
    /// The given `tcp_stream` is consumed. If an error is returned, the TCP
    /// connection is therefore dropped.
    ///
    /// Returns the TLS stream together with the peer that successfully
    /// authenticated.
    ///
    /// # Errors
    /// * TlsServerHandshakeError::RegistryError if the registry cannot be
    ///   accessed.
    /// * TlsServerHandshakeError::CertificateNotInRegistry if a certificate
    ///   that is expected to be in the registry is not found.
    /// * TlsServerHandshakeError::MalformedSelfCertificate if the node's own
    ///   server certificate is malformed.
    /// * TlsServerHandshakeError::MalformedClientCertificate if a client
    ///   certificate corresponding to a client in `allowed_clients` is
    ///   malformed.
    /// * TlsServerHandshakeError::HandshakeError if there is an error during
    ///   the TLS handshake, or the handshake fails, e.g., if the node_id in the
    ///   subject CN of the client's certificate presented in the handshake is
    ///   not in `allowed_clients`, or if the client's certificate presented in
    ///   the handshake does not exactly match the client's certificate in the
    ///   registry.
    ///
    /// # Panics
    /// * If the secret key corresponding to the server certificate cannot be
    ///   found or is malformed in the server's secret key store. Note that this
    ///   is an error in the setup of the node and registry.
    async fn perform_tls_server_handshake(
        &self,
        tcp_stream: TcpStream,
        allowed_clients: AllowedClients,
        registry_version: RegistryVersion,
    ) -> Result<(Box<dyn TlsStream>, AuthenticatedPeer), TlsServerHandshakeError>;

    /// Transforms a TCP stream into a TLS stream by performing a TLS server
    /// handshake. No client authentication is performed.
    ///
    /// For the handshake, the server uses the following configuration:
    /// * Minimum protocol version: TLS 1.3
    /// * Supported signature algorithms: ed25519
    /// * Allowed cipher suites: TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384
    /// * Client authentication: no client authentication is performed
    ///
    /// Whenever the TLS handshake fails, this method returns an error.
    ///
    /// The given `tcp_stream` is consumed. If an error is returned, the TCP
    /// connection is therefore dropped.
    ///
    /// # Errors
    /// * TlsServerHandshakeError::RegistryError if the registry cannot be
    ///   accessed.
    /// * TlsServerHandshakeError::CertificateNotInRegistry if a certificate
    ///   that is expected to be in the registry is not found.
    /// * TlsServerHandshakeError::MalformedSelfCertificate if the node's own
    ///   server certificate is malformed.
    /// * TlsServerHandshakeError::HandshakeError if there is an error during
    ///   the TLS handshake, or the handshake fails.
    ///
    /// # Panics
    /// * If the secret key corresponding to the server certificate cannot be
    ///   found or is malformed in the server's secret key store. Note that this
    ///   is an error in the setup of the node and registry.
    async fn perform_tls_server_handshake_without_client_auth(
        &self,
        tcp_stream: TcpStream,
        registry_version: RegistryVersion,
    ) -> Result<Box<dyn TlsStream>, TlsServerHandshakeError>;

    /// Transforms a TCP stream into a TLS stream by first performing a TLS
    /// client handshake and then verifying that the peer is the given `server`.
    ///
    /// For the handshake, the client uses the following configuration:
    /// * Minimum protocol version: TLS 1.3
    /// * Supported signature algorithms: ed25519
    /// * Allowed cipher suites: TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384
    /// * Server authentication: mandatory, with ed25519 certificate
    ///
    /// To determine whether the peer (that successfully performed the
    /// handshake) is the `server`, the following steps are taken:
    /// 1. Determine the peer's node ID N_claimed from the _subject name_ of
    ///    the certificate C_handshake that the peer presented during the
    ///    handshake (and for which the peer therefore knows the private key).
    ///    Return an error if N_claimed is not the `server`.
    /// 2. Determine the certificate C_registry by querying the registry for the
    ///    TLS certificate of node with ID N_claimed. Return an error if the
    ///    C_registry does not equal C_handshake.
    ///
    /// The given `tcp_stream` is consumed. If an error is returned, the TCP
    /// connection is therefore dropped.
    ///
    /// # Errors
    /// * TlsClientHandshakeError::RegistryError if the registry cannot be
    ///   accessed.
    /// * TlsClientHandshakeError::CertificateNotInRegistry if a certificate
    ///   that is expected to be in the registry is not found.
    /// * TlsClientHandshakeError::MalformedSelfCertificate if the node's own
    ///   client certificate is malformed.
    /// * TlsClientHandshakeError::MalformedServerCertificate if the server
    ///   certificate obtained from the registry (as specified by `server)` is
    ///   malformed.
    /// * TlsClientHandshakeError::HandshakeError if there is an error during
    ///   the TLS handshake, or the handshake fails, e.g., if the node_id in the
    ///   subject CN of the server's certificate presented in the handshake does
    ///   not equal `server`, or if the server's certificate presented in the
    ///   handshake does not exactly match the `server`'s certificate in the
    ///   registry.
    ///
    /// # Panics
    /// * If the secret key corresponding to the client certificate cannot be
    ///   found or is malformed in the client's secret key store. Note that this
    ///   is an error in the setup of the node and registry.
    async fn perform_tls_client_handshake(
        &self,
        tcp_stream: TcpStream,
        server: NodeId,
        registry_version: RegistryVersion,
    ) -> Result<Box<dyn TlsStream>, TlsClientHandshakeError>;
}

#[derive(Clone, Debug)]
/// A list of allowed TLS peers, which can be `All` to allow any node to connect.
pub struct AllowedClients {
    nodes: SomeOrAllNodes,
}

impl AllowedClients {
    pub fn new(nodes: SomeOrAllNodes) -> Result<Self, AllowedClientsError> {
        let allowed_clients = Self { nodes };
        Self::ensure_clients_not_empty(&allowed_clients)?;
        Ok(allowed_clients)
    }

    /// Create an `AllowedClients` with a set of nodes.
    pub fn new_with_nodes(node_ids: BTreeSet<NodeId>) -> Result<Self, AllowedClientsError> {
        Self::new(SomeOrAllNodes::Some(node_ids))
    }

    /// Access the allowed nodes.
    pub fn nodes(&self) -> &SomeOrAllNodes {
        &self.nodes
    }

    fn ensure_clients_not_empty(candidate: &Self) -> Result<(), AllowedClientsError> {
        match &candidate.nodes {
            SomeOrAllNodes::Some(node_ids) => {
                if node_ids.is_empty() {
                    return Err(AllowedClientsError::ClientsEmpty);
                }
            }
            SomeOrAllNodes::All => { /* All is considered non-empty */ }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// The allowed clients could not be created.
pub enum AllowedClientsError {
    /// Attempted to create an `AllowedClients` with a malformed certificate
    /// protobuf.
    MalformedCertProto { internal_error: String },
    /// Attempted to create an `AllowedClients` with `Some` clients
    /// but empty nodes and certificates lists.
    ClientsEmpty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A list of node IDs, or "all nodes"
pub enum SomeOrAllNodes {
    Some(BTreeSet<NodeId>),
    All,
}

impl SomeOrAllNodes {
    pub fn new_with_single_node(node_id: NodeId) -> Self {
        let mut nodes = BTreeSet::new();
        nodes.insert(node_id);
        Self::Some(nodes)
    }

    pub fn contains(&self, node_id: NodeId) -> bool {
        match self {
            SomeOrAllNodes::Some(node_ids) => node_ids.contains(&node_id),
            SomeOrAllNodes::All => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
/// An authenticated Node ID
pub enum AuthenticatedPeer {
    /// Authenticated Node ID
    Node(NodeId),
}
