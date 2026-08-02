//! Firefox BiDi adapter for [`fingerprinting::FingerprintHost`].

use async_trait::async_trait;
use fingerprinting::{FingerprintApplyError, FingerprintApplyPlan, FingerprintHost};
use serde_json::{json, Value};

use crate::bidi::BidiTransport;

/// Applies a fingerprint plan through WebDriver BiDi preload scripts and
/// optional UA / viewport overrides. Works for any context created after
/// the preload script is registered.
pub struct FirefoxBidiHost<'a> {
    pub transport: &'a dyn BidiTransport,
    /// Optional browsing context for viewport override.
    pub context: Option<&'a str>,
}

impl FirefoxBidiHost<'_> {
    pub async fn add_preload_script(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<String, FingerprintApplyError> {
        let function_declaration = format!("() => {{ {}; }}", plan.init_script);
        let response = self
            .transport
            .send(
                "script.addPreloadScript",
                json!({ "functionDeclaration": function_declaration }),
            )
            .await
            .map_err(|error| FingerprintApplyError::Host(format!("{error:?}")))?;
        response
            .get("script")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                FingerprintApplyError::Host(
                    "Firefox BiDi did not return a preload script id".into(),
                )
            })
    }

    pub async fn remove_preload_script(&self, script_id: &str) -> Result<(), FingerprintApplyError> {
        self.transport
            .send("script.removePreloadScript", json!({ "script": script_id }))
            .await
            .map_err(|error| FingerprintApplyError::Host(format!("{error:?}")))?;
        Ok(())
    }
}

#[async_trait]
impl FingerprintHost for FirefoxBidiHost<'_> {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        let _script_id = self.add_preload_script(plan).await?;

        // Best-effort: newer Firefox builds expose these emulation commands.
        let _ = self
            .transport
            .send(
                "emulation.setUserAgentOverride",
                json!({ "userAgent": plan.user_agent }),
            )
            .await;

        if let Some(context) = self.context {
            let _ = self
                .transport
                .send(
                    "browsingContext.setViewport",
                    json!({
                        "context": context,
                        "viewport": {
                            "width": plan.device_metrics.width,
                            "height": plan.device_metrics.height,
                        },
                        "devicePixelRatio": plan.device_metrics.device_scale_factor,
                    }),
                )
                .await;
        }

        Ok(())
    }
}
