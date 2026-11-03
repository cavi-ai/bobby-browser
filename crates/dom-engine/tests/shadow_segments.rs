use dom_engine::shadow_segment_descriptor;
use types::TargetSpec;

#[test]
fn shadow_descriptors_preserve_ordinals_for_each_exact_target_kind() {
    for target in [
        TargetSpec {
            css: Some("input".into()),
            ..TargetSpec::default()
        },
        TargetSpec {
            test_id: Some("email".into()),
            ..TargetSpec::default()
        },
        TargetSpec {
            role: Some("textbox".into()),
            accessible_name: Some("Email".into()),
            ..TargetSpec::default()
        },
    ] {
        for ordinal in [0, 1] {
            let target = TargetSpec {
                ordinal: Some(ordinal),
                ..target.clone()
            };
            let descriptor = shadow_segment_descriptor(&target).unwrap();
            assert_eq!(descriptor["ordinal"], ordinal);
        }
    }
}

#[test]
fn unnamed_file_shadow_targets_keep_type_name_and_ordinal() {
    let target = TargetSpec {
        role: Some("button".into()),
        accessible_name: Some(String::new()),
        attributes: [("type".into(), "file".into())].into_iter().collect(),
        ordinal: Some(1),
        ..TargetSpec::default()
    };
    let descriptor = shadow_segment_descriptor(&target).unwrap();
    assert_eq!(descriptor["name"], "");
    assert_eq!(descriptor["inputType"], "file");
    assert_eq!(descriptor["ordinal"], 1);
}
