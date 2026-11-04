//! Recognizes a one-time-code step (a single code field, or one character per
//! box) from a form snapshot and the page's visible text. Structural only:
//! nothing here depends on a site.

use types::{
    Evidence, FormControl, FormControlKind, FormControlOperation, FormControlState,
    FormControlTarget, FormSnapshot, IntentHints,
};

const MIN_BOXES: usize = 4;
const MAX_BOXES: usize = 8;
const MAX_HINT_CHARS: usize = 200;

/// One field of a code step, in page order.
#[derive(Debug, Clone)]
pub(crate) struct CodeField {
    pub id: String,
    pub target: Option<FormControlTarget>,
    pub max_length: Option<u32>,
}

/// The code step a page is showing.
#[derive(Debug, Clone)]
pub(crate) struct CodeStep {
    /// A single field, or the boxes of a split code in visual order.
    pub fields: Vec<CodeField>,
    pub code_length: Option<u32>,
    pub destination_hint: Option<String>,
    /// The control that submits the step, when the page offers one.
    pub submit: Option<FormControlTarget>,
}

impl CodeStep {
    pub(crate) fn evidence(&self) -> Evidence {
        Evidence::CodeEntry {
            fields: self
                .fields
                .iter()
                .filter_map(|field| field.target.clone())
                .collect(),
            code_length: self.code_length,
            destination_hint: self.destination_hint.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strength {
    Weak,
    Strong,
}

/// The code step in `snapshot`, or `None` when the page shows none. `hints`
/// pins the field when the caller names it; otherwise the page decides.
pub(crate) fn detect(
    snapshot: &FormSnapshot,
    page_text: &str,
    hints: Option<&IntentHints>,
) -> Option<CodeStep> {
    let controls = ordered_controls(snapshot);
    let fields = match hints.filter(|hints| names_a_control(hints)) {
        Some(hints) => hinted_fields(&controls, hints)?,
        None => detected_fields(&controls, page_text)?,
    };
    let first = controls
        .iter()
        .find(|control| control.id == fields[0].id)
        .copied()?;
    let code_length = if fields.len() > 1 {
        u32::try_from(fields.len()).ok()
    } else {
        single_code_length(first, page_text)
    };
    Some(CodeStep {
        submit: submit_target(snapshot, &controls, first),
        destination_hint: destination_hint(page_text),
        code_length,
        fields,
    })
}

fn names_a_control(hints: &IntentHints) -> bool {
    hints.role.is_some()
        || hints.accessible_name.is_some()
        || hints.near_text.is_some()
        || hints.ordinal.is_some()
}

/// Every control in page order. Snapshot ids number controls in document
/// order across forms and unowned controls alike.
fn ordered_controls(snapshot: &FormSnapshot) -> Vec<&FormControl> {
    let mut controls: Vec<&FormControl> = snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(snapshot.unowned_controls.iter())
        .collect();
    controls.sort_by_key(|control| control_number(&control.id));
    controls
}

fn control_number(id: &str) -> usize {
    id.rsplit('-')
        .next()
        .and_then(|number| number.parse().ok())
        .unwrap_or(usize::MAX)
}

fn is_text_entry(control: &FormControl) -> bool {
    matches!(
        control.control_kind,
        FormControlKind::Text | FormControlKind::Number
    ) && control
        .supported_operations
        .contains(&FormControlOperation::SetText)
}

fn field_of(control: &FormControl) -> CodeField {
    CodeField {
        id: control.id.clone(),
        target: control.target.clone(),
        max_length: control.constraints.max_length,
    }
}

/// Runs of 4 to 8 adjacent one-character text controls in one form.
fn box_runs<'a>(controls: &[&'a FormControl]) -> Vec<Vec<&'a FormControl>> {
    let sized = |run: &Vec<&FormControl>| (MIN_BOXES..=MAX_BOXES).contains(&run.len());
    let mut runs = Vec::new();
    let mut run: Vec<&FormControl> = Vec::new();
    for control in controls {
        let is_box = is_text_entry(control) && control.constraints.max_length == Some(1);
        let continues = run
            .last()
            .is_some_and(|previous| previous.form_id == control.form_id);
        if is_box && (run.is_empty() || continues) {
            run.push(control);
            continue;
        }
        if sized(&run) {
            runs.push(std::mem::take(&mut run));
        }
        run.clear();
        if is_box {
            run.push(control);
        }
    }
    if sized(&run) {
        runs.push(run);
    }
    runs
}

fn detected_fields(controls: &[&FormControl], page_text: &str) -> Option<Vec<CodeField>> {
    let singles: Vec<(&FormControl, Strength)> = controls
        .iter()
        .filter(|control| is_text_entry(control) && control.constraints.max_length != Some(1))
        .filter_map(|control| Some((*control, single_strength(control)?)))
        .collect();
    if let Some((control, _)) = singles
        .iter()
        .find(|(control, _)| has_one_time_code_autocomplete(control))
    {
        return Some(vec![field_of(control)]);
    }
    if let Some(run) = box_runs(controls).into_iter().next() {
        return Some(run.into_iter().map(field_of).collect());
    }
    let supported = supports_code_entry(page_text);
    singles
        .iter()
        .find(|(_, strength)| *strength == Strength::Strong)
        .or_else(|| {
            singles
                .iter()
                .find(|(_, strength)| supported && *strength == Strength::Weak)
        })
        .map(|(control, _)| vec![field_of(control)])
}

fn hinted_fields(controls: &[&FormControl], hints: &IntentHints) -> Option<Vec<CodeField>> {
    let wanted = hints.accessible_name.as_deref().or(match &hints.near_text {
        Some(types::TextMatch::Exact(name)) => Some(name.as_str()),
        _ => None,
    });
    let ordinal = hints.ordinal.unwrap_or(0);
    let hinted = controls
        .iter()
        .filter(|control| is_text_entry(control))
        .filter(|control| {
            let Some(target) = &control.target else {
                return false;
            };
            hints
                .role
                .as_deref()
                .is_none_or(|role| target.role.eq_ignore_ascii_case(role))
                && wanted.is_none_or(|name| target.accessible_name == name)
        })
        .nth(ordinal)
        .copied()?;
    if let Some(run) = box_runs(controls)
        .into_iter()
        .find(|run| run.iter().any(|control| control.id == hinted.id))
    {
        return Some(run.into_iter().map(field_of).collect());
    }
    Some(vec![field_of(hinted)])
}

fn has_one_time_code_autocomplete(control: &FormControl) -> bool {
    control
        .autocomplete
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("one-time-code"))
}

/// Words that mark a field a person types a received code into.
const STRONG_PHRASES: &[&[&str]] = &[
    &["one", "time"],
    &["otp"],
    &["passcode"],
    &["pass", "code"],
    &["verification"],
    &["verify", "code"],
    &["security", "code"],
    &["confirmation", "code"],
    &["authentication", "code"],
    &["authenticator"],
    &["auth", "code"],
    &["sms", "code"],
    &["login", "code"],
    &["signin", "code"],
    &["sign", "in", "code"],
    &["email", "code"],
    &["text", "code"],
    &["6", "digit"],
    &["six", "digit"],
];

/// Kinds of "code" that are not a received one-time code.
const OTHER_CODES: &[&str] = &[
    "promo",
    "promotional",
    "coupon",
    "discount",
    "zip",
    "postal",
    "post",
    "country",
    "area",
    "gift",
    "voucher",
    "referral",
    "invite",
    "invitation",
    "redeem",
    "dial",
    "calling",
    "swift",
    "routing",
    "sort",
    "currency",
    "language",
];

fn control_words(control: &FormControl) -> Vec<String> {
    [
        control.accessible_name.as_deref(),
        control.label.as_deref(),
        control.placeholder.as_deref(),
        control.description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(words)
    .collect()
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn contains_phrase(words: &[String], phrase: &[&str]) -> bool {
    words.windows(phrase.len()).any(|window| {
        window
            .iter()
            .zip(phrase)
            .all(|(word, expected)| word == expected)
    })
}

fn single_strength(control: &FormControl) -> Option<Strength> {
    if has_one_time_code_autocomplete(control) {
        return Some(Strength::Strong);
    }
    let words = control_words(control);
    if STRONG_PHRASES
        .iter()
        .any(|phrase| contains_phrase(&words, phrase))
    {
        return Some(Strength::Strong);
    }
    let is_code = words.iter().any(|word| word == "code");
    let other = words
        .iter()
        .any(|word| OTHER_CODES.contains(&word.as_str()));
    (is_code && !other).then_some(Strength::Weak)
}

const SUPPORT_PHRASES: &[&str] = &[
    "we sent",
    "we've sent",
    "we\u{2019}ve sent",
    "we have sent",
    "has been sent",
    "was sent",
    "sent you",
    "enter the code",
    "enter your code",
    "enter code",
    "enter the verification",
    "enter the security",
    "enter the 6",
    "enter the 4",
    "enter the 8",
    "check your email",
    "check your inbox",
    "check your phone",
    "check your messages",
    "check your text",
    "one-time",
    "one time code",
    "verification code",
    "security code",
    "passcode",
    "digit code",
];

fn supports_code_entry(page_text: &str) -> bool {
    let text = page_text.to_lowercase();
    SUPPORT_PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// The length the page states for a single code field.
fn single_code_length(control: &FormControl, page_text: &str) -> Option<u32> {
    let plausible = |length: &u32| (MIN_BOXES as u32..=12).contains(length);
    control
        .constraints
        .max_length
        .filter(plausible)
        .or_else(|| {
            control
                .constraints
                .pattern
                .as_deref()
                .and_then(pattern_length)
                .filter(plausible)
        })
        .or_else(|| {
            digit_count(&control_words(control))
                .or_else(|| digit_count(&words(page_text)))
                .filter(plausible)
        })
}

/// `{6}` in a `pattern` such as `\d{6}` or `[0-9]{6}`.
fn pattern_length(pattern: &str) -> Option<u32> {
    let start = pattern.find('{')? + 1;
    let end = pattern[start..].find('}')? + start;
    pattern[start..end].parse().ok()
}

/// The number before "digit", as in "6-digit code".
fn digit_count(words: &[String]) -> Option<u32> {
    words.windows(2).find_map(|pair| {
        matches!(pair[1].as_str(), "digit" | "digits")
            .then(|| pair[0].parse().ok())
            .flatten()
    })
}

fn submit_target(
    snapshot: &FormSnapshot,
    controls: &[&FormControl],
    field: &FormControl,
) -> Option<FormControlTarget> {
    let activates = |control: &FormControl| {
        control
            .supported_operations
            .contains(&FormControlOperation::Activate)
            && control.target.is_some()
    };
    if let Some(form) = field
        .form_id
        .as_ref()
        .and_then(|id| snapshot.forms.iter().find(|form| &form.id == id))
    {
        return form
            .submit_control_ids
            .iter()
            .filter_map(|id| form.controls.iter().find(|control| &control.id == id))
            .find(|control| activates(control))
            .and_then(|control| control.target.clone());
    }
    // Outside a form, only a control whose own name says it submits the step.
    controls
        .iter()
        .filter(|control| control.form_id.is_none() && activates(control))
        .find(|control| {
            let name = control
                .accessible_name
                .as_deref()
                .map(words)
                .unwrap_or_default();
            let refuses = name
                .iter()
                .any(|word| matches!(word.as_str(), "resend" | "cancel" | "back" | "again"));
            !refuses
                && name.iter().any(|word| {
                    matches!(
                        word.as_str(),
                        "verify" | "submit" | "continue" | "confirm" | "next" | "done" | "login"
                    )
                })
        })
        .and_then(|control| control.target.clone())
}

/// The page's own sentence about where the code went, as shown. Engines
/// differ on whether block text keeps its line breaks, so a line is cut down
/// to the sentence around "sent" when it holds more than that.
fn destination_hint(page_text: &str) -> Option<String> {
    const SENT_WORDS: &[&str] = &["sent", "sending", "emailed", "texted", "check your"];
    let lines: Vec<&str> = page_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let says_sent = |line: &&str| {
        let lower = line.to_lowercase();
        SENT_WORDS.iter().any(|word| lower.contains(word))
    };
    let index = lines
        .iter()
        .position(|line| says_sent(line) && masks_a_destination(&sent_sentence(line)))
        .or_else(|| lines.iter().position(says_sent))?;
    let mut hint = sent_sentence(lines[index]);
    if !masks_a_destination(&hint) {
        if let Some(next) = lines
            .get(index + 1)
            .filter(|line| masks_a_destination(line))
        {
            hint.push(' ');
            hint.push_str(next);
        }
    }
    Some(hint.chars().take(MAX_HINT_CHARS).collect())
}

/// From the subject before "sent" through the masked destination, when the
/// line says "sent"; the line itself otherwise.
fn sent_sentence(line: &str) -> String {
    const LEADING: &[&str] = &[
        "we",
        "we've",
        "we\u{2019}ve",
        "have",
        "has",
        "been",
        "was",
        "code",
        "a",
        "an",
        "the",
        "just",
        "verification",
        "security",
        "your",
    ];
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(at) = tokens.iter().position(|token| {
        matches!(
            token
                .trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_lowercase()
                .as_str(),
            "sent" | "sending" | "emailed" | "texted"
        )
    }) else {
        return line.to_owned();
    };
    let mut start = at;
    while start > 0 && LEADING.contains(&tokens[start - 1].to_lowercase().as_str()) {
        start -= 1;
    }
    let mut end = at;
    for (offset, token) in tokens[at..].iter().enumerate().take(12) {
        end = at + offset;
        if masks_a_destination(token) || token.ends_with('.') {
            break;
        }
    }
    tokens[start..=end].join(" ")
}

fn masks_a_destination(line: &str) -> bool {
    line.contains('@')
        || line.contains('*')
        || line.contains('\u{2022}')
        || line.contains('\u{25cf}')
        || line.chars().filter(char::is_ascii_digit).count() >= 2
}

/// Whether `control` shows exactly `expected`, ignoring the spaces and dashes
/// a page formats a code with. `None` when the page gives no readable value.
pub(crate) fn holds(control: &FormControl, expected: &str) -> Option<bool> {
    match &control.state {
        FormControlState::Text { value } => Some(
            value
                .chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '-')
                .eq(expected.chars()),
        ),
        FormControlState::Empty => Some(expected.is_empty()),
        _ => None,
    }
}

/// Every control of `snapshot`, for callers that look a field up by id.
pub(crate) fn find_control<'a>(snapshot: &'a FormSnapshot, id: &str) -> Option<&'a FormControl> {
    snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(snapshot.unowned_controls.iter())
        .find(|control| control.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn control(id: u32, name: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut base = json!({
            "id": format!("control-{id}"),
            "target": {"role": "textbox", "accessibleName": name},
            "controlKind": "text",
            "accessibleName": name,
            "state": {"kind": "empty"},
            "validity": {"willValidate": true, "valid": true},
            "supportedOperations": ["setText", "clear"]
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    fn snapshot(controls: Vec<serde_json::Value>) -> FormSnapshot {
        serde_json::from_value(json!({
            "schemaVersion": 1,
            "pageId": "018f0000-0000-7000-8000-000000000001",
            "forms": [],
            "unownedControls": controls,
            "truncated": false,
            "pageDerived": true
        }))
        .expect("snapshot fixture")
    }

    fn boxes(count: u32) -> Vec<serde_json::Value> {
        (1..=count)
            .map(|n| {
                control(
                    n,
                    &format!("Digit {n}"),
                    json!({"constraints": {"maxLength": 1}}),
                )
            })
            .collect()
    }

    #[test]
    fn a_promo_code_field_is_not_a_code_step_even_beside_code_words() {
        let page = snapshot(vec![control(1, "Promo code", json!({}))]);
        assert!(detect(&page, "Enter the code at checkout", None).is_none());
    }

    #[test]
    fn a_plain_code_field_needs_the_page_to_say_a_code_was_sent() {
        let page = snapshot(vec![control(1, "Code", json!({}))]);
        assert!(detect(&page, "Welcome", None).is_none());
        let step = detect(&page, "We sent a code to your email", None).expect("code step");
        assert_eq!(step.fields.len(), 1);
    }

    #[test]
    fn four_to_eight_adjacent_single_character_boxes_are_one_split_code() {
        let step = detect(&snapshot(boxes(6)), "", None).expect("split code");
        assert_eq!(step.fields.len(), 6);
        assert_eq!(step.code_length, Some(6));
        assert!(detect(&snapshot(boxes(3)), "", None).is_none());
        assert!(detect(&snapshot(boxes(9)), "", None).is_none());
    }

    #[test]
    fn the_destination_hint_survives_text_without_line_breaks() {
        let page = snapshot(vec![control(
            1,
            "Verification code",
            json!({"constraints": {"maxLength": 6}}),
        )]);
        let step = detect(
            &page,
            "Check your email We sent a code to a***@example.com Verification code Verify",
            None,
        )
        .expect("code step");
        assert_eq!(
            step.destination_hint.as_deref(),
            Some("We sent a code to a***@example.com")
        );
    }

    #[test]
    fn the_destination_hint_is_the_pages_own_line_with_its_masked_address() {
        let page = snapshot(vec![control(
            1,
            "Verification code",
            json!({"constraints": {"maxLength": 6}}),
        )]);
        let step = detect(
            &page,
            "Check your email\nWe sent a code to a***@example.com\nResend",
            None,
        )
        .expect("code step");
        assert_eq!(
            step.destination_hint.as_deref(),
            Some("We sent a code to a***@example.com")
        );
        assert_eq!(step.code_length, Some(6));
    }
}
