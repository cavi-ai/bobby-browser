//! Portable apply plan and host trait for engine adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::FingerprintApplyError;
use crate::script::{build_init_script, build_probe_script};
use crate::{create_session, FingerprintConfig, FingerprintSession, ScreenResolution};

/// Device metrics for CDP `Emulation.setDeviceMetricsOverride` / BiDi viewport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceMetrics {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

impl From<&ScreenResolution> for DeviceMetrics {
    fn from(screen: &ScreenResolution) -> Self {
        Self {
            width: screen.width,
            height: screen.available_height.max(1),
            device_scale_factor: screen.pixel_ratio,
            mobile: false,
        }
    }
}

/// Engine-agnostic fingerprint apply payload.
///
/// Chromium, Firefox, and future hosts consume the same plan: inject
/// [`Self::init_script`], override UA/locale/timezone, and set device metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintApplyPlan {
    pub session: FingerprintSession,
    pub init_script: String,
    pub probe_script: String,
    pub user_agent: String,
    pub locale: String,
    pub timezone_id: String,
    pub device_metrics: DeviceMetrics,
}

impl FingerprintApplyPlan {
    /// Build a plan from config. Returns `None` when fingerprinting is disabled.
    pub fn from_config(config: &FingerprintConfig) -> Result<Option<Self>, FingerprintApplyError> {
        if !config.enabled {
            return Ok(None);
        }
        let session = create_session(config);
        session.validate_consistency()?;
        Ok(Some(Self::from_session(session)))
    }

    pub fn from_session(session: FingerprintSession) -> Self {
        let init_script = build_init_script(&session);
        let probe_script = build_probe_script();
        let device_metrics = DeviceMetrics::from(&session.screen_resolution);
        Self {
            user_agent: session.user_agent.clone(),
            locale: session.locale.clone(),
            timezone_id: session.timezone_id.clone(),
            device_metrics,
            init_script,
            probe_script,
            session,
        }
    }
}

/// Host that can apply a [`FingerprintApplyPlan`] to a browsing context.
///
/// Implement this for Chromium (CDP), Firefox (BiDi + extension), and any
/// future engine without changing the fingerprinting crate.
#[async_trait]
pub trait FingerprintHost: Send + Sync {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError>;
}
