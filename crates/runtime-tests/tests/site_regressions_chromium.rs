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
    password_manager_fill_requires_the_session_opt_in,
    a_split_code_with_one_named_box_takes_the_code,
    select_one_picks_the_exact_option_of_an_aria_combobox,
    a_submit_without_expected_state_is_verified_by_the_page,
    control_ids_stay_on_unnamed_controls_when_the_form_grows,
    aria_checkboxes_and_radios_take_set_checked,
    editable_regions_read_back_their_typed_text,
    type_text_replaces_a_prefilled_value,
    an_exact_name_outranks_one_with_a_required_marker,
    snapshot_targets_reach_controls_inside_custom_elements,
    complete_form_honors_a_field_ordinal,
    button_menus_take_select_one_and_select_many,
    text_boxes_pick_a_suggestion_without_a_role,
    a_combobox_without_the_value_lists_its_options,
    type_text_css_prefers_the_editable_node,
    a_phone_field_is_a_text_box,
    a_custom_select_reads_and_verifies_its_shown_value,
    a_checked_box_satisfies_its_required_group,
    a_file_slot_reports_the_file_its_row_shows,
    a_snapshot_target_survives_a_sibling_losing_its_name,
    a_control_id_survives_its_name_changing,
    activating_a_control_that_removes_itself_completes,
    activating_a_control_that_navigates_completes,
    a_shown_value_reads_back_with_its_own_spacing,
    a_combobox_waits_through_its_loading_row,
    a_combobox_presses_an_option_that_moves_as_it_is_read,
    a_chosen_button_reads_as_checked,
    inspect_reads_a_listbox_one_option_per_line,
    element_screenshots_show_the_element,
    screenshots_name_their_session_and_page,
    nested_shadow_controls_act_from_their_targets,
    a_shadow_combobox_takes_its_options_and_text,
    a_fill_form_workflow_fills_and_submits,
    an_extract_page_workflow_reads_the_fields,
    a_collect_links_workflow_reads_every_page
);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_file_control_reports_the_file_its_widget_shows() {
    let dir = tempfile::tempdir().expect("upload dir");
    let rig = Rig::chromium_with_upload_roots(vec![dir.path().to_path_buf()]).await;
    cases::a_file_control_reports_the_file_its_widget_shows(&rig, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_file_control_id_uploads_into_its_own_slot() {
    let dir = tempfile::tempdir().expect("upload dir");
    let rig = Rig::chromium_with_upload_roots(vec![dir.path().to_path_buf()]).await;
    cases::a_file_control_id_uploads_into_its_own_slot(&rig, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_removed_file_is_replaced_and_a_file_control_clears() {
    let dir = tempfile::tempdir().expect("upload dir");
    let rig = Rig::chromium_with_upload_roots(vec![dir.path().to_path_buf()]).await;
    cases::a_removed_file_is_replaced_and_a_file_control_clears(&rig, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn an_attach_button_uploads_into_its_slot() {
    let dir = tempfile::tempdir().expect("upload dir");
    let rig = Rig::chromium_with_upload_roots(vec![dir.path().to_path_buf()]).await;
    cases::an_attach_button_uploads_into_its_slot(&rig, dir.path()).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn uploads_by_label_refuse_a_slot_that_shows_a_file() {
    let dir = tempfile::tempdir().expect("upload dir");
    let rig = Rig::chromium_with_upload_roots(vec![dir.path().to_path_buf()]).await;
    cases::uploads_by_label_refuse_a_slot_that_shows_a_file(&rig, dir.path()).await;
}

macro_rules! workflow_file_cases {
    ($($name:ident),+ $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires installed Chrome or Chromium"]
        async fn $name() {
            let dir = tempfile::tempdir().expect("workflow dir");
            let rig = Rig::chromium_with_workflow_dirs(vec![dir.path().to_path_buf()]).await;
            cases::$name(&rig, dir.path()).await;
        }
    )+};
}

workflow_file_cases!(
    a_workflow_file_fills_a_form_from_its_inputs,
    a_rerun_from_a_step_keeps_earlier_results,
    a_for_each_step_runs_a_workflow_per_item,
);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_humanized_click_reaches_an_option_scrolled_out_of_its_list() {
    let rig = Rig::chromium_humanized().await;
    cases::a_humanized_click_reaches_an_option_scrolled_out_of_its_list(&rig).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn credential_fill_fills_only_on_its_origin() {
    let fixture = cases::credential_fixture().await;
    let rig = Rig::chromium_with_credentials(fixture.credentials.clone()).await;
    cases::a_configured_credential_fills_its_password_field(&rig, &fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn password_manager_fill_clicks_the_managers_prompt() {
    let (x, y) = cases::PASSWORD_MANAGER_PROMPT_POINT;
    let endpoint = runtime_tests::clicking_vision_node(x, y, 0.9, Default::default()).await;
    let rig = Rig::chromium_with_vision_node(&endpoint).await;
    cases::password_manager_fill_clicks_the_managers_prompt(&rig).await;
}

/// Screenshots of a display with two device pixels per CSS pixel are twice
/// the viewport's size; a vision click read off one lands on its control.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_vision_click_lands_on_a_high_density_display() {
    let endpoint = runtime_tests::pointing_vision_node().await;
    let rig = Rig::chromium_with_vision_node_at(&endpoint, 2.0).await;
    cases::a_vision_click_lands_on_the_control_the_screenshot_shows(&rig).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn password_manager_fill_clicks_the_prompt_on_a_high_density_display() {
    let endpoint = runtime_tests::pointing_vision_node().await;
    let rig = Rig::chromium_with_vision_node_at(&endpoint, 2.0).await;
    cases::password_manager_fill_clicks_the_managers_prompt(&rig).await;
}

/// Local models report modest confidence even on a correct point; the click is
/// still refused on page controls and verified by the fill.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn password_manager_fill_accepts_a_modest_confidence_prompt_click() {
    let (x, y) = cases::PASSWORD_MANAGER_PROMPT_POINT;
    let endpoint = runtime_tests::clicking_vision_node(x, y, 0.7, Default::default()).await;
    let rig = Rig::chromium_with_vision_node(&endpoint).await;
    cases::password_manager_fill_clicks_the_managers_prompt(&rig).await;
}
