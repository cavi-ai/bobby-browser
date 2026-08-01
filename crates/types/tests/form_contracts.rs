use serde_json::{json, Value};
use types::{FormSnapshot, FORM_SNAPSHOT_SCHEMA_VERSION};

fn valid_snapshot() -> Value {
    json!({
        "schemaVersion": 1,
        "pageId": "00000000-0000-4000-8000-000000000001",
        "forms": [{
            "id": "form-1",
            "target": null,
            "accessibleName": "Account application",
            "description": "Required fields",
            "groups": [{
                "id": "group-contact",
                "label": "Contact details",
                "description": null,
                "controlIds": ["email", "password"]
            }],
            "controls": [{
                "id": "email",
                "formId": "form-1",
                "groupId": "group-contact",
                "target": {
                    "role": "textbox",
                    "accessibleName": "Email address",
                    "ordinal": null,
                    "framePath": [],
                    "shadowPath": []
                },
                "controlKind": "email",
                "accessibleName": "Email address",
                "label": "Email address",
                "description": "Use a work address",
                "placeholder": "name@example.com",
                "autocomplete": "email",
                "state": { "kind": "text", "value": "ada@example.test" },
                "constraints": {
                    "required": true,
                    "readOnly": false,
                    "disabled": false,
                    "pattern": "[^@]+@[^@]+",
                    "minLength": 3,
                    "maxLength": 254,
                    "min": null,
                    "max": null,
                    "step": null,
                    "multiple": false,
                    "accept": []
                },
                "validity": {
                    "willValidate": true,
                    "valid": true,
                    "flags": [],
                    "message": null,
                    "describedBy": ["Use a work address"]
                },
                "options": [],
                "supportedOperations": ["setText", "clear"]
            }, {
                "id": "password",
                "formId": "form-1",
                "groupId": "group-contact",
                "target": {
                    "role": "textbox",
                    "accessibleName": "Password",
                    "ordinal": null,
                    "framePath": [],
                    "shadowPath": []
                },
                "controlKind": "password",
                "accessibleName": "Password",
                "label": "Password",
                "description": null,
                "placeholder": null,
                "autocomplete": "current-password",
                "state": { "kind": "redacted", "present": true },
                "constraints": {
                    "required": true,
                    "readOnly": false,
                    "disabled": false,
                    "pattern": null,
                    "minLength": 8,
                    "maxLength": null,
                    "min": null,
                    "max": null,
                    "step": null,
                    "multiple": false,
                    "accept": []
                },
                "validity": {
                    "willValidate": true,
                    "valid": true,
                    "flags": [],
                    "message": null,
                    "describedBy": []
                },
                "options": [],
                "supportedOperations": ["setText", "clear"]
            }, {
                "id": "submit",
                "formId": "form-1",
                "groupId": null,
                "target": {
                    "role": "button",
                    "accessibleName": "Continue",
                    "ordinal": null,
                    "framePath": [],
                    "shadowPath": []
                },
                "controlKind": "submit",
                "accessibleName": "Continue",
                "label": null,
                "description": null,
                "placeholder": null,
                "autocomplete": null,
                "state": { "kind": "empty" },
                "constraints": {
                    "required": false,
                    "readOnly": false,
                    "disabled": false,
                    "pattern": null,
                    "minLength": null,
                    "maxLength": null,
                    "min": null,
                    "max": null,
                    "step": null,
                    "multiple": false,
                    "accept": []
                },
                "validity": {
                    "willValidate": false,
                    "valid": true,
                    "flags": [],
                    "message": null,
                    "describedBy": []
                },
                "options": [],
                "supportedOperations": ["activate"]
            }],
            "submitControlIds": ["submit"],
            "resetControlIds": [],
            "validity": {
                "valid": true,
                "invalidControlIds": []
            }
        }],
        "unownedControls": [],
        "truncated": false
    })
}

#[test]
fn canonical_form_snapshot_round_trips_exactly_and_preserves_redacted_presence() {
    let expected = valid_snapshot();
    let snapshot: FormSnapshot = serde_json::from_value(expected.clone()).unwrap();

    assert_eq!(FORM_SNAPSHOT_SCHEMA_VERSION, 1);
    assert_eq!(serde_json::to_value(snapshot).unwrap(), expected);
}

#[test]
fn canonical_form_snapshot_rejects_unsupported_versions_and_unknown_fields() {
    let mut version = valid_snapshot();
    version["schemaVersion"] = json!(2);
    assert!(serde_json::from_value::<FormSnapshot>(version).is_err());

    let mut unknown = valid_snapshot();
    unknown["engineNodeId"] = json!("backend-42");
    assert!(serde_json::from_value::<FormSnapshot>(unknown).is_err());
}

#[test]
fn canonical_form_snapshot_rejects_secret_text_for_password_controls() {
    let mut snapshot = valid_snapshot();
    snapshot["forms"][0]["controls"][1]["state"] = json!({"kind":"text","value":"vault-secret-92"});

    assert!(serde_json::from_value::<FormSnapshot>(snapshot).is_err());
}

#[test]
fn canonical_form_snapshot_rejects_dangling_group_and_submit_references() {
    let mut group = valid_snapshot();
    group["forms"][0]["groups"][0]["controlIds"] = json!(["missing"]);
    assert!(serde_json::from_value::<FormSnapshot>(group).is_err());

    let mut submit = valid_snapshot();
    submit["forms"][0]["submitControlIds"] = json!(["email"]);
    assert!(serde_json::from_value::<FormSnapshot>(submit).is_err());
}

#[test]
fn canonical_form_snapshot_rejects_one_sided_group_membership() {
    let mut value = valid_snapshot();
    value["forms"][0]["groups"][0]["controlIds"] = serde_json::json!(["email"]);
    assert!(serde_json::from_value::<FormSnapshot>(value).is_err());
}

#[test]
fn canonical_form_snapshot_rejects_oversized_text_and_collections() {
    let mut text = valid_snapshot();
    text["forms"][0]["accessibleName"] = json!("x".repeat(2_049));
    assert!(serde_json::from_value::<FormSnapshot>(text).is_err());

    let mut controls = valid_snapshot();
    let control = controls["forms"][0]["controls"][0].clone();
    controls["forms"][0]["controls"] = Value::Array((0..513).map(|_| control.clone()).collect());
    assert!(serde_json::from_value::<FormSnapshot>(controls).is_err());
}
