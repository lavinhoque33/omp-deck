//! In-process TLS (`--tls-cert`/`--tls-key`): rustls 0.23 on ring, handshakes off the accept
//! loop so a slow client never stalls others. Each connection reserves its connection slot before
//! the handshake starts, so stalled handshakes count against the connection caps.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::RelayError;
use crate::transport::{Guarded, Limits, refuse};

/// Loads a PEM certificate chain and private key (PKCS#8, PKCS#1 or SEC1).
pub fn load_tls(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>, RelayError> {
    let tls = |path: &Path, e: &dyn std::fmt::Display| {
        RelayError::Tls(format!("{}: {e}", path.display()))
    };
    let chain = CertificateDer::pem_file_iter(cert)
        .map_err(|e| tls(cert, &e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls(cert, &e))?;
    if chain.is_empty() {
        return Err(tls(cert, &"no certificate found"));
    }
    let key_der = PrivateKeyDer::from_pem_file(key).map_err(|e| tls(key, &e))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| RelayError::Tls(e.to_string()))?
    .with_no_client_auth()
    .with_single_cert(chain, key_der)
    .map_err(|e| RelayError::Tls(e.to_string()))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Accepts TCP connections and yields them once their TLS handshake finished.
pub(crate) struct TlsListener {
    local: SocketAddr,
    ready: mpsc::Receiver<(Guarded<TlsStream<TcpStream>>, SocketAddr)>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl TlsListener {
    pub fn new(
        listener: TcpListener,
        config: Arc<rustls::ServerConfig>,
        limits: Arc<Limits>,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> std::io::Result<TlsListener> {
        let local = listener.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, ready) = mpsc::channel(64);
        let accept_task = tokio::spawn(async move {
            loop {
                let (tcp, addr) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // Usually EMFILE: back off instead of spinning.
                        log::warn!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Reserved before the handshake task exists; dropping `tcp` closes the socket.
                let Some(slot) = limits.accept(addr.ip()) else {
                    refuse(addr);
                    continue;
                };
                let _ = tcp.set_nodelay(true);
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => {
                            let _ = tx
                                .send((Guarded::new(stream, slot, request_timeout), addr))
                                .await;
                        }
                        Ok(Err(e)) => log::debug!("TLS handshake from {addr} failed: {e}"),
                        Err(_) => log::debug!("TLS handshake from {addr} timed out"),
                    }
                });
            }
        });
        Ok(TlsListener {
            local,
            ready,
            accept_task,
        })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl Listener for TlsListener {
    type Io = Guarded<TlsStream<TcpStream>>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(c) => c,
            // The accept task only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}
