//! Persisted browser selection and the optional per-session engine, resolved
//! into the preference a launch pins and the selection that composes the factory.

use companion_protocol::BrowserEngine;
use config::{BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig};
use types::{ProfileId, SkillBrowserEngine};

use crate::selection::EnginePreference;

/// What a launch uses: the preference pinned for the session (or the factory
/// default when the session names no engine) and the selection the worker
/// factory is composed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenSessionEngine {
    pub preference: EnginePreference,
    pub selection: BrowserSelectionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEngineError {
    message: String,
}

impl std::fmt::Display for SessionEngineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionEngineError {}

pub fn choose_session_engine(
    persisted: BrowserSelectionConfig,
    session_engine: Option<SkillBrowserEngine>,
) -> Result<ChosenSessionEngine, SessionEngineError> {
    let selection = startable(persisted);
    let preference = match session_engine {
        Some(engine) => EnginePreference::for_engine(engine),
        None => map_preference(selection.preference.clone())?,
    };
    Ok(ChosenSessionEngine {
        preference,
        selection,
    })
}

/// A Firefox preference with no enrolled profile has nothing to launch;
/// managed Chromium needs no enrollment, so the runtime starts on it until a
/// profile is paired.
fn startable(mut selection: BrowserSelectionConfig) -> BrowserSelectionConfig {
    if selection.firefox.is_empty()
        && matches!(
            selection.preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                ..
            }
        )
    {
        selection.preference = EnginePreferenceConfig::ManagedChromium;
    }
    selection
}

fn map_preference(config: EnginePreferenceConfig) -> Result<EnginePreference, SessionEngineError> {
    match config {
        EnginePreferenceConfig::ManagedChromium => Ok(EnginePreference::ManagedChromium),
        EnginePreferenceConfig::Exact { engine, profile_id } => {
            let engine = browser_engine(engine);
            let profile_id = match engine {
                // A Chromium exact selection's profile id already picked the
                // durable-vs-disposable user-data-dir when the worker factory
                // was composed. There is only ever one Chromium registration,
                // so routing needs no profile_id to match against, and the
                // Firefox-only ProfileId (a UUID) cannot carry an arbitrary
                // Chromium profile name.
                BrowserEngine::Chromium => None,
                _ => profile_id
                    .map(|value| {
                        uuid::Uuid::parse_str(&value)
                            .map(ProfileId)
                            .map_err(|error| SessionEngineError {
                                message: error.to_string(),
                            })
                    })
                    .transpose()?,
            };
            Ok(EnginePreference::Exact { engine, profile_id })
        }
        EnginePreferenceConfig::Prefer { engines } => Ok(EnginePreference::Prefer {
            engines: engines.into_iter().map(browser_engine).collect(),
        }),
    }
}

fn browser_engine(value: BrowserEngineConfig) -> BrowserEngine {
    match value {
        BrowserEngineConfig::Firefox => BrowserEngine::Firefox,
        BrowserEngineConfig::Chromium => BrowserEngine::Chromium,
        BrowserEngineConfig::WebKit => BrowserEngine::WebKit,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use companion_protocol::BrowserEngine;
    use config::FirefoxCompanionConfig;
    use types::{CommandError, ProfileId, SessionId};

    use super::*;
    use crate::selection::{BrowserWorkerSelector, FactoryRegistration, RequiredCapabilities};
    use crate::{BrowserWorker, WorkerFactory};

    struct IdleFactory;

    #[async_trait]
    impl WorkerFactory for IdleFactory {
        async fn launch(
            &self,
            _session_id: &SessionId,
        ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
            Err(CommandError {
                code: types::ErrorCode::BrowserLaunchFailed,
                message: "idle factory does not launch".into(),
                layer: types::ErrorLayer::Driver,
                retryable: false,
            })
        }
    }

    fn enrolled_firefox(profile_id: &ProfileId) -> FirefoxCompanionConfig {
        FirefoxCompanionConfig {
            profile_id: profile_id.0.to_string(),
            bidi_url: "ws://127.0.0.1:9222/session".into(),
            profile_dir: PathBuf::from("firefox-profile"),
            companion_bind: "127.0.0.1:0".into(),
            descriptor_path: PathBuf::from("descriptor.json"),
            attachment_only: false,
            timeout_ms: 1,
            pairing_code_ttl_ms: 1,
            attachment_ttl_ms: 1,
        }
    }

    #[test]
    fn empty_firefox_selection_becomes_managed_chromium() {
        let persisted = BrowserSelectionConfig {
            preference: EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: Some("not-a-firefox-uuid".into()),
            },
            firefox: Vec::new(),
            chromium: Vec::new(),
        };

        let chosen = choose_session_engine(persisted, None).unwrap();

        assert_eq!(
            chosen.selection.preference,
            EnginePreferenceConfig::ManagedChromium
        );
        assert!(chosen.selection.firefox.is_empty());
        assert_eq!(chosen.preference, EnginePreference::ManagedChromium);
    }

    #[test]
    fn chromium_exact_selection_drops_profile_id_for_the_single_registration() {
        let persisted = BrowserSelectionConfig {
            preference: EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Chromium,
                profile_id: Some("signed-in-chrome".into()),
            },
            firefox: Vec::new(),
            chromium: Vec::new(),
        };

        let chosen = choose_session_engine(persisted, None).unwrap();

        assert_eq!(
            chosen.preference,
            EnginePreference::Exact {
                engine: BrowserEngine::Chromium,
                profile_id: None,
            }
        );
        assert_eq!(
            chosen.selection.preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Chromium,
                profile_id: Some("signed-in-chrome".into()),
            }
        );

        let selector = BrowserWorkerSelector::new(
            vec![FactoryRegistration::negotiated(
                BrowserEngine::Chromium,
                None,
                Arc::new(IdleFactory),
            )],
            RequiredCapabilities::default(),
        );
        assert!(
            selector.can_select(&chosen.preference),
            "the launch has one Chromium registration and ProfileId is a Firefox UUID, so Exact matches that registration with profile_id None"
        );
        assert!(
            !selector.can_select(&EnginePreference::Exact {
                engine: BrowserEngine::Chromium,
                profile_id: Some(ProfileId::new()),
            }),
            "find() matches a Firefox profile id; a Chromium profile id would miss the single registration"
        );
    }

    #[test]
    fn session_engine_selection_pins_the_requested_engine_without_the_persisted_profile() {
        let profile_id = ProfileId::new();
        let persisted = BrowserSelectionConfig {
            preference: EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: Some(profile_id.0.to_string()),
            },
            firefox: vec![enrolled_firefox(&profile_id)],
            chromium: Vec::new(),
        };

        let chosen =
            choose_session_engine(persisted.clone(), Some(SkillBrowserEngine::Firefox)).unwrap();

        assert_eq!(
            chosen.preference,
            EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            }
        );
        assert_eq!(chosen.selection, persisted);
    }

    #[test]
    fn enrolled_firefox_selection_keeps_its_exact_profile() {
        let profile_id = ProfileId::new();
        let persisted = BrowserSelectionConfig {
            preference: EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: Some(profile_id.0.to_string()),
            },
            firefox: vec![enrolled_firefox(&profile_id)],
            chromium: Vec::new(),
        };

        let chosen = choose_session_engine(persisted.clone(), None).unwrap();

        assert_eq!(chosen.selection, persisted);
        assert_eq!(
            chosen.preference,
            EnginePreference::Exact {
                engine: BrowserEngine::Firefox,
                profile_id: Some(profile_id),
            }
        );
    }
}
