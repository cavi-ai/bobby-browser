use sha2::{Digest, Sha256};

/// Opaque identity for the requested source, independent of canonical paths
/// and temporary artifact filenames. Workers emit it only after admission and
/// file selection; intent verification can match it without exposing paths.
pub fn upload_source_reference(source: &str) -> String {
    format!(
        "upload://sha256/{}",
        hex::encode(Sha256::digest(source.as_bytes()))
    )
}
