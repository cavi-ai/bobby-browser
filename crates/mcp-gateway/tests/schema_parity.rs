//! Cross-surface command contract drift guards.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
struct CommandContract {
    primitive: BTreeMap<String, VariantContract>,
    intent: BTreeMap<String, VariantContract>,
}

#[derive(Debug, PartialEq, Eq)]
struct VariantContract {
    fields: Vec<String>,
    required: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_schema<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
        return schema;
    };
    let name = reference
        .strip_prefix("#/$defs/")
        .unwrap_or_else(|| panic!("unsupported schema reference: {reference}"));
    &root["$defs"][name]
}

fn command_variants(root: &Value, name: &str) -> BTreeMap<String, VariantContract> {
    let union = &root["$defs"][name];
    union["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("{name} oneOf must be an array"))
        .iter()
        .map(|variant| {
            let kind = variant["properties"]["kind"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} variant must pin kind"));
            let input = resolve_schema(root, &variant["properties"]["input"]);
            let mut fields = input
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|properties| properties.keys().cloned())
                .collect::<Vec<_>>();
            fields.sort();
            let mut required = input
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|field| {
                    field
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}.{kind} required field must be a string"))
                        .to_owned()
                })
                .collect::<Vec<_>>();
            required.sort();
            (kind.to_owned(), VariantContract { fields, required })
        })
        .collect()
}

fn command_contract(root: &Value) -> CommandContract {
    CommandContract {
        primitive: command_variants(root, "PrimitiveCommand"),
        intent: command_variants(root, "IntentCommand"),
    }
}

fn rust_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(types::RuntimeCommand)).unwrap()
}

fn render_typescript_contract(contract: &CommandContract) -> String {
    fn render_group(output: &mut String, name: &str, variants: &BTreeMap<String, VariantContract>) {
        output.push_str("  ");
        output.push_str(name);
        output.push_str(": {\n");
        for (kind, contract) in variants {
            output.push_str("    ");
            output.push_str(&serde_json::to_string(kind).unwrap());
            output.push_str(": { fields: ");
            output.push_str(&serde_json::to_string(&contract.fields).unwrap());
            output.push_str(", required: ");
            output.push_str(&serde_json::to_string(&contract.required).unwrap());
            output.push_str(" },\n");
        }
        output.push_str("  },\n");
    }

    let mut output = String::from("export const COMMAND_CONTRACT = {\n");
    render_group(&mut output, "primitive", &contract.primitive);
    render_group(&mut output, "intent", &contract.intent);
    output.push_str("} as const;\n");
    output
}

fn assert_generated(path: &Path, expected: &str) {
    if std::env::var_os("UPDATE_COMMAND_CONTRACTS").is_some() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, expected).unwrap();
        return;
    }

    let actual = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("missing generated contract {}: {error}", path.display()));
    assert_eq!(
        actual,
        expected,
        "stale generated contract {}",
        path.display()
    );
}

#[test]
fn mcp_command_contract_matches_rust_wire_type() {
    let rust = command_contract(&rust_schema());
    let mcp = command_contract(&mcp_gateway::schema_for_test("command_execute"));
    assert_eq!(mcp, rust);
}

#[test]
fn generated_openapi_command_schema_matches_rust_wire_type() {
    let mut expected = serde_json::to_string_pretty(&rust_schema()).unwrap();
    expected.push('\n');
    assert_generated(
        &repo_root().join("docs/bobby-browser/source/openapi/command.schema.json"),
        &expected,
    );
}

#[test]
fn generated_typescript_command_contract_matches_rust_wire_type() {
    let expected = render_typescript_contract(&command_contract(&rust_schema()));
    assert_generated(
        &repo_root().join("packages/typescript-sdk/src/generated/command-contract.ts"),
        &expected,
    );
}

#[test]
fn openapi_command_uses_generated_schema() {
    let openapi =
        fs::read_to_string(repo_root().join("docs/bobby-browser/source/openapi/v1.yaml")).unwrap();
    assert!(
        openapi.contains("command:\n          $ref: \"./command.schema.json\""),
        "CommandEnvelope.command must reference command.schema.json"
    );
}
