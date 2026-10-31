//! The gateway's VAPID key (RFC 8292): a P-256 key stored as PKCS#8 (`push.key`, owner-only),
//! and the ES256 JWT that identifies the gateway to a push service. The long-lived key is only
//! ever handled by `ring` (constant-time signing).

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use zeroize::Zeroizing;

use crate::gateway::remote::RemoteError;

/// The VAPID key pair.
pub struct VapidKey {
    pair: EcdsaKeyPair,
    rng: SystemRandom,
    /// base64url of the 65-byte uncompressed public point (`applicationServerKey`).
    public: String,
}

impl VapidKey {
    /// A fresh key, as PKCS#8 (what `push.key` holds).
    pub fn generate_pkcs8() -> Result<Zeroizing<Vec<u8>>, RemoteError> {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .map(|doc| Zeroizing::new(doc.as_ref().to_vec()))
            .map_err(|_| RemoteError::Random)
    }

    /// The key in `pkcs8` (`None`: not a P-256 PKCS#8 key).
    pub fn from_pkcs8(pkcs8: &[u8]) -> Option<VapidKey> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng).ok()?;
        let public = URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
        Some(VapidKey { pair, rng, public })
    }

    /// base64url of the uncompressed public point.
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// The `Authorization` value for a push service at `audience` (its origin): `vapid t=<ES256
    /// JWT>, k=<public key>`, valid for `valid` from now. `None` only if signing failed.
    pub fn authorization(&self, audience: &str, subject: &str, valid: Duration) -> Option<String> {
        let exp = SystemTime::now()
            .checked_add(valid)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD
            .encode(serde_json::json!({ "aud": audience, "exp": exp, "sub": subject }).to_string());
        let signed = format!("{header}.{claims}");
        let signature = self.pair.sign(&self.rng, signed.as_bytes()).ok()?;
        let signature = URL_SAFE_NO_PAD.encode(signature.as_ref());
        Some(format!("vapid t={signed}.{signature}, k={}", self.public))
    }
}

/// Loads the key at `path`, creating it on first use. A broken file is reported, never replaced
/// (devices subscribed with its public key).
pub fn load_or_create(path: &Path) -> Result<VapidKey, RemoteError> {
    let io = |source: io::Error| RemoteError::Io {
        path: path.to_path_buf(),
        source,
    };
    let corrupt = |detail: &str| RemoteError::Corrupt {
        path: path.to_path_buf(),
        detail: detail.to_owned(),
    };
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            VapidKey::from_pkcs8(&bytes).ok_or_else(|| corrupt("not a P-256 PKCS#8 key"))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let pkcs8 = VapidKey::generate_pkcs8()?;
            crate::gateway::write_private(path, &pkcs8).map_err(io)?;
            // Re-read: what is on disk is the key from now on.
            let stored = Zeroizing::new(std::fs::read(path).map_err(io)?);
            if stored.as_slice() != pkcs8.as_slice() {
                return Err(corrupt("the new key did not persist"));
            }
            VapidKey::from_pkcs8(&stored).ok_or_else(|| corrupt("not a P-256 PKCS#8 key"))
        }
        Err(e) => Err(io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    #[test]
    fn the_jwt_is_es256_signed_over_audience_expiry_and_subject() {
        let key = VapidKey::from_pkcs8(&VapidKey::generate_pkcs8().unwrap()).unwrap();
        let value = key
            .authorization(
                "https://push.example.com",
                "mailto:a@b.example",
                Duration::from_secs(3600),
            )
            .unwrap();
        let (t, k) = value
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, key.public_key());
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.com");
        assert_eq!(claims["sub"], "mailto:a@b.example");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let exp = claims["exp"].as_u64().unwrap();
        assert!((now + 3590..=now + 3610).contains(&exp), "{exp} vs {now}");
        let public = URL_SAFE_NO_PAD.decode(k).unwrap();
        assert_eq!((public.len(), public[0]), (65, 4));
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(signature.len(), 64, "JWS ES256 is r || s");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn created_once_owner_only_and_a_broken_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets").join("push.key");
        let first = load_or_create(&path).unwrap();
        let again = load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), again.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(
            load_or_create(&path),
            Err(RemoteError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
    }
}
