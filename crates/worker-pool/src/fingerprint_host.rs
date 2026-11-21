//! Chromium CDP adapter for [`fingerprinting::FingerprintHost`].

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetLocaleOverrideParams,
    SetTimezoneOverrideParams, SetTouchEmulationEnabledParams, UserAgentBrandVersion,
    UserAgentMetadata,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::types::MethodId;
use chromiumoxide::{error::CdpError, Command, Method, Page};
use fingerprinting::{FingerprintApplyError, FingerprintApplyPlan, FingerprintHost};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct ChromiumPageHost<'a> {
    pub page: &'a Page,
    pub shared_locale: &'a AtomicBool,
}

/// CDP setUserAgentOverride with a loose metadata object so we can include
/// deprecated `fullVersion` (required for `navigator.userAgentData` uaFullVersion).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserAgentOverrideCmd {
    user_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    accept_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_agent_metadata: Option<Value>,
    #[serde(skip_serializing)]
    method: &'static str,
}

impl Method for UserAgentOverrideCmd {
    fn identifier(&self) -> MethodId {
        self.method.into()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UserAgentOverrideReturns {}

impl Command for UserAgentOverrideCmd {
    type Response = UserAgentOverrideReturns;
}

impl UserAgentOverrideCmd {
    fn for_domain(method: &'static str, plan: &FingerprintApplyPlan, metadata: Value) -> Self {
        Self {
            user_agent: plan.user_agent.clone(),
            accept_language: Some(plan.locale.clone()),
            platform: Some(plan.session.platform.clone()),
            user_agent_metadata: Some(metadata),
            method,
        }
    }
}

fn user_agent_metadata_value(plan: &FingerprintApplyPlan) -> Result<Value, FingerprintApplyError> {
    let hints = &plan.session.client_hints;
    let brands = hints
        .brands
        .iter()
        .map(|b| UserAgentBrandVersion::new(&b.brand, &b.version))
        .collect::<Vec<_>>();
    let full_version_list = hints
        .full_version_list
        .iter()
        .map(|b| UserAgentBrandVersion::new(&b.brand, &b.version))
        .collect::<Vec<_>>();

    let meta = UserAgentMetadata {
        brands: Some(brands),
        full_version_list: Some(full_version_list),
        platform: hints.platform.clone(),
        platform_version: hints.platform_version.clone(),
        architecture: hints.architecture.clone(),
        model: hints.model.clone(),
        mobile: hints.mobile,
        bitness: Some(hints.bitness.clone()),
        wow64: Some(false),
        form_factors: Some(vec!["Desktop".to_string()]),
    };
    let mut value = serde_json::to_value(meta).map_err(|error| {
        FingerprintApplyError::Host(format!("userAgentMetadata serialize failed: {error}"))
    })?;
    if let Some(obj) = value.as_object_mut() {
        // Deprecated CDP field — still what Chromium uses for uaFullVersion.
        obj.insert("fullVersion".into(), json!(hints.full_version));
    }
    Ok(value)
}

#[async_trait]
impl FingerprintHost for ChromiumPageHost<'_> {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        let metadata = user_agent_metadata_value(plan)?;
        let emulation_ua = UserAgentOverrideCmd::for_domain(
            "Emulation.setUserAgentOverride",
            plan,
            metadata.clone(),
        );
        let network_ua =
            UserAgentOverrideCmd::for_domain("Network.setUserAgentOverride", plan, metadata);
        let locale = SetLocaleOverrideParams {
            locale: Some(plan.locale.clone()),
        };
        let timezone = SetTimezoneOverrideParams {
            timezone_id: plan.timezone_id.clone(),
        };
        let metrics = SetDeviceMetricsOverrideParams::builder()
            .width(plan.device_metrics.width as i64)
            .height(plan.device_metrics.height as i64)
            .device_scale_factor(plan.device_metrics.device_scale_factor)
            .mobile(plan.device_metrics.mobile)
            .build()
            .map_err(FingerprintApplyError::Host)?;
        let touch = (plan.session.max_touch_points == 0)
            .then(|| SetTouchEmulationEnabledParams::new(false));

        // Independent overrides can run concurrently; init script stays last so
        // document-start injection sees the final UA/metrics environment.
        let (r_emu, r_net, r_locale, r_tz, r_metrics, r_media) = tokio::join!(
            self.page.execute(emulation_ua),
            self.page.execute(network_ua),
            self.page.emulate_locale(locale),
            self.page.emulate_timezone(timezone),
            self.page.execute(metrics),
            self.page
                .emulate_media_features(vec![MediaFeature::new("prefers-color-scheme", "dark",)]),
        );
        r_emu.map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        r_net.map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        if let Err(error) = r_locale {
            if !is_existing_locale_override(&error)
                || !effective_locale_matches(self.page, &plan.locale)
                    .await
                    .unwrap_or(false)
            {
                return Err(FingerprintApplyError::Host(error.to_string()));
            }
            self.shared_locale.store(true, Ordering::Relaxed);
        }
        r_tz.map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        r_metrics.map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        r_media.map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        if let Some(touch) = touch {
            self.page
                .execute(touch)
                .await
                .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        }

        self.page
            .add_init_script(plan.init_script.as_str())
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        Ok(())
    }
}

fn is_existing_locale_override(error: &CdpError) -> bool {
    matches!(error, CdpError::Chrome(details)
        if details.code == -32000 && details.message == "Another locale override is already in effect")
}

/// Check the browser's Intl locale in Chromium's isolated world.
async fn effective_locale_matches(page: &Page, locale: &str) -> Result<bool, CdpError> {
    let context = page
        .secondary_execution_context()
        .await?
        .ok_or_else(|| CdpError::msg("isolated locale context is unavailable"))?;
    let requested = serde_json::to_string(&locale.replace('_', "-"))?;
    let expression = format!(
        "Intl.getCanonicalLocales({requested})[0] === Intl.NumberFormat().resolvedOptions().locale"
    );
    let params = EvaluateParams::builder()
        .expression(expression)
        .context_id(context)
        .return_by_value(true)
        .build()
        .map_err(CdpError::msg)?;
    Ok(page.evaluate(params).await?.into_value::<bool>()?)
}

/// Reacquire a previously shared override when its original owner has closed.
pub(crate) async fn ensure_shared_locale(page: &Page, locale: &str) -> Result<(), CdpError> {
    if effective_locale_matches(page, locale).await? {
        return Ok(());
    }
    if let Err(error) = page
        .emulate_locale(SetLocaleOverrideParams {
            locale: Some(locale.to_owned()),
        })
        .await
    {
        if !is_existing_locale_override(&error)
            || !effective_locale_matches(page, locale)
                .await
                .unwrap_or(false)
        {
            return Err(error);
        }
        return Ok(());
    }
    if !effective_locale_matches(page, locale).await? {
        return Err(CdpError::msg(
            "browser locale does not match requested locale",
        ));
    }
    Ok(())
}
