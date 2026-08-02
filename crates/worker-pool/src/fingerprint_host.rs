//! Chromium CDP adapter for [`fingerprinting::FingerprintHost`].

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetLocaleOverrideParams, SetTimezoneOverrideParams,
};
use chromiumoxide::Page;
use fingerprinting::{FingerprintApplyError, FingerprintApplyPlan, FingerprintHost};

pub struct ChromiumPageHost<'a> {
    pub page: &'a Page,
}

#[async_trait]
impl FingerprintHost for ChromiumPageHost<'_> {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        self.page
            .set_user_agent(plan.user_agent.as_str())
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        self.page
            .emulate_locale(SetLocaleOverrideParams {
                locale: Some(plan.locale.clone()),
            })
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        self.page
            .emulate_timezone(SetTimezoneOverrideParams {
                timezone_id: plan.timezone_id.clone(),
            })
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        let metrics = SetDeviceMetricsOverrideParams::builder()
            .width(plan.device_metrics.width as i64)
            .height(plan.device_metrics.height as i64)
            .device_scale_factor(plan.device_metrics.device_scale_factor)
            .mobile(plan.device_metrics.mobile)
            .build()
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        self.page
            .execute(metrics)
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        self.page
            .add_init_script(plan.init_script.as_str())
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        Ok(())
    }
}
