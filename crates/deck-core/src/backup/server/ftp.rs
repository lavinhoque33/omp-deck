//! FTP and explicit FTPS (`AUTH TLS`) through `suppaftp`'s blocking client: passive mode with the
//! data connection dialled to the control connection's address (NAT-safe, no FTP bounce), binary
//! type, rustls on ring. FTPS trusts the WebPKI roots or, failing that, a pinned leaf certificate
//! (trust on first use).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};
use suppaftp::types::FileType;
use suppaftp::{
    FtpError, FtpStream, ImplFtpStream, Mode, RustlsConnector, RustlsFtpStream, TlsStream,
};

use super::layout::Transport;
use super::{ServerAuth, ServerConfig};
use crate::backup::{CONNECT_TIMEOUT, Error, Result, STALL_TIMEOUT};

/// `NLST` answers for a folder that is empty on some servers.
const NO_FILES: [u32; 2] = [450, 550];

struct Ftp<T: TlsStream> {
    stream: ImplFtpStream<T>,
}

/// Connects, upgrades to TLS when `secure` (FTPS), logs in and enters (creating) the folder.
pub(super) fn connect(config: &ServerConfig, secure: bool) -> Result<Box<dyn Transport>> {
    let tcp = dial(&config.host, config.effective_port())?;
    let greet = |e: FtpError| Error::Server(format!("the server did not greet as FTP: {e}"));
    match secure {
        false => {
            let stream = FtpStream::connect_with_stream(tcp).map_err(greet)?;
            Ok(Box::new(Ftp::ready(stream, config)?))
        }
        true => {
            let stream = RustlsFtpStream::connect_with_stream(tcp).map_err(greet)?;
            let verifier = Arc::new(PinVerifier::new(config.pinned.clone())?);
            let tls = ClientConfig::builder_with_provider(verifier.provider.clone())
                .with_safe_default_protocol_versions()
                .map_err(|e| Error::Server(format!("TLS setup failed: {e}")))?
                .dangerous()
                .with_custom_certificate_verifier(verifier.clone())
                .with_no_client_auth();
            let explain = |e: FtpError| {
                verifier.verdict().unwrap_or_else(|| {
                    Error::Server(format!("the TLS connection to the server failed: {e}"))
                })
            };
            let secure = stream
                .into_secure(RustlsConnector::from(Arc::new(tls)), &config.host)
                .map_err(explain)?;
            Ftp::ready(secure, config)
                .map(|f| Box::new(f) as Box<dyn Transport>)
                .map_err(|e| verifier.verdict().unwrap_or(e))
        }
    }
}

/// A TCP connection to the first address of `host` that answers, with stall timeouts.
fn dial(host: &str, port: u16) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Server(format!("cannot find the server {host}: {e}")))?
        .collect();
    let mut last = None;
    for addr in &addrs {
        match tcp(*addr) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(Error::Server(match last {
        Some(e) => format!("cannot connect to {host}:{port}: {e}"),
        None => format!("cannot find the server {host}"),
    }))
}

fn tcp(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    s.set_read_timeout(Some(STALL_TIMEOUT))?;
    s.set_write_timeout(Some(STALL_TIMEOUT))?;
    Ok(s)
}

fn failed(what: &str) -> impl FnOnce(FtpError) -> Error + '_ {
    move |e| Error::Server(format!("{what} failed: {}", tidy(&e)))
}

/// The server's reply (`530 Login incorrect.`) as the user should read it.
fn tidy(e: &FtpError) -> String {
    match e {
        FtpError::UnexpectedResponse(r) => format!(
            "the server answered {}",
            String::from_utf8_lossy(&r.body).trim()
        ),
        other => other.to_string(),
    }
}

fn status(e: &FtpError) -> Option<u32> {
    match e {
        FtpError::UnexpectedResponse(r) => Some(r.status.code()),
        _ => None,
    }
}

impl<T: TlsStream + 'static> Ftp<T> {
    fn ready(stream: ImplFtpStream<T>, config: &ServerConfig) -> Result<Ftp<T>> {
        let mut stream =
            stream.passive_stream_builder(|addr| tcp(addr).map_err(FtpError::ConnectionError));
        stream.set_mode(Mode::Passive);
        stream.set_passive_nat_workaround(true);
        let (user, password) = match &config.auth {
            ServerAuth::Anonymous => ("anonymous", "anonymous@"),
            ServerAuth::Password { user, password } => (user.as_str(), password.as_str()),
            ServerAuth::Key { .. } => {
                return Err(Error::InvalidServer(
                    "key files work with SFTP only".to_owned(),
                ));
            }
        };
        stream.login(user, password).map_err(|e| match status(&e) {
            Some(530) => Error::Server(format!("the server refused the login: {}", tidy(&e))),
            _ => failed("Logging in")(e),
        })?;
        stream
            .transfer_type(FileType::Binary)
            .map_err(failed("Switching to binary mode"))?;
        let mut ftp = Ftp { stream };
        ftp.enter(&config.folder)?;
        Ok(ftp)
    }

    /// Changes into `folder`, creating each missing level.
    fn enter(&mut self, folder: &str) -> Result<()> {
        if folder.starts_with('/') {
            self.stream
                .cwd("/")
                .map_err(failed("Opening the root folder"))?;
        }
        for part in folder.split('/').filter(|p| !p.is_empty()) {
            if self.stream.cwd(part).is_ok() {
                continue;
            }
            self.stream
                .mkdir(part)
                .map_err(failed(&format!("Creating the folder {part}")))?;
            self.stream
                .cwd(part)
                .map_err(failed(&format!("Opening the folder {part}")))?;
        }
        Ok(())
    }
}

impl<T: TlsStream + 'static> Transport for Ftp<T> {
    fn list(&mut self) -> Result<Vec<String>> {
        match self.stream.nlst(None) {
            Ok(names) => Ok(names
                .iter()
                .filter_map(|n| n.trim_end_matches(['\r', '\n']).rsplit('/').next())
                .filter(|n| !n.is_empty() && *n != "." && *n != "..")
                .map(str::to_owned)
                .collect()),
            Err(e) if status(&e).is_some_and(|c| NO_FILES.contains(&c)) => Ok(Vec::new()),
            Err(e) => Err(failed("Listing the backups")(e)),
        }
    }

    fn read(&mut self, name: &str, out: &mut dyn Write) -> Result<()> {
        let mut data = self
            .stream
            .retr_as_stream(name)
            .map_err(failed("Downloading the backup"))?;
        let copied = io::copy(&mut data, out);
        // Dropping the stream on an error closes the data connection and reads the reply.
        copied.map_err(|e| Error::Server(format!("Downloading the backup failed: {e}")))?;
        data.finish().map_err(failed("Downloading the backup"))?;
        out.flush()
            .map_err(|e| Error::Server(format!("Downloading the backup failed: {e}")))
    }

    fn write(&mut self, name: &str, data: &mut dyn Read) -> Result<()> {
        let mut up = self
            .stream
            .put_with_stream(name)
            .map_err(failed("Uploading the backup"))?;
        io::copy(data, &mut up)
            .map_err(|e| Error::Server(format!("Uploading the backup failed: {e}")))?;
        up.finish().map_err(failed("Uploading the backup"))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.stream
            .rename(from, to)
            .map_err(failed("Renaming the uploaded file"))
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        self.stream
            .rm(name)
            .map_err(failed(&format!("Deleting {name}")))
    }
}

impl<T: TlsStream> Drop for Ftp<T> {
    fn drop(&mut self) {
        let _ = self.stream.quit();
    }
}

/// `SHA256:AB:CD:…` of a DER certificate (the form `openssl x509 -fingerprint -sha256` prints).
pub(super) fn cert_fingerprint(der: &[u8]) -> String {
    let hex: Vec<String> = Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    format!("SHA256:{}", hex.join(":"))
}

/// What the certificate check concluded, for a readable error after the handshake failed.
#[derive(Debug)]
enum Verdict {
    Untrusted(String),
    Changed(String),
}

/// WebPKI validation, or exact match against the pinned leaf certificate when one is pinned.
#[derive(Debug)]
struct PinVerifier {
    pinned: Option<String>,
    webpki: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
    algorithms: WebPkiSupportedAlgorithms,
    verdict: Mutex<Option<Verdict>>,
}

impl PinVerifier {
    fn new(pinned: Option<String>) -> Result<PinVerifier> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|e| Error::Server(format!("TLS setup failed: {e}")))?;
        Ok(PinVerifier {
            pinned,
            webpki,
            algorithms: provider.signature_verification_algorithms,
            provider,
            verdict: Mutex::new(None),
        })
    }

    fn note(&self, v: Verdict) {
        *self.verdict.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
    }

    /// The typed trust error, when the certificate check is why the connection failed.
    fn verdict(&self) -> Option<Error> {
        let v = self.verdict.lock().unwrap_or_else(|p| p.into_inner());
        match v.as_ref()? {
            Verdict::Untrusted(fp) => Some(Error::UntrustedServer {
                fingerprint: fp.clone(),
            }),
            Verdict::Changed(fp) => Some(Error::ServerChanged {
                pinned: self.pinned.clone().unwrap_or_default(),
                presented: fp.clone(),
            }),
        }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fp = cert_fingerprint(end_entity);
        match &self.pinned {
            Some(p) if *p == fp => Ok(ServerCertVerified::assertion()),
            Some(_) => {
                self.note(Verdict::Changed(fp));
                Err(rustls::Error::General(
                    "the certificate is not the pinned one".to_owned(),
                ))
            }
            None => self
                .webpki
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .inspect_err(|_| self.note(Verdict::Untrusted(fp))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
