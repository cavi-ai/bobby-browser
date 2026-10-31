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
    iframe_scoped_snapshot_descends_into_child_frames,
    scoped_snapshot_frame_targets_reach_the_second_frame,
    scoped_snapshot_targets_preserve_parent_frame_paths,
    scoped_snapshot_in_frame_descends_into_child_frames,
    scoped_frame_descent_respects_target_path_depth,
    scoped_iframe_descent_respects_the_node_budget,
    password_manager_fill_never_touches_the_field,
    password_manager_fill_requires_the_session_opt_in
);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn password_manager_fill_clicks_the_managers_prompt() {
    let (x, y) = cases::PASSWORD_MANAGER_PROMPT_POINT;
    let endpoint = runtime_tests::clicking_vision_node(x, y, Default::default()).await;
    let rig = Rig::chromium_with_vision_node(&endpoint).await;
    cases::password_manager_fill_clicks_the_managers_prompt(&rig).await;
}
