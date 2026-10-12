//! Browser behavior cases on minimal local pages against live Chromium, one
//! runtime per case. See `support/cases.rs`.

mod support;

use support::cases;
use support::rig::Rig;

macro_rules! chromium_cases {
    ($($name:ident),+ $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires installed Chrome or Chromium"]
        async fn $name() {
            let rig = Rig::chromium().await;
            cases::$name(&rig).await;
        }
    )+};
}

cases::every_case!(
    chromium_cases,
    iframe_scoped_targets_reach_the_second_frame,
    scoped_snapshot_frame_targets_reach_the_second_frame,
    scoped_iframe_descent_respects_the_node_budget
);
