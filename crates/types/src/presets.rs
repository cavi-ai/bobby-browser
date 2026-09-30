//! Named capability sets a principal is minted with.
//!
//! `bobby init --preset` and the generated capability matrix in the docs read
//! this one table, so what a host's credential can do and what the docs say it
//! can do cannot drift apart.

use crate::Capability;

/// A named capability floor for one kind of principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityPreset {
    /// Local operator: every capability, including `authority:admin`.
    Unrestricted,
    /// Every capability except `authority:admin`.
    Agent,
    /// Claude Code and other interactive agent hosts.
    Claude,
    /// Codex CLI.
    Codex,
    /// A sandboxed NVIDIA OpenShell tenant.
    Openshell,
}

const UNRESTRICTED: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::NetworkEgress,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::JobSubmit,
    Capability::JobRead,
    Capability::JobCancel,
    Capability::AuthorityAdmin,
    Capability::BrowserFingerprint,
    Capability::BrowserHumanize,
];

const AGENT: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::NetworkEgress,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::JobSubmit,
    Capability::JobRead,
    Capability::JobCancel,
    Capability::BrowserFingerprint,
    Capability::BrowserHumanize,
];

/// What the shipped bobby agent skill uses. JavaScript evaluation and the
/// fingerprint/humanize pair are not part of that workflow, so a host
/// credential leaves them out until the operator mints `agent`.
const HOST: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::NetworkEgress,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::JobSubmit,
    Capability::JobRead,
    Capability::JobCancel,
];

const OPENSHELL: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::IntentExecute,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
];

impl CapabilityPreset {
    pub const ALL: [Self; 5] = [
        Self::Unrestricted,
        Self::Agent,
        Self::Claude,
        Self::Codex,
        Self::Openshell,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Agent => "agent",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Openshell => "openshell",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.as_str() == raw.trim())
    }

    pub const fn capabilities(self) -> &'static [Capability] {
        match self {
            Self::Unrestricted => UNRESTRICTED,
            Self::Agent => AGENT,
            Self::Claude | Self::Codex => HOST,
            Self::Openshell => OPENSHELL,
        }
    }

    pub fn contains(self, capability: Capability) -> bool {
        self.capabilities().contains(&capability)
    }

    /// One line for `bobby init` and the docs matrix.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Unrestricted => {
                "local operator: every capability, including authority:admin"
            }
            Self::Agent => "no authority:admin; every other capability",
            Self::Claude | Self::Codex => {
                "the shipped agent skill's workflow: no JavaScript evaluation, fingerprint, humanize, or authority:admin"
            }
            Self::Openshell => {
                "sandboxed tenant: browse, intents, files, evidence, and recovery only"
            }
        }
    }
}

/// What holding `capability` lets a principal do, for the docs matrix.
pub const fn capability_effect(capability: Capability) -> &'static str {
    match capability {
        Capability::SessionRead => "list sessions, read runtime info, subscribe to events",
        Capability::SessionWrite => "create and delete sessions",
        Capability::PageRead => "read page state",
        Capability::PageWrite => "open and close pages",
        Capability::BrowserMutate => "submit commands: navigate, click, type",
        Capability::NetworkEgress => {
            "run the HTTP job handlers (http_probe, http_wait, http_fetch)"
        }
        Capability::FileUpload => "upload local files into the page",
        Capability::FileDownload => "download files to disk",
        Capability::JavascriptEvaluate => "run JavaScript in the page",
        Capability::IntentExecute => "run intent commands (locate, fill, submit, follow)",
        Capability::VisionAssist => "escalate stuck intents and extraction to a vision model",
        Capability::ContextRead => "read remembered site structure",
        Capability::ArtifactRead => "read stored artifacts",
        Capability::ArtifactCapture => "capture screenshots and other artifacts",
        Capability::RecoveryRead => "read checkpoints and recovery state",
        Capability::RecoveryWrite => "save checkpoints and recover workflows",
        Capability::JobSubmit => "submit background jobs",
        Capability::JobRead => "read background jobs",
        Capability::JobCancel => "cancel background jobs",
        Capability::AuthorityAdmin => "mint and revoke principals",
        Capability::BrowserFingerprint => "spoof the browser fingerprint",
        Capability::BrowserHumanize => "humanize input timing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subset(narrow: CapabilityPreset, wide: CapabilityPreset) -> bool {
        narrow
            .capabilities()
            .iter()
            .all(|capability| wide.contains(*capability))
    }

    #[test]
    fn presets_narrow_from_unrestricted_to_openshell() {
        assert_eq!(
            CapabilityPreset::Unrestricted.capabilities().len(),
            Capability::ALL.len()
        );
        for capability in Capability::ALL {
            assert!(CapabilityPreset::Unrestricted.contains(capability));
        }
        assert!(subset(
            CapabilityPreset::Agent,
            CapabilityPreset::Unrestricted
        ));
        assert!(subset(CapabilityPreset::Claude, CapabilityPreset::Agent));
        assert!(subset(CapabilityPreset::Codex, CapabilityPreset::Agent));
        assert!(subset(
            CapabilityPreset::Openshell,
            CapabilityPreset::Claude
        ));
        for preset in CapabilityPreset::ALL {
            assert_eq!(
                preset.contains(Capability::AuthorityAdmin),
                preset == CapabilityPreset::Unrestricted,
                "{}",
                preset.as_str()
            );
        }
        for capability in [
            Capability::JavascriptEvaluate,
            Capability::BrowserFingerprint,
            Capability::BrowserHumanize,
        ] {
            assert!(!CapabilityPreset::Claude.contains(capability));
            assert!(!CapabilityPreset::Codex.contains(capability));
        }
    }

    #[test]
    fn presets_round_trip_their_names_and_list_no_duplicates() {
        for preset in CapabilityPreset::ALL {
            assert_eq!(CapabilityPreset::parse(preset.as_str()), Some(preset));
            let capabilities = preset.capabilities();
            for (index, capability) in capabilities.iter().enumerate() {
                assert!(
                    !capabilities[index + 1..].contains(capability),
                    "{} lists {} twice",
                    preset.as_str(),
                    capability.as_str()
                );
            }
        }
        assert_eq!(CapabilityPreset::parse("root"), None);
    }
}
