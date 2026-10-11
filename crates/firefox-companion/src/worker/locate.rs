use super::TargetLocation;
use types::{CommandError, ErrorCode};

pub enum Answer<'a> {
    Obscured,
    Hit {
        found: bool,
        ambiguous: bool,
        css_path: Option<&'a str>,
    },
    Other,
}

pub enum Follow<'a> {
    Obscured,
    Ambiguous,
    Css(&'a str),
    Unresolved,
}

pub fn answer(result: &Result<TargetLocation, CommandError>) -> Answer<'_> {
    match result {
        Err(error) if error.code == ErrorCode::TargetObscured => Answer::Obscured,
        Ok(location) => Answer::Hit {
            found: location.found,
            ambiguous: location.ambiguous,
            css_path: location.css_path.as_deref(),
        },
        Err(_) => Answer::Other,
    }
}

/// What a NotFound candidate walk does with the companion search.
/// An obscured target surfaces even when the walk was not truncated.
pub fn follow_not_found(truncated: bool, located: Answer<'_>) -> Follow<'_> {
    match located {
        Answer::Obscured => Follow::Obscured,
        Answer::Hit {
            ambiguous: true, ..
        } if truncated => Follow::Ambiguous,
        Answer::Hit {
            found: true,
            css_path: Some(css),
            ..
        } if truncated => Follow::Css(css),
        _ => Follow::Unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(found: bool, ambiguous: bool, css: Option<&str>) -> Answer<'_> {
        Answer::Hit {
            found,
            ambiguous,
            css_path: css,
        }
    }

    #[test]
    fn obscured_surfaces_before_truncation_matters() {
        assert!(matches!(
            follow_not_found(false, Answer::Obscured),
            Follow::Obscured
        ));
        assert!(matches!(
            follow_not_found(true, Answer::Obscured),
            Follow::Obscured
        ));
    }

    #[test]
    fn a_truncated_walk_keeps_an_unambiguous_companion_path() {
        assert!(matches!(
            follow_not_found(true, hit(true, true, Some("#a"))),
            Follow::Ambiguous
        ));
        assert!(matches!(
            follow_not_found(true, hit(true, false, Some("#a"))),
            Follow::Css("#a")
        ));
        assert!(matches!(
            follow_not_found(true, hit(false, false, Some("#a"))),
            Follow::Unresolved
        ));
        assert!(matches!(
            follow_not_found(false, hit(true, false, Some("#a"))),
            Follow::Unresolved
        ));
        assert!(matches!(
            follow_not_found(true, Answer::Other),
            Follow::Unresolved
        ));
    }
}
