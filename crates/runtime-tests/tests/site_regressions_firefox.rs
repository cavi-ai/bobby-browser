//! Regressions first observed on a real site, reproduced on minimal local
//! pages against the installed Firefox companion. One Firefox runs every
//! case in sequence; each failing case is named in the final panic.

mod support;

use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use support::cases;
use support::rig::Rig;

macro_rules! run_cases {
    ($rig:expr, $failures:expr, $($case:ident),+ $(,)?) => {
        $(
            if AssertUnwindSafe(cases::$case(&$rig)).catch_unwind().await.is_err() {
                $failures.push(stringify!($case));
            }
        )+
    };
}

#[tokio::test]
#[ignore = "requires installed headed Firefox and paired test profile"]
async fn site_regressions_hold_on_firefox() {
    let rig = Rig::firefox().await;
    let mut failures: Vec<&str> = Vec::new();
    run_cases!(
        rig,
        failures,
        hidden_subtrees_do_not_spend_the_node_budget,
        snapshot_target_scopes_the_tree,
        workflow_start_reports_the_settled_page,
        observe_after_navigate_includes_late_content,
        accessible_names_are_computed,
        secret_words_are_not_secrets,
        disclosed_credentials_are_withheld,
        large_dom_link_resolves_for_intent_follow,
        oversized_page_reports_truncation_not_target_not_found,
        redacted_page_fields_do_not_block_actions,
        snapshot_budget_keeps_ancestors_of_kept_nodes,
        containers_are_not_named_from_content,
    );
    assert!(failures.is_empty(), "failing cases: {failures:?}");
}
