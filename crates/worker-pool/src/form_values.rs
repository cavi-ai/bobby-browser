//! The values of filled form fields, appended to page text. A page's
//! rendered text never holds what its fields contain; this section lists
//! them by field name under [`FORM_VALUES_HEADING`].
//!
//! Left out: password fields; fields whose `autocomplete` asks for a
//! one-time code, a password or payment card data; fields whose identifiers,
//! accessible name, label, placeholder or description name a secret; values shaped
//! like a credential or card number; fields inside frames; and controls whose
//! text the page already shows.

use types::{FormControl, FormControlKind, FormControlState, FormSnapshot, PageId};

use crate::form_snapshot::{normalize_form_snapshot, RawFormSnapshot};
use crate::secret_material::contains_secret_material;

/// Heading of the field-values section in page text.
pub(crate) const FORM_VALUES_HEADING: &str = "Form field values:";
/// Controls read from the page for the section.
pub(crate) const FORM_VALUE_CONTROLS: usize = 64;
const MAX_FIELDS: usize = 50;
const MAX_VALUE_CHARS: usize = 256;
const MAX_NAME_CHARS: usize = 80;

/// Field kinds whose value is text the agent typed or the page filled.
const LISTED_KINDS: &[FormControlKind] = &[
    FormControlKind::Text,
    FormControlKind::Email,
    FormControlKind::Search,
    FormControlKind::Number,
    FormControlKind::Date,
    FormControlKind::Time,
    FormControlKind::DateTimeLocal,
    FormControlKind::Combobox,
];

/// Words in a field's metadata that mark its value as a secret.
const SECRET_MARKERS: &[&str] = &[
    "authorization",
    "authentication",
    "bearer",
    "token",
    "secret",
    "password",
    "passwd",
    "passcode",
    "api-key",
    "api_key",
    "api key",
    "apikey",
    "credential",
    "one-time",
    "one time",
    "verification code",
    "security code",
];

/// Whole words in a field's metadata that mark it as a code or card field.
const SECRET_WORDS: &[&str] = &["otp", "totp", "2fa", "mfa", "cvc", "cvv", "csc", "pin"];

/// Filter DOM identifiers before normalizing the snapshot for display.
pub(crate) fn with_inspection_form_values(text: String, page_id: &PageId, encoded: &str) -> String {
    let Ok(mut raw) = serde_json::from_str::<RawFormSnapshot>(encoded) else {
        return text;
    };
    raw.controls.retain(|control| {
        control.frame_path.is_empty()
            && ![control.dom_id.as_deref(), control.dom_name.as_deref()]
                .into_iter()
                .flatten()
                .any(identifier_names_a_secret)
    });
    match normalize_form_snapshot(page_id.clone(), raw, FORM_VALUE_CONTROLS) {
        Ok(snapshot) => with_form_values(text, &snapshot),
        Err(_) => text,
    }
}

/// `text` followed by the snapshot's listed field values, or `text` as it
/// was when no field is listed.
fn with_form_values(mut text: String, snapshot: &FormSnapshot) -> String {
    let lines: Vec<String> = snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(snapshot.unowned_controls.iter())
        .filter_map(listed_value)
        .take(MAX_FIELDS)
        .collect();
    if lines.is_empty() {
        return text;
    }
    if !text.is_empty() {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(FORM_VALUES_HEADING);
    for line in lines {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

/// `name: "value"` for a field the section may list.
fn listed_value(control: &FormControl) -> Option<String> {
    let FormControlState::Text { value } = &control.state else {
        return None;
    };
    let value = value.trim();
    if value.is_empty() || !LISTED_KINDS.contains(&control.control_kind) {
        return None;
    }
    if control
        .target
        .as_ref()
        .is_some_and(|target| !target.frame_path.is_empty())
    {
        return None;
    }
    if control
        .autocomplete
        .as_deref()
        .is_some_and(sensitive_autocomplete)
    {
        return None;
    }
    let metadata = [
        control.autocomplete.as_deref(),
        control.accessible_name.as_deref(),
        control.label.as_deref(),
        control.placeholder.as_deref(),
        control.description.as_deref(),
    ];
    if metadata.into_iter().flatten().any(names_a_secret) {
        return None;
    }
    if contains_secret_material(value) || credential_shaped(value) {
        return None;
    }
    let name = [
        control.accessible_name.as_deref(),
        control.label.as_deref(),
        control.placeholder.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|name| !name.is_empty())
    .unwrap_or("Unnamed field");
    Some(format!(
        "{}: {:?}",
        clip(name, MAX_NAME_CHARS),
        clip(value, MAX_VALUE_CHARS)
    ))
}

/// An `autocomplete` token for a one-time code, a password or card data.
fn sensitive_autocomplete(autocomplete: &str) -> bool {
    autocomplete.split_ascii_whitespace().any(|token| {
        let token = token.to_ascii_lowercase();
        matches!(
            token.as_str(),
            "one-time-code" | "current-password" | "new-password"
        ) || token.starts_with("cc-")
    })
}

fn names_a_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    SECRET_MARKERS.iter().any(|marker| lower.contains(marker))
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| SECRET_WORDS.contains(&word))
}

/// DOM identifiers also use camel case, including acronyms like loginOTPCode.
fn identifier_names_a_secret(identifier: &str) -> bool {
    let mut words = String::with_capacity(identifier.len());
    let mut chars = identifier.chars().peekable();
    let mut previous = ' ';
    while let Some(current) = chars.next() {
        if current.is_ascii_uppercase()
            && (previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase()
                    && chars.peek().is_some_and(char::is_ascii_lowercase)))
        {
            words.push(' ');
        }
        words.push(current);
        previous = current;
    }
    names_a_secret(identifier) || names_a_secret(&words)
}

/// Twelve or more digits in a run broken only by single spaces or dashes,
/// or 32 or more characters of letters, digits, `_` and `-`.
fn credential_shaped(value: &str) -> bool {
    let (mut digits, mut word, mut after_separator) = (0, 0, false);
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            word += 1;
        } else {
            word = 0;
        }
        if c.is_ascii_digit() {
            digits += 1;
            after_separator = false;
        } else if (c == ' ' || c == '-') && digits > 0 && !after_separator {
            after_separator = true;
        } else {
            digits = 0;
            after_separator = false;
        }
        if digits >= 12 || word >= 32 {
            return true;
        }
    }
    false
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{FormControlTarget, FormControlValidity, PageId, SemanticTargetSegment};

    fn control(
        kind: FormControlKind,
        name: &str,
        autocomplete: Option<&str>,
        state: FormControlState,
    ) -> FormControl {
        FormControl {
            id: format!("control-{name}"),
            form_id: None,
            group_id: None,
            target: Some(FormControlTarget {
                attributes: Default::default(),
                role: "textbox".into(),
                accessible_name: name.into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            }),
            control_kind: kind,
            accessible_name: Some(name.into()),
            label: None,
            description: None,
            placeholder: None,
            autocomplete: autocomplete.map(Into::into),
            state,
            constraints: Default::default(),
            validity: FormControlValidity {
                will_validate: true,
                valid: true,
                flags: Vec::new(),
                message: None,
                described_by: Vec::new(),
            },
            options: Vec::new(),
            supported_operations: Vec::new(),
        }
    }

    fn text(value: &str) -> FormControlState {
        FormControlState::Text {
            value: value.into(),
        }
    }

    fn snapshot(controls: Vec<FormControl>) -> FormSnapshot {
        FormSnapshot {
            schema_version: types::FORM_SNAPSHOT_SCHEMA_VERSION,
            page_id: PageId::new(),
            forms: Vec::new(),
            unowned_controls: controls,
            truncated: false,
            page_busy: false,
        }
    }

    #[test]
    fn filled_fields_follow_the_page_text_under_the_heading() {
        let listed = with_form_values(
            "Your account".into(),
            &snapshot(vec![
                control(
                    FormControlKind::Email,
                    "Email",
                    Some("username"),
                    text("ada@example.com"),
                ),
                control(FormControlKind::Text, "City", None, text("Lyon")),
                control(
                    FormControlKind::Text,
                    "Empty",
                    None,
                    FormControlState::Empty,
                ),
            ]),
        );
        assert_eq!(
            listed,
            "Your account\n\nForm field values:\nEmail: \"ada@example.com\"\nCity: \"Lyon\""
        );
    }

    #[test]
    fn secrets_and_frame_fields_stay_out() {
        let mut framed = control(FormControlKind::Text, "Card holder", None, text("Ada"));
        framed.target.as_mut().unwrap().frame_path = vec![SemanticTargetSegment {
            role: "iframe".into(),
            accessible_name: "Payment".into(),
            ordinal: None,
        }];
        let mut placeholder_secret = control(FormControlKind::Text, "Key", None, text("plum"));
        placeholder_secret.placeholder = Some("Paste your API key".into());
        let withheld = snapshot(vec![
            control(
                FormControlKind::Password,
                "Password",
                Some("current-password"),
                FormControlState::Redacted { present: true },
            ),
            control(
                FormControlKind::Text,
                "Code",
                Some("one-time-code"),
                text("424242"),
            ),
            control(
                FormControlKind::Text,
                "Saved",
                Some("current-password"),
                text("maple-harbor"),
            ),
            control(
                FormControlKind::Text,
                "Number",
                Some("cc-number"),
                text("4111"),
            ),
            control(FormControlKind::Text, "API token", None, text("violet")),
            control(FormControlKind::Text, "OTP", None, text("1234")),
            placeholder_secret,
            control(
                FormControlKind::Text,
                "Note",
                None,
                text("password=hunter2-hunter2"),
            ),
            control(
                FormControlKind::Text,
                "Reference",
                None,
                text("4111 1111 1111 1111"),
            ),
            control(
                FormControlKind::Text,
                "Reference",
                None,
                text("abcdefghijklmnopqrstuvwxyz0123456789"),
            ),
            framed,
            control(
                FormControlKind::ContentEditable,
                "Body",
                None,
                text("shown"),
            ),
        ]);
        assert_eq!(with_form_values("Page".into(), &withheld), "Page");
    }

    #[test]
    fn long_values_and_names_are_clipped() {
        let listed = with_form_values(
            String::new(),
            &snapshot(vec![control(
                FormControlKind::Text,
                &"Name ".repeat(40),
                None,
                text(&"lorem ipsum ".repeat(40)),
            )]),
        );
        let line = listed.lines().nth(1).expect("value line");
        let (name, value) = line.split_once(": ").expect("name and value");
        assert_eq!(name, format!("{}...", "Name ".repeat(16)));
        assert_eq!(value.chars().count(), MAX_VALUE_CHARS + "...".len() + 2);
        assert!(value.ends_with("...\""), "{line}");
        assert!(listed.starts_with(FORM_VALUES_HEADING));
    }
}
