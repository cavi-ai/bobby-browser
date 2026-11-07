use sha2::{Digest, Sha256};

/// Opaque identity for the requested source string, independent of canonical
/// paths and temporary artifact filenames; it is not a digest of the file.
/// Workers emit it only after admission and file selection; intent
/// verification can match it without exposing paths.
pub fn upload_source_reference(source: &str) -> String {
    format!(
        "upload://source/{}",
        hex::encode(Sha256::digest(source.as_bytes()))
    )
}
