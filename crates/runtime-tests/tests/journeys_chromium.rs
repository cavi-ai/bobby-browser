//! End-to-end agent journeys against live Chromium. See `support/journeys.rs`.

mod support;

use support::journeys::{self, Dirs};
use support::rig::Rig;

macro_rules! chromium_journey {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires installed Chrome or Chromium"]
        async fn $name() {
            let dirs = Dirs::new();
            let rig = Rig::chromium_with_upload_roots(dirs.upload_roots()).await;
            journeys::$name(&rig, &dirs).await;
        }
    };
}

chromium_journey!(j1_sign_in_redirect_then_app);
chromium_journey!(j2_search_and_follow_result);
chromium_journey!(j3_form_validation_then_success);
chromium_journey!(j4_upload_hidden_and_visible);
chromium_journey!(j5_popup_and_frame);
chromium_journey!(j6_cross_site_navigation_recovery);
