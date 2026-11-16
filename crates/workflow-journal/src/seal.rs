//! Authenticated encryption of journal records at rest.

use std::borrow::Cow;
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const KEY_BYTES: usize = 32;
const DOMAIN: &[u8] = b"bobby-browser/workflow-journal/record/v1";

/// Symmetric key for sealed journal records. Zeroized on drop.
pub struct JournalKey([u8; KEY_BYTES]);

impl std::fmt::Debug for JournalKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JournalKey(..)")
    }
}

impl Drop for JournalKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
struct SealedLine {
    sealed: u8,
    nonce: String,
    ct: String,
}

impl JournalKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    /// Reads the 32-byte key at `path` without creating it.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let mut bytes = std::fs::read(path)?;
        let key = <[u8; KEY_BYTES]>::try_from(bytes.as_slice()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a 32-byte journal key", path.display()),
            )
        });
        bytes.zeroize();
        key.map(Self)
    }

    /// Reads the 32-byte key at `path`, creating it owner-only when missing.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        match Self::load(path) {
            Ok(key) => Ok(key),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut generated = [0u8; KEY_BYTES];
                SystemRandom::new()
                    .fill(&mut generated)
                    .map_err(|_| std::io::Error::other("could not generate a journal key"))?;
                let key = Self(generated);
                generated.zeroize();
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                // A concurrent starter may have created it first; use its key.
                let mut file = match options.open(path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Self::load(path);
                    }
                    Err(error) => return Err(error),
                };
                file.write_all(&key.0)?;
                file.sync_all()?;
                Ok(key)
            }
            Err(error) => Err(error),
        }
    }

    fn cipher(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &self.0).expect("32-byte key"))
    }

    /// Seals one record (its trailing newline is not sealed), returning the
    /// on-disk line including its own newline.
    pub(crate) fn seal_line(&self, record: &[u8]) -> std::io::Result<Vec<u8>> {
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| std::io::Error::other("could not generate a journal nonce"))?;
        let mut ciphertext = record.to_vec();
        self.cipher()
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(DOMAIN),
                &mut ciphertext,
            )
            .map_err(|_| std::io::Error::other("could not seal a journal record"))?;
        let mut line = serde_json::to_vec(&SealedLine {
            sealed: 1,
            nonce: STANDARD.encode(nonce_bytes),
            ct: STANDARD.encode(&ciphertext),
        })?;
        line.push(b'\n');
        Ok(line)
    }

    fn open_line(&self, sealed: &SealedLine) -> Option<Vec<u8>> {
        let nonce: [u8; NONCE_LEN] = STANDARD.decode(&sealed.nonce).ok()?.try_into().ok()?;
        let mut data = STANDARD.decode(&sealed.ct).ok()?;
        let len = self
            .cipher()
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(DOMAIN),
                &mut data,
            )
            .ok()?
            .len();
        data.truncate(len);
        data.push(b'\n');
        Some(data)
    }
}

/// A sealed line yields its decrypted record ending in a newline, or `None`
/// when no key is given or it does not open. Any other line is returned as is.
pub fn plain_line<'a>(line: &'a [u8], key: Option<&JournalKey>) -> Option<Cow<'a, [u8]>> {
    if !line.starts_with(br#"{"sealed":"#) {
        return Some(Cow::Borrowed(line));
    }
    let Ok(sealed) = serde_json::from_slice::<SealedLine>(line) else {
        return Some(Cow::Borrowed(line));
    };
    if sealed.sealed != 1 {
        return Some(Cow::Borrowed(line));
    }
    key?.open_line(&sealed).map(Cow::Owned)
}
