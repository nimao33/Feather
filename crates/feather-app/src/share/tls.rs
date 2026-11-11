//! The servers' TLS certificate: made once on this computer, signed by
//! itself, and kept beside the settings with its key encrypted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::secret;

/// Where the certificate and key are kept.
pub fn folder() -> Option<PathBuf> {
    crate::settings::file_path()?
        .parent()
        .map(|parent| parent.join("sharing"))
}

/// The certificate and key, made on first use.
pub struct Identity {
    pub cert: Vec<u8>,
    key: Vec<u8>,
}

impl Identity {
    pub fn load_or_create(folder: &Path) -> Result<Self, String> {
        if let Some(identity) = Self::load(folder) {
            return Ok(identity);
        }
        let identity = Self::create()?;
        std::fs::create_dir_all(folder)
            .map_err(|error| format!("Could not create {}: {error}", folder.display()))?;
        std::fs::write(folder.join("server.cer"), &identity.cert)
            .map_err(|error| format!("Could not save the certificate: {error}"))?;
        std::fs::write(folder.join("server.key"), secret::protect(&identity.key)?)
            .map_err(|error| format!("Could not save the certificate key: {error}"))?;
        Ok(identity)
    }

    fn load(folder: &Path) -> Option<Self> {
        let cert = std::fs::read(folder.join("server.cer")).ok()?;
        let key = secret::unprotect(&std::fs::read(folder.join("server.key")).ok()?).ok()?;
        (!cert.is_empty() && !key.is_empty()).then_some(Self { cert, key })
    }

    fn create() -> Result<Self, String> {
        let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        if let Some(host) = std::env::var_os("COMPUTERNAME") {
            names.push(host.to_string_lossy().to_ascii_lowercase());
        }
        if let Some(address) = super::lan_address() {
            names.push(address.to_string());
        }
        let mut params = rcgen::CertificateParams::new(names)
            .map_err(|error| format!("Could not make a certificate: {error}"))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Feather on this computer");
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(2045, 1, 1);
        let key = rcgen::KeyPair::generate()
            .map_err(|error| format!("Could not make a certificate key: {error}"))?;
        let cert = params
            .self_signed(&key)
            .map_err(|error| format!("Could not sign the certificate: {error}"))?;
        Ok(Self {
            cert: cert.der().to_vec(),
            key: key.serialize_der(),
        })
    }

    /// The SHA-256 fingerprint browsers show, as `AB:CD:...`.
    pub fn fingerprint(&self) -> String {
        ring::digest::digest(&ring::digest::SHA256, &self.cert)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// A TLS server setup with safe defaults: TLS 1.2 and 1.3 only.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(self.cert.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone())),
            )
            .map_err(|error| format!("The certificate could not be used: {error}"))?;
        // FTP clients resume the control connection's session on data
        // connections; keep enough sessions for that.
        config.session_storage = rustls::server::ServerSessionMemoryCache::new(256);
        Ok(Arc::new(config))
    }
}
