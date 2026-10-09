//! End-to-end agent journeys against live Chromium. See `support/journeys.rs`.

mod support;

use support::journeys::{self, Dirs};
use support::rig::Rig;

macro_rules! chromium_journeys {
    ($($name:ident),+ $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires installed Chrome or Chromium"]
        async fn $name() {
            let dirs = Dirs::new();
            let rig = Rig::chromium_with_upload_roots(dirs.upload_roots()).await;
            journeys::$name(&rig, &dirs).await;
        }
    )+};
}

journeys::every_journey!(chromium_journeys);
