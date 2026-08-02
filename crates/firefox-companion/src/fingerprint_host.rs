//! Firefox BiDi adapter for [`fingerprinting::FingerprintHost`].

use async_trait::async_trait;
use fingerprinting::{FingerprintApplyError, FingerprintApplyPlan, FingerprintHost};
use serde_json::{json, Value};

use crate::bidi::BidiTransport;

/// Applies a fingerprint plan through WebDriver BiDi preload scripts and
/// UA / locale / timezone / viewport overrides.
pub struct FirefoxBidiHost<'a> {
    pub transport: &'a dyn BidiTransport,
    /// Optional browsing context for per-context overrides.
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

    pub async fn remove_preload_script(
        &self,
        script_id: &str,
    ) -> Result<(), FingerprintApplyError> {
        self.transport
            .send("script.removePreloadScript", json!({ "script": script_id }))
            .await
            .map_err(|error| FingerprintApplyError::Host(format!("{error:?}")))?;
        Ok(())
    }

    fn context_params(&self) -> Value {
        match self.context {
            Some(context) => json!({ "contexts": [context] }),
            None => json!({}),
        }
    }

    /// UA / locale / timezone / viewport. Best-effort: older Firefox builds may
    /// lack individual emulation commands (min gecko is still below all of them).
    pub async fn apply_emulation_overrides(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        let mut ua_params = self.context_params();
        if let Some(obj) = ua_params.as_object_mut() {
            obj.insert("userAgent".into(), json!(plan.user_agent));
        }
        let _ = self
            .transport
            .send("emulation.setUserAgentOverride", ua_params)
            .await;

        let mut locale_params = self.context_params();
        if let Some(obj) = locale_params.as_object_mut() {
            obj.insert("locale".into(), json!(plan.locale));
        }
        let _ = self
            .transport
            .send("emulation.setLocaleOverride", locale_params)
            .await;

        let mut tz_params = self.context_params();
        if let Some(obj) = tz_params.as_object_mut() {
            obj.insert("timezone".into(), json!(plan.timezone_id));
        }
        let _ = self
            .transport
            .send("emulation.setTimezoneOverride", tz_params)
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

#[async_trait]
impl FingerprintHost for FirefoxBidiHost<'_> {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        let _script_id = self.add_preload_script(plan).await?;
        self.apply_emulation_overrides(plan).await?;
        Ok(())
    }
}
