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
