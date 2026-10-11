use types::ErrorCode;

pub const MODAL_OBSTRUCTION: &str = "target is behind an open modal dialog; dismiss it first";
pub const COVERED_CLICK: &str = "click target is covered by another element";
pub const CLICK_OUTSIDE_VIEWPORT: &str = "click target is outside the viewport";
pub const ACCESSIBILITY_MISS: &str = "resolved target is hidden from the accessibility tree";
pub const FIREFOX_POINTER_OBSCURED: &str =
    "Firefox native click target is obscured after viewport preflight";
pub const FIREFOX_POINTER_OUT_OF_BOUNDS: &str =
    "Firefox native click target has no clickable point in the viewport";

/// A target missing from the accessibility tree is obscured when a modal is open.
pub fn accessibility_miss(modal_open: bool) -> (ErrorCode, &'static str) {
    if modal_open {
        (ErrorCode::TargetObscured, MODAL_OBSTRUCTION)
    } else {
        (ErrorCode::TargetNotFound, ACCESSIBILITY_MISS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_modal_is_the_only_accessibility_obscure() {
        assert_eq!(
            accessibility_miss(true),
            (ErrorCode::TargetObscured, MODAL_OBSTRUCTION)
        );
        assert_eq!(
            accessibility_miss(false),
            (ErrorCode::TargetNotFound, ACCESSIBILITY_MISS)
        );
    }
}
