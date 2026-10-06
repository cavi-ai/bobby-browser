//! Secret-material detection shared in behavior with
//! `packages/firefox-companion/src/secret-material.ts`; both are pinned to
//! `tests/fixtures/secret-material.json`.
//!
//! A word such as "password" or "token" is not a secret. Only text that
//! discloses a credential is: auth-scheme credentials, `key=value`
//! disclosures, PEM private keys, JWTs, prefixed tokens, AWS and Google keys,
//! and long mixed-case alphanumeric runs (skipped for http(s) URLs).

fn is_alnum(character: char) -> bool {
    character.is_ascii_alphanumeric()
}

fn is_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn is_auth_token_char(character: char) -> bool {
    is_alnum(character) || matches!(character, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

fn is_long_run_char(character: char) -> bool {
    is_alnum(character) || matches!(character, '+' | '/' | '_' | '=' | '-')
}

fn is_base64url_char(character: char) -> bool {
    is_alnum(character) || matches!(character, '_' | '-')
}

fn starts_with_at(chars: &[char], at: usize, needle: &str, ignore_case: bool) -> bool {
    let mut index = at;
    for expected in needle.chars() {
        let Some(&actual) = chars.get(index) else {
            return false;
        };
        let equal = if ignore_case {
            actual.eq_ignore_ascii_case(&expected)
        } else {
            actual == expected
        };
        if !equal {
            return false;
        }
        index += 1;
    }
    true
}

fn run_length(chars: &[char], from: usize, accepts: impl Fn(char) -> bool) -> usize {
    chars[from.min(chars.len())..]
        .iter()
        .take_while(|character| accepts(**character))
        .count()
}

fn is_credential_shaped(token: &[char]) -> bool {
    if token.iter().any(char::is_ascii_digit) || token.last() == Some(&'=') {
        return true;
    }
    token.iter().skip(1).any(char::is_ascii_uppercase) && token.iter().any(char::is_ascii_lowercase)
}

fn has_auth_scheme_credential(chars: &[char]) -> bool {
    for start in 0..chars.len() {
        if start != 0 && !chars[start - 1].is_whitespace() {
            continue;
        }
        let word_end = if starts_with_at(chars, start, "bearer", true) {
            start + 6
        } else if starts_with_at(chars, start, "basic", true) {
            start + 5
        } else {
            continue;
        };
        let gap = run_length(chars, word_end, char::is_whitespace);
        if gap == 0 {
            continue;
        }
        let token_start = word_end + gap;
        let length = run_length(chars, token_start, is_auth_token_char);
        if length >= 8 && is_credential_shaped(&chars[token_start..token_start + length]) {
            return true;
        }
    }
    false
}

/// End offsets of each keyword that can start at `at` in lowercased `chars`.
fn disclosure_keyword_end(chars: &[char], at: usize) -> Option<usize> {
    for keyword in ["password", "passwd", "secret", "token", "authorization"] {
        if starts_with_at(chars, at, keyword, false) {
            return Some(at + keyword.len());
        }
    }
    if starts_with_at(chars, at, "credential", false) {
        let end = at + "credential".len();
        return Some(if chars.get(end) == Some(&'s') {
            end + 1
        } else {
            end
        });
    }
    let separated = |prefix: &str, suffixes: &[&str]| -> Option<usize> {
        if !starts_with_at(chars, at, prefix, false) {
            return None;
        }
        let mut cursor = at + prefix.len();
        if matches!(chars.get(cursor), Some('-' | '_' | ' ')) {
            cursor += 1;
        }
        suffixes
            .iter()
            .find(|suffix| starts_with_at(chars, cursor, suffix, false))
            .map(|suffix| cursor + suffix.len())
    };
    separated("api", &["key"])
        .or_else(|| separated("private", &["key", "token", "secret"]))
        .or_else(|| separated("pairing", &["code"]))
}

fn has_key_value_disclosure(value: &str) -> bool {
    let chars: Vec<char> = value.chars().map(|c| c.to_ascii_lowercase()).collect();
    for start in 0..chars.len() {
        let Some(mut cursor) = disclosure_keyword_end(&chars, start) else {
            continue;
        };
        cursor += run_length(&chars, cursor, char::is_whitespace);
        if !matches!(chars.get(cursor), Some(':' | '=')) {
            continue;
        }
        cursor += 1;
        cursor += run_length(&chars, cursor, char::is_whitespace);
        if run_length(&chars, cursor, |c| !c.is_whitespace()) >= 4 {
            return true;
        }
    }
    false
}

fn has_pem_private_key(value: &str) -> bool {
    value
        .find("-----BEGIN")
        .is_some_and(|at| value[at + "-----BEGIN".len()..].contains("PRIVATE KEY-----"))
}

fn has_jwt(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        if !starts_with_at(chars, start, "eyJ", false) {
            return false;
        }
        let mut cursor = start + 3;
        let header = run_length(chars, cursor, is_base64url_char);
        if header < 7 {
            return false;
        }
        cursor += header;
        for minimum in [10, 10] {
            if chars.get(cursor) != Some(&'.') {
                return false;
            }
            cursor += 1;
            let length = run_length(chars, cursor, is_base64url_char);
            if length < minimum {
                return false;
            }
            cursor += length;
        }
        true
    })
}

fn has_prefixed_token(chars: &[char]) -> bool {
    const PREFIXES: [&str; 14] = [
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
    ];
    (0..chars.len()).any(|start| {
        if start > 0 && is_alnum(chars[start - 1]) {
            return false;
        }
        PREFIXES.iter().any(|prefix| {
            starts_with_at(chars, start, prefix, false)
                && run_length(chars, start + prefix.len(), is_base64url_char) >= 16
        })
    })
}

fn has_aws_access_key(chars: &[char]) -> bool {
    let is_upper_digit = |c: char| c.is_ascii_uppercase() || c.is_ascii_digit();
    (0..chars.len()).any(|start| {
        (start == 0 || !is_upper_digit(chars[start - 1]))
            && starts_with_at(chars, start, "AKIA", false)
            && chars.get(start + 4..start + 20).is_some_and(|tail| tail.iter().all(|c| is_upper_digit(*c)))
    })
}

fn has_google_api_key(chars: &[char]) -> bool {
    (0..chars.len()).any(|start| {
        starts_with_at(chars, start, "AIza", false)
            && chars
                .get(start + 4..start + 39)
                .is_some_and(|tail| tail.iter().all(|c| is_base64url_char(*c)))
    })
}

fn has_long_credential_run(chars: &[char]) -> bool {
    let mut start = 0;
    while start < chars.len() {
        let length = run_length(chars, start, is_long_run_char);
        if length == 0 {
            start += 1;
            continue;
        }
        let run = &chars[start..start + length];
        if length >= 40
            && run.iter().any(char::is_ascii_digit)
            && run.iter().any(char::is_ascii_uppercase)
            && run.iter().any(char::is_ascii_lowercase)
        {
            return true;
        }
        start += length;
    }
    false
}

fn is_http_url(value: &str) -> bool {
    let trimmed = value.trim().as_bytes();
    let has_prefix = |prefix: &str| {
        trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    };
    has_prefix("http://") || has_prefix("https://")
}

pub(crate) fn contains_secret_material(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    has_auth_scheme_credential(&chars)
        || has_key_value_disclosure(value)
        || has_pem_private_key(value)
        || has_jwt(&chars)
        || has_prefixed_token(&chars)
        || has_aws_access_key(&chars)
        || has_google_api_key(&chars)
        || (!is_http_url(value) && has_long_credential_run(&chars))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        benign: Vec<String>,
        secret: Vec<String>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../tests/fixtures/secret-material.json")).unwrap()
    }

    #[test]
    fn shared_fixture_benign_text_is_not_secret() {
        for value in fixture().benign {
            assert!(!contains_secret_material(&value), "{value}");
        }
    }

    #[test]
    fn shared_fixture_secret_text_is_secret() {
        for value in fixture().secret {
            assert!(contains_secret_material(&value), "{value}");
        }
    }
}
