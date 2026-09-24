//! TLS for Postgres connections: a `tokio_postgres` `MakeTlsConnect` over
//! rustls (ring provider — the same stack the rest of the workspace already
//! links, so no second crypto library comes along).
//!
//! Two flavours, matching libpq's sslmodes:
//! - [`PgSsl::Prefer`] / [`PgSsl::Require`] encrypt WITHOUT checking the
//!   server certificate. That is libpq's meaning of those modes, and what
//!   psql does against the typical self-signed server certificate; the
//!   connection is protected from eavesdropping, not from an active MITM.
//! - [`PgSsl::VerifyFull`] checks the chain against the OS trust store
//!   (Windows certificate store via `rustls-native-certs`) and the host
//!   name against the certificate.
//!
//! Channel binding (SCRAM-SHA-256-PLUS) is not offered: the stream reports
//! `ChannelBinding::none()`, so SCRAM falls back to plain SCRAM-SHA-256 —
//! which every server accepts unless `channel_binding=require` is forced on
//! the client, which this app never does.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect};

/// The sslmode a connection is opened with. Mirrors libpq; `verify-ca` is
/// deliberately absent (it needs a CA file setting nobody asked for).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PgSsl {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

impl PgSsl {
    /// What `tokio_postgres` itself is told: it only knows whether to ask
    /// for TLS and whether falling back to plaintext is allowed.
    pub(crate) fn protocol_mode(self) -> tokio_postgres::config::SslMode {
        use tokio_postgres::config::SslMode;
        match self {
            PgSsl::Disable => SslMode::Disable,
            PgSsl::Prefer => SslMode::Prefer,
            PgSsl::Require | PgSsl::VerifyFull => SslMode::Require,
        }
    }
}

/// `libpq` asks for this ALPN since PG 17; a PG 17 server that sees any
/// OTHER protocol offered refuses the handshake, one that sees none (or
/// this one) is happy — so sending it is the only always-safe choice.
const ALPN_POSTGRESQL: &[u8] = b"postgresql";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts any server certificate, but still checks that the handshake
/// was signed by the key IN that certificate — i.e. exactly libpq's
/// `sslmode=require` posture.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn no_verify_config() -> Result<Arc<ClientConfig>, String> {
    static CFG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CFG.get_or_init(|| {
        let provider = provider();
        let mut cfg = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![ALPN_POSTGRESQL.to_vec()];
        Ok(Arc::new(cfg))
    })
    .clone()
}

fn verify_full_config() -> Result<Arc<ClientConfig>, String> {
    // Loading the Windows store walks every certificate in it — done once
    // per process, not once per connect.
    static CFG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CFG.get_or_init(|| {
        let loaded = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
        if added == 0 {
            let why = loaded.errors.first().map(|e| format!(": {e}")).unwrap_or_default();
            return Err(format!(
                "verify-full: nepodařilo se načíst důvěryhodné certifikáty systému{why}"
            ));
        }
        let mut cfg = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![ALPN_POSTGRESQL.to_vec()];
        Ok(Arc::new(cfg))
    })
    .clone()
}

/// The `MakeTlsConnect` handed to `tokio_postgres`; the cancel path reuses
/// the same value, so a cancel request travels the same way the session
/// did.
///
/// `Disable` still carries a (no-verify) config: `tokio_postgres` calls
/// `make_tls_connect` for EVERY connection before it looks at sslmode, so
/// refusing there would fail plaintext connects too. Under `Disable` the
/// connector is built and never used.
#[derive(Clone)]
pub struct MakeRustlsConnect {
    config: Arc<ClientConfig>,
}

impl MakeRustlsConnect {
    pub fn new(ssl: PgSsl) -> Result<Self, String> {
        let config = match ssl {
            PgSsl::Disable | PgSsl::Prefer | PgSsl::Require => no_verify_config()?,
            PgSsl::VerifyFull => verify_full_config()?,
        };
        Ok(Self { config })
    }
}

impl<S> MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    /// Never fails: a host rustls cannot use as a TLS name (one with a space, say)
    /// must still connect under `disable`, so the bad name is carried along
    /// and only reported if a handshake is actually attempted.
    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, io::Error> {
        let server_name = ServerName::try_from(domain.to_string())
            .map_err(|_| format!("neplatný název serveru pro TLS: {domain}"));
        Ok(RustlsConnect { config: self.config.clone(), server_name })
    }
}

pub struct RustlsConnect {
    config: Arc<ClientConfig>,
    server_name: Result<ServerName<'static>, String>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let connector = tokio_rustls::TlsConnector::from(self.config);
        Box::pin(async move {
            let name = self
                .server_name
                .map_err(|m| io::Error::new(io::ErrorKind::InvalidInput, m))?;
            let tls = connector.connect(name, stream).await?;
            Ok(RustlsStream(Box::new(tls)))
        })
    }
}

/// Boxed so the (large) rustls session state doesn't bloat the future
/// `tokio_postgres` keeps inline.
pub struct RustlsStream<S>(Box<tokio_rustls::client::TlsStream<S>>);

impl<S> tokio_postgres::tls::TlsStream for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mode_maps_verify_full_to_require() {
        use tokio_postgres::config::SslMode;
        assert_eq!(PgSsl::Disable.protocol_mode(), SslMode::Disable);
        assert_eq!(PgSsl::Prefer.protocol_mode(), SslMode::Prefer);
        assert_eq!(PgSsl::Require.protocol_mode(), SslMode::Require);
        // verify-full must never silently fall back to plaintext.
        assert_eq!(PgSsl::VerifyFull.protocol_mode(), SslMode::Require);
    }

    #[test]
    fn disable_still_hands_out_a_connector() {
        // tokio_postgres asks for one before it checks sslmode — refusing
        // here would break plaintext connects (found the hard way).
        let mut make = MakeRustlsConnect::new(PgSsl::Disable).unwrap();
        assert!(<MakeRustlsConnect as MakeTlsConnect<tokio::net::TcpStream>>::make_tls_connect(
            &mut make, "localhost"
        )
        .is_ok());
    }

    #[test]
    fn prefer_and_require_build_a_config_that_offers_postgresql_alpn() {
        for ssl in [PgSsl::Prefer, PgSsl::Require] {
            let cfg = MakeRustlsConnect::new(ssl).unwrap().config;
            assert_eq!(cfg.alpn_protocols, vec![b"postgresql".to_vec()]);
        }
    }

    #[test]
    fn verify_full_loads_the_system_store() {
        // Every Windows install has root certificates; an empty store here
        // would mean verify-full could never succeed anywhere.
        let cfg = MakeRustlsConnect::new(PgSsl::VerifyFull).unwrap().config;
        assert_eq!(cfg.alpn_protocols, vec![b"postgresql".to_vec()]);
    }

    fn connector_for(host: &str) -> RustlsConnect {
        let mut make = MakeRustlsConnect::new(PgSsl::Require).unwrap();
        <MakeRustlsConnect as MakeTlsConnect<tokio::net::TcpStream>>::make_tls_connect(&mut make, host)
            .unwrap()
    }

    #[test]
    fn ip_address_host_is_a_valid_tls_server_name() {
        assert!(connector_for("10.203.8.4").server_name.is_ok());
    }

    #[test]
    fn a_name_rustls_rejects_is_deferred_not_fatal() {
        // Must not fail make_tls_connect, or `disable` would break too.
        let c = connector_for("bad host");
        assert!(c.server_name.unwrap_err().contains("bad host"));
    }
}
