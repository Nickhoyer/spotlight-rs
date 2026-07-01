//! Local encryption for the clipboard history. Everything written to disk (the
//! metadata index and each stored image) is sealed with ChaCha20-Poly1305 under
//! a per-machine random key kept in the secret store (macOS Keychain in release
//! builds). Files on disk are `nonce || ciphertext+tag`; a wrong key or tampered
//! bytes fail to open and are treated as an empty history.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

/// Secret-store key holding the base64 history-encryption key.
const KEY_ID: &str = "clipboard-key";

/// The Poly1305 authentication tag length, in bytes.
const TAG_LEN: usize = 16;

/// A loaded encryption key plus a CSPRNG for nonces.
pub struct Cipher {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Cipher {
    /// Load the stored key, generating and persisting one on first use.
    pub fn load_or_create() -> Self {
        let rng = SystemRandom::new();
        let key_bytes = load_key(&rng);
        let unbound =
            UnboundKey::new(&CHACHA20_POLY1305, &key_bytes).expect("32-byte key is valid");
        Self {
            key: LessSafeKey::new(unbound),
            rng,
        }
    }

    /// Seal `plaintext`, returning `nonce || ciphertext+tag`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| anyhow!("rng failure"))?;
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| anyhow!("seal failure"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&in_out);
        Ok(out)
    }

    /// Open bytes produced by [`Cipher::encrypt`]. Returns `None` on any
    /// authentication/format failure.
    pub fn decrypt(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < NONCE_LEN + TAG_LEN {
            return None;
        }
        let (nonce_bytes, ct) = data.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).ok()?;
        let mut buf = ct.to_vec();
        let plaintext = self.key.open_in_place(nonce, Aad::empty(), &mut buf).ok()?;
        Some(plaintext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cipher() -> Cipher {
        let key = [7u8; 32];
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key).unwrap();
        Cipher {
            key: LessSafeKey::new(unbound),
            rng: SystemRandom::new(),
        }
    }

    #[test]
    fn round_trips() {
        let c = test_cipher();
        let msg = b"copied content that must never touch disk in the clear";
        let sealed = c.encrypt(msg).unwrap();
        assert_ne!(&sealed[12..], &msg[..]); // ciphertext differs from plaintext
        assert_eq!(c.decrypt(&sealed).as_deref(), Some(&msg[..]));
    }

    #[test]
    fn rejects_tampered_or_short() {
        let c = test_cipher();
        let mut sealed = c.encrypt(b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert_eq!(c.decrypt(&sealed), None);
        assert_eq!(c.decrypt(b"short"), None);
    }
}

/// Fetch the stored 32-byte key, or generate + persist a fresh one.
fn load_key(rng: &SystemRandom) -> [u8; 32] {
    if let Some(b64) = spotlight_config::load_secret(KEY_ID) {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return key;
            }
        }
    }
    let mut key = [0u8; 32];
    rng.fill(&mut key).expect("rng failure generating key");
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    let _ = spotlight_config::save_secret(KEY_ID, &encoded);
    key
}
