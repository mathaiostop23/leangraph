//! Secrets at rest.
//!
//! The keys this holds — an Anthropic key, a GitHub token — are the two things
//! worth stealing from this database. Storing them in plaintext means a stray
//! backup, a mounted volume, or a `SELECT *` in a support session hands them
//! over.
//!
//! AES-256-GCM with a random nonce per write. Not a key-management system: the
//! master key lives in the environment, or failing that in a file beside the
//! database — which is weaker, because an attacker who can read the database
//! can usually read the file next to it. That trade-off is stated at startup
//! rather than hidden, because a user who thinks their key is protected when it
//! is only obscured is worse off than one who knows.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub struct Vault {
    cipher: Aes256Gcm,
    /// True when the key sits beside the database rather than coming from the
    /// environment. Surfaced so the operator can see which they have.
    pub key_on_disk: bool,
}

fn decode_key(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    let bytes = if s.len() == 64 {
        hex::decode(s).context("LEANGRAPH_MASTER_KEY is not valid hex")?
    } else {
        bail!("LEANGRAPH_MASTER_KEY must be 64 hex characters (32 bytes)");
    };
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    Ok(k)
}

impl Vault {
    /// Environment first, then a file beside the database, generating one if
    /// neither exists.
    pub fn open(data_dir: &Path) -> Result<Vault> {
        if let Ok(env) = std::env::var("LEANGRAPH_MASTER_KEY") {
            if !env.trim().is_empty() {
                return Ok(Vault {
                    cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&decode_key(&env)?)),
                    key_on_disk: false,
                });
            }
        }

        let path = data_dir.join("master.key");
        let key = if path.exists() {
            decode_key(&std::fs::read_to_string(&path).context("reading master.key")?)?
        } else {
            let mut k = [0u8; 32];
            OsRng.fill_bytes(&mut k);
            std::fs::create_dir_all(data_dir).ok();
            std::fs::write(&path, hex::encode(k)).context("writing master.key")?;
            restrict(&path);
            k
        };
        restrict(&path);
        Ok(Vault {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
            key_on_disk: true,
        })
    }

    /// Returns `(nonce, ciphertext)`. A fresh random nonce every time: reusing
    /// one under the same key breaks GCM outright, and 96 random bits is safe
    /// well past the number of secrets this will ever hold.
    pub fn seal(&self, plaintext: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        Ok((nonce.to_vec(), ct))
    }

    pub fn open_sealed(&self, nonce: &[u8], ciphertext: &[u8]) -> Result<String> {
        if nonce.len() != 12 {
            bail!("stored secret has a malformed nonce");
        }
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            // GCM authenticates: this fails if the ciphertext was altered *or*
            // if the master key changed. Both mean the same thing to a caller —
            // the secret is unreadable and must be set again.
            .map_err(|_| anyhow::anyhow!("secret could not be decrypted; was the master key changed?"))?;
        String::from_utf8(pt).context("decrypted secret is not valid UTF-8")
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

/// What to show instead of the secret: enough to recognise which key is
/// configured, not enough to be one.
pub fn hint(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = secret.chars().take(6).collect();
    let tail: String = secret.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}
