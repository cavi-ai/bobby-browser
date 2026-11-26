use types::ErrorCode;

pub const MODAL_OBSTRUCTION: &str = "target is behind an open modal dialog; dismiss it first";
pub const COVERED_CLICK: &str = "click target is covered by another element";
pub const CLICK_OUTSIDE_VIEWPORT: &str = "click target is outside the viewport";
pub const ACCESSIBILITY_MISS: &str = "resolved target is hidden from the accessibility tree";
pub const FIREFOX_POINTER_OBSCURED: &str =
    "Firefox native click target is obscured after viewport preflight";
pub const FIREFOX_POINTER_OUT_OF_BOUNDS: &str =
    "Firefox native click target has no clickable point in the viewport";

/// A frame from another origin that keeps the page's controls covered after
/// Escape.
pub const FRAME_OVERLAY_PERSISTED: &str =
    "a frame from another origin still covers the page's controls after Escape";

/// Function taking `focus`: whether a frame from another origin holds focus or
/// covers the center of a page control. With `focus`, an unfocused one is
/// focused so a key reaches it.
pub const FRAME_OVERLAY_SCRIPT: &str = "(focus)=>{const foreign=f=>{try{return f.contentDocument===null}catch(e){return true}};const covers=f=>Array.from(document.querySelectorAll('input,textarea,select,button')).some(c=>{const r=c.getBoundingClientRect();return r.width>0&&r.height>0&&document.elementFromPoint(r.left+r.width/2,r.top+r.height/2)===f});const overlay=Array.from(document.querySelectorAll('iframe')).find(f=>foreign(f)&&(document.activeElement===f||covers(f)));if(!overlay)return false;if(focus&&document.activeElement!==overlay)overlay.focus();return true}";

/// [`COVERED_CLICK`], naming the covering element as `role "name" (descriptor)`
/// when it is known. A part that discloses a credential is left out.
pub fn covered_click(cover: Option<&crate::CoveringElement>) -> String {
    let Some(cover) = cover else {
        return COVERED_CLICK.to_owned();
    };
    fn shown(part: &Option<String>) -> Option<&str> {
        part.as_deref().map(str::trim).filter(|text| {
            !text.is_empty() && !crate::secret_material::contains_secret_material(text)
        })
    }
    let parts: Vec<String> = [
        shown(&cover.role).map(str::to_owned),
        shown(&cover.name).map(|name| format!("\"{name}\"")),
        shown(&cover.descriptor).map(|descriptor| format!("({descriptor})")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        COVERED_CLICK.to_owned()
    } else {
        format!("{COVERED_CLICK}: {}", parts.join(" "))
    }
}

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

    #[test]
    fn a_covered_click_names_the_cover_without_secrets() {
        assert_eq!(covered_click(None), COVERED_CLICK);
        let dialog = crate::CoveringElement {
            role: Some("dialog".into()),
            name: Some("Enter the code".into()),
            descriptor: Some("div#code.modal".into()),
        };
        assert_eq!(
            covered_click(Some(&dialog)),
            format!("{COVERED_CLICK}: dialog \"Enter the code\" (div#code.modal)")
        );
        let secret = crate::CoveringElement {
            role: None,
            name: Some("token=AbCdEfGh12345678".into()),
            descriptor: Some("div.banner".into()),
        };
        assert_eq!(
            covered_click(Some(&secret)),
            format!("{COVERED_CLICK}: (div.banner)")
        );
        assert_eq!(
            covered_click(Some(&crate::CoveringElement::default())),
            COVERED_CLICK
        );
    }
}
