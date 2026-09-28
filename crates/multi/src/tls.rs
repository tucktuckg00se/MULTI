//! HTTPS for `multi serve`: the user's certificate (`web.tls_cert` /
//! `web.tls_key`) or a self-signed one made on first use and kept next to the
//! config file. rustls with the ring provider; works offline.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use multi_core::config::Web;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// File names of the generated certificate, next to the config file.
pub const SELF_CERT: &str = "multi-web-cert.pem";
pub const SELF_KEY: &str = "multi-web-key.pem";

pub struct Tls {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the leaf certificate, `AA:BB:…`.
    pub fingerprint: String,
    pub cert_path: PathBuf,
    /// Made just now.
    pub generated: bool,
}

/// Certificate and key for `web`, relative paths resolved against the config
/// file's directory.
pub fn load(web: &Web, config_path: &Path) -> Result<Tls> {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (cert, key, generated) = match (&web.tls_cert, &web.tls_key) {
        (Some(c), Some(k)) => (dir.join(c), dir.join(k), false),
        _ => {
            let (c, k) = (dir.join(SELF_CERT), dir.join(SELF_KEY));
            let generated = !(c.exists() && k.exists());
            if generated {
                generate(web.bind, &c, &k)?;
            }
            (c, k, generated)
        }
    };
    let certs = CertificateDer::pem_file_iter(&cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| anyhow!("cannot read certificate {}: {e}", cert.display()))?;
    let leaf = certs
        .first()
        .ok_or_else(|| anyhow!("no certificate in {}", cert.display()))?;
    let fingerprint = fingerprint(leaf);
    let key_der = PrivateKeyDer::from_pem_file(&key)
        .map_err(|e| anyhow!("cannot read private key {}: {e}", key.display()))?;
    Ok(Tls {
        config: server_config(certs, key_der)?,
        fingerprint,
        cert_path: cert,
        generated,
    })
}

pub fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("certificate and key do not form a usable pair")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Names the self-signed certificate covers.
fn names(bind: IpAddr) -> Vec<String> {
    let mut v = vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()];
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim();
        if !h.is_empty() && h != "localhost" {
            v.push(h.to_string());
        }
    }
    if !bind.is_unspecified() && !bind.is_loopback() {
        v.push(bind.to_string());
    }
    v
}

/// A self-signed certificate; the key is written with mode 0600.
pub fn generate(bind: IpAddr, cert: &Path, key: &Path) -> Result<()> {
    let made = rcgen::generate_simple_self_signed(names(bind))
        .context("cannot make a self-signed certificate")?;
    crate::web::write_atomic(key, &made.signing_key.serialize_pem())
        .with_context(|| format!("cannot write {}", key.display()))?;
    crate::web::write_atomic(cert, &made.cert.pem())
        .with_context(|| format!("cannot write {}", cert.display()))?;
    Ok(())
}
