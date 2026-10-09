//! Regressions first observed on a real site, reproduced on minimal local
//! pages against live Chromium. See `support/cases.rs` for each finding.

mod support;

use support::cases;
use support::rig::Rig;

macro_rules! chromium_case {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires installed Chrome or Chromium"]
        async fn $name() {
            let rig = Rig::chromium().await;
            cases::$name(&rig).await;
        }
    };
}

chromium_case!(hidden_subtrees_do_not_spend_the_node_budget);
chromium_case!(snapshot_target_scopes_the_tree);
chromium_case!(workflow_start_reports_the_settled_page);
chromium_case!(observe_after_navigate_includes_late_content);
chromium_case!(accessible_names_are_computed);
chromium_case!(secret_words_are_not_secrets);
chromium_case!(scheme_like_text_is_page_text);
chromium_case!(disclosed_credentials_are_withheld);
chromium_case!(large_dom_link_resolves_for_intent_follow);
chromium_case!(redacted_page_fields_do_not_block_actions);
chromium_case!(snapshot_budget_keeps_ancestors_of_kept_nodes);
chromium_case!(containers_are_not_named_from_content);
chromium_case!(type_text_reaches_the_visible_duplicate);
chromium_case!(intent_follow_clicks_the_visible_duplicate);
chromium_case!(snapshot_scopes_to_a_named_list);
chromium_case!(snapshot_targets_act_on_the_described_element);
chromium_case!(type_text_enter_reports_the_settled_page);
chromium_case!(intent_follow_post_state_shows_the_settled_page);
chromium_case!(type_text_enter_reports_the_rewritten_url);
chromium_case!(intent_follow_post_state_waits_for_fetched_content);
chromium_case!(intent_follow_waits_for_a_late_data_request);
chromium_case!(settles_beside_class_churn);
chromium_case!(settles_beside_text_churn);
chromium_case!(settles_beside_moving_children);
chromium_case!(settles_beside_combined_churn);
chromium_case!(network_tracking_survives_a_heavy_page);
chromium_case!(type_text_enter_reports_a_late_title);
chromium_case!(type_text_enter_waits_for_a_landed_response);
chromium_case!(navigate_settles_on_a_polling_page);
chromium_case!(navigate_ignores_requests_the_navigation_cancelled);
chromium_case!(type_text_enter_reports_a_keydown_navigation_at_once);
chromium_case!(actions_wait_for_a_late_target);
chromium_case!(actions_fail_a_missing_target_within_one_bound);
chromium_case!(navigate_waits_for_late_scripts);
chromium_case!(page_titles_withhold_disclosed_credentials);
chromium_case!(observation_carries_each_text_once);
chromium_case!(sign_in_fields_show_their_labels);
// `oversized_page_reports_truncation_not_target_not_found` is Firefox-only:
// the 1024-node candidate cap it exercises exists only in the Firefox companion.
