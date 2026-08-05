use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn assert_file_digest(path: &Path, expected: &str) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "digest mismatch for {}: expected {expected}, actual {actual}",
            path.display()
        ))
    }
}

pub fn assert_effect_count(effect: &str, actual: u64, expected: u64) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{effect} durable effect count: expected {expected}, actual {actual}"
        ))
    }
}

pub fn assert_journal_terminal_once(path: &Path) -> Result<(), String> {
    let journal = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read journal {}: {error}", path.display()))?;
    let mut terminal_by_command = BTreeMap::<String, usize>::new();
    for line in journal.lines() {
        let record: serde_json::Value =
            serde_json::from_str(line).map_err(|error| format!("invalid journal JSON: {error}"))?;
        if matches!(record["phase"].as_str(), Some("completed" | "failed")) {
            let command_id = record["commandId"]
                .as_str()
                .ok_or_else(|| "terminal journal record is missing commandId".to_string())?;
            *terminal_by_command.entry(command_id.into()).or_default() += 1;
        }
    }
    if terminal_by_command.is_empty() {
        return Err("journal contains no terminal command records".into());
    }
    if let Some((command_id, count)) = terminal_by_command.iter().find(|(_, count)| **count != 1) {
        return Err(format!(
            "command {command_id} has {count} terminal journal records"
        ));
    }
    Ok(())
}

pub struct EvidenceBundle {
    pub directory: PathBuf,
}

impl EvidenceBundle {
    pub fn create(journey: &str, run_id: &str) -> Result<Self, std::io::Error> {
        let directory = repository_root()
            .join("target/modern-gauntlet-artifacts")
            .join(journey)
            .join(run_id);
        std::fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub fn write_json<T: Serialize>(
        &self,
        name: &str,
        value: &T,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let bytes = serde_json::to_vec_pretty(value)?;
        std::fs::write(self.directory.join(name), bytes)?;
        Ok(())
    }

    pub fn copy_if_present(&self, name: &str, source: &Path) -> Result<(), std::io::Error> {
        if source.is_file() {
            std::fs::copy(source, self.directory.join(name))?;
        }
        Ok(())
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime-tests is nested beneath repository root")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::assert_file_digest;

    #[test]
    fn file_digest_assertion_reports_expected_and_actual_hashes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("report.csv");
        std::fs::write(&path, b"actual report").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"expected report"));

        let error = assert_file_digest(&path, &expected).unwrap_err();

        assert!(error.contains(&expected));
        assert!(error.contains(&format!("{:x}", Sha256::digest(b"actual report"))));
    }
}
