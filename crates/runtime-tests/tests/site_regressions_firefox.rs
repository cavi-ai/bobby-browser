//! Regressions first observed on a real site, reproduced on minimal local
//! pages against the installed Firefox companion. One Firefox runs every
//! case in sequence; each failing case is named in the final panic.

mod support;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

use futures_util::FutureExt;
use support::cases;
use support::rig::Rig;

type Case = for<'a> fn(&'a Rig) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

/// The cases are a table of boxed futures walked by one loop. Awaiting each
/// case inline made this test's debug-build poll frame grow with every case
/// and overflowed the 2 MB test-thread stack before the first case finished.
macro_rules! case_table {
    ($($case:ident),+ $(,)?) => {
        [$((stringify!($case), (|rig| Box::pin(cases::$case(rig))) as Case)),+]
    };
}

#[tokio::test]
#[ignore = "requires installed headed Firefox and paired test profile"]
async fn site_regressions_hold_on_firefox() {
    let rig = Rig::firefox().await;
    let mut failures: Vec<&str> = Vec::new();
    let table = case_table!(
        hidden_subtrees_do_not_spend_the_node_budget,
        snapshot_target_scopes_the_tree,
        workflow_start_reports_the_settled_page,
        observe_after_navigate_includes_late_content,
        accessible_names_are_computed,
        secret_words_are_not_secrets,
        scheme_like_text_is_page_text,
        disclosed_credentials_are_withheld,
        large_dom_link_resolves_for_intent_follow,
        oversized_page_reports_truncation_not_target_not_found,
        redacted_page_fields_do_not_block_actions,
        snapshot_budget_keeps_ancestors_of_kept_nodes,
        containers_are_not_named_from_content,
        type_text_reaches_the_visible_duplicate,
        intent_follow_clicks_the_visible_duplicate,
        snapshot_scopes_to_a_named_list,
        snapshot_targets_act_on_the_described_element,
        type_text_enter_reports_the_settled_page,
        intent_follow_post_state_shows_the_settled_page,
        type_text_enter_reports_the_rewritten_url,
        intent_follow_post_state_waits_for_fetched_content,
        intent_follow_waits_for_a_late_data_request,
        settles_beside_class_churn,
        settles_beside_text_churn,
        settles_beside_moving_children,
        settles_beside_combined_churn,
        network_tracking_survives_a_heavy_page,
        browser_events_survive_a_request_burst,
        type_text_enter_reports_a_late_title,
        type_text_enter_waits_for_a_landed_response,
        navigate_settles_on_a_polling_page,
        navigate_ignores_requests_the_navigation_cancelled,
        type_text_enter_reports_a_keydown_navigation_at_once,
        actions_wait_for_a_late_target,
        actions_fail_a_missing_target_within_one_bound,
        navigate_waits_for_late_scripts,
        page_titles_withhold_disclosed_credentials,
        observation_carries_each_text_once,
        sign_in_fields_show_their_labels,
    );
    for (name, case) in table {
        if AssertUnwindSafe(case(&rig)).catch_unwind().await.is_err() {
            failures.push(name);
        }
    }
    assert!(failures.is_empty(), "failing cases: {failures:?}");
}
