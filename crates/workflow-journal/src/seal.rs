//! Authenticated encryption of journal records at rest.

use std::borrow::Cow;
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

const KEY_BYTES: usize = 32;
const DOMAIN: &[u8] = b"bobby-browser/workflow-journal/record/v2";
/// SHA-256 of the empty input: the previous digest of a file's first line.
pub(crate) const GENESIS: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

const INDEX_DOMAIN: &[u8] = b"bobby-browser/workflow-journal/segment-index/v1";

/// Binds a sealed line to the exact bytes of the line before it.
fn associated_data(previous: &[u8; 32]) -> Vec<u8> {
    let mut data = DOMAIN.to_vec();
    data.extend_from_slice(previous);
    data
}

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

    /// Seals one record (its trailing newline is not sealed) after the line
    /// whose digest is `previous`, returning the on-disk line with its newline.
    pub(crate) fn seal_line(&self, record: &[u8], previous: &[u8; 32]) -> std::io::Result<Vec<u8>> {
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        self.seal_with(record, associated_data(previous))
    }

    /// Seals a segment index; its `previousTail` fixes its place in the chain.
    pub(crate) fn seal_index(&self, index: &[u8]) -> std::io::Result<Vec<u8>> {
        self.seal_with(index, INDEX_DOMAIN.to_vec())
    }

    /// Opens an index sealed by [`Self::seal_index`].
    pub(crate) fn open_index(&self, line: &[u8]) -> Option<Vec<u8>> {
        let sealed = parse_sealed(line)?;
        let mut plain = self.open_with(&sealed, INDEX_DOMAIN.to_vec())?;
        plain.pop();
        Some(plain)
    }

    fn seal_with(&self, record: &[u8], aad: Vec<u8>) -> std::io::Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| std::io::Error::other("could not generate a journal nonce"))?;
        let mut ciphertext = record.to_vec();
        self.cipher()
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad),
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

    fn open_line(&self, sealed: &SealedLine, previous: &[u8; 32]) -> Option<Vec<u8>> {
        self.open_with(sealed, associated_data(previous))
    }

    /// Returns the plaintext followed by a newline.
    fn open_with(&self, sealed: &SealedLine, aad: Vec<u8>) -> Option<Vec<u8>> {
        let nonce: [u8; NONCE_LEN] = STANDARD.decode(&sealed.nonce).ok()?.try_into().ok()?;
        let mut data = STANDARD.decode(&sealed.ct).ok()?;
        let len = self
            .cipher()
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut data,
            )
            .ok()?
            .len();
        data.truncate(len);
        data.push(b'\n');
        Some(data)
    }
}

fn parse_sealed(line: &[u8]) -> Option<SealedLine> {
    if !line.starts_with(br#"{"sealed":"#) {
        return None;
    }
    serde_json::from_slice::<SealedLine>(line)
        .ok()
        .filter(|sealed| sealed.sealed == 1)
}

pub(crate) fn digest(line: &[u8]) -> [u8; 32] {
    Sha256::digest(line).into()
}

/// Opens one line known to follow the line with digest `previous`. A sealed
/// line needs the key; a plain line is returned only when `plain_allowed`.
pub(crate) fn open_at<'a>(
    line: &'a [u8],
    key: Option<&JournalKey>,
    previous: &[u8; 32],
    plain_allowed: bool,
) -> Option<Cow<'a, [u8]>> {
    match parse_sealed(line) {
        Some(sealed) => key?.open_line(&sealed, previous).map(Cow::Owned),
        None => plain_allowed.then_some(Cow::Borrowed(line)),
    }
}

/// Opens the lines of one journal file in order. Each sealed line must follow
/// the exact bytes of the line before it, and plain lines are accepted only
/// before the first sealed line.
pub struct LineOpener<'k> {
    key: Option<&'k JournalKey>,
    previous: [u8; 32],
    sealed_seen: bool,
}

impl<'k> LineOpener<'k> {
    pub fn new(key: Option<&'k JournalKey>) -> Self {
        Self {
            key,
            previous: GENESIS,
            sealed_seen: false,
        }
    }

    /// Continues a chain whose last line had digest `previous`.
    pub(crate) fn resume(
        key: Option<&'k JournalKey>,
        previous: [u8; 32],
        sealed_seen: bool,
    ) -> Self {
        Self {
            key,
            previous,
            sealed_seen,
        }
    }

    /// Feed every complete line of the file, in order, whether or not it opens.
    /// Returns the record bytes ending in a newline, or `None` when the line is
    /// not trusted at this position.
    pub fn open<'a>(&mut self, line: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let opened = open_at(line, self.key, &self.previous, !self.sealed_seen);
        self.sealed_seen |= parse_sealed(line).is_some();
        self.previous = digest(line);
        opened
    }

    /// Digest of the last line fed, or the genesis digest before any.
    pub(crate) fn previous(&self) -> [u8; 32] {
        self.previous
    }

    pub(crate) fn sealed_seen(&self) -> bool {
        self.sealed_seen
    }
}
