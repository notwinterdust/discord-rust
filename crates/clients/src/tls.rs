//! Optional rustls-backed TLS streams.
//!
//! This module is compiled only when the `tls` feature is enabled. It provides
//! the shared client configuration and a small stream wrapper used by both the
//! HTTPS REST transport and the WSS gateway connector.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::{ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;

/// Builds a rustls client configuration with the bundled web public roots.
///
/// The configuration uses the `ring` provider explicitly so it works even when
/// no process-wide provider has been installed.
#[must_use]
pub fn client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Resolves a host and opens a TCP connection with an optional timeout.
fn tcp_connect(host: &str, port: u16, timeout: Option<Duration>) -> Result<TcpStream, String> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?;

    let mut last_error = None;
    for address in addresses {
        let result = match timeout {
            Some(timeout) => TcpStream::connect_timeout(&address, timeout),
            None => TcpStream::connect(address),
        };
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.map_or_else(
        || format!("{host}:{port} did not resolve"),
        |error| error.to_string(),
    ))
}

/// A TLS byte stream backed by a [`TcpStream`].
pub struct TlsStream {
    inner: StreamOwned<ClientConnection, TcpStream>,
}

impl TlsStream {
    /// Connects to `host:port` and performs the TLS handshake.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when DNS resolution, the TCP connection,
    /// certificate validation or the TLS handshake fails.
    pub fn connect(host: &str, port: u16, timeout: Option<Duration>) -> Result<Self, String> {
        let config = client_config();
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|error| format!("invalid TLS server name `{host}`: {error}"))?;
        let tcp = tcp_connect(host, port, timeout)?;
        if let Some(timeout) = timeout {
            tcp.set_read_timeout(Some(timeout))
                .and_then(|_| tcp.set_write_timeout(Some(timeout)))
                .map_err(|error| error.to_string())?;
        }
        let connection = ClientConnection::new(config, server_name)
            .map_err(|error| format!("could not start TLS: {error}"))?;
        let mut stream = Self {
            inner: StreamOwned::new(connection, tcp),
        };

        // Drive the handshake now so callers get a connect error instead of an
        // unrelated I/O error on their first read or write.
        while stream.inner.conn.is_handshaking() {
            stream
                .inner
                .conn
                .complete_io(&mut stream.inner.sock)
                .map_err(|error| format!("TLS handshake failed: {error}"))?;
        }
        Ok(stream)
    }

    /// Sets the read timeout on the underlying TCP stream.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the timeout cannot be applied.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.sock.set_read_timeout(timeout)
    }

    /// Sets the write timeout on the underlying TCP stream.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the timeout cannot be applied.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.sock.set_write_timeout(timeout)
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl std::fmt::Debug for TlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::client_config;

    #[test]
    fn client_config_builds_without_a_global_provider() {
        let config = client_config();
        assert!(config.alpn_protocols.is_empty());
    }
}
