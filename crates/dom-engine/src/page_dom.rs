use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// DOM observation body for an element bound to `el`.
/// Visibility requires rendered geometry and excludes CSS-hidden elements.
/// The same statement is the body of `renderedLeafVisible` in `page_dom.js`.
pub const ELEMENT_VISIBILITY_SCRIPT: &str = "const s=el.ownerDocument.defaultView.getComputedStyle(el),r=el.getBoundingClientRect(); return s.visibility!=='hidden'&&s.display!=='none'&&r.width>0&&r.height>0";

/// Body with `el` bound. Pin a visible dialog associated with the control
/// for a later disappearance check, retaining its identity across reads.
pub fn enclosing_dialog_script(composed: bool) -> String {
    static NEXT_PIN: AtomicU64 = AtomicU64::new(1);
    let pin = NEXT_PIN.fetch_add(1, Ordering::Relaxed);
    format!(
        r#"
const dialogSelector = 'dialog,[role="dialog" i],[role="alertdialog" i]';
const visible = el => {{ {ELEMENT_VISIBILITY_SCRIPT} }};
const isDialog = node => node?.matches(dialogSelector) && visible(node);
if (!el) throw new Error('dismissal control is detached');
let dialog = el;
while (dialog && !isDialog(dialog))
    dialog = dialog.assignedSlot || dialog.parentElement || dialog.getRootNode().host;
if (!dialog) {{
    const root = el.getRootNode();
    const ids = (el.getAttribute('aria-controls') || '').split(/\s+/);
    const related = new Set(ids.map(id => root.getElementById?.(id)).filter(isDialog));
    if (related.size > 1) return '';
    if (related.size === 0) return null;
    [dialog] = related;
}}
const key = Symbol.for('bobby.dismissalPins');
const pins = globalThis[key] ??= new WeakMap();
let index = 0;
const pin = node => {{
    let id = pins.get(node);
    if (!id) {{
        id = '{pin}-' + (++index);
        pins.set(node, id);
    }}
    const attribute = 'data-bobby-dismissal-anchor';
    if (node.getAttribute(attribute) !== id) node.setAttribute(attribute, id);
    return '[' + attribute + '="' + id + '"]';
}};
const path = [pin(dialog)];
for (let host = dialog.getRootNode().host; {composed} && host; host = host.getRootNode().host)
    path.unshift(pin(host));
return path.join(' >>> ');
"#
    )
}

/// An empty selector signals multiple visible dialogs. Keep this distinct
/// from a control that has no associated dialog.
pub fn decode_enclosing_dialog(
    selector: Option<String>,
) -> Result<Option<String>, types::CommandError> {
    if selector.as_deref() == Some("") {
        return Err(types::CommandError {
            code: types::ErrorCode::TargetAmbiguous,
            message: "the dismissal control is associated with more than one visible dialog".into(),
            layer: types::ErrorLayer::Page,
            retryable: false,
        });
    }
    Ok(selector)
}

fn page_bindings() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        include_str!("page_dom.js")
            .replace("export const ", "const ")
            .replace("export function ", "function ")
    });
    CELL.get().expect("page bindings").as_str()
}

fn cached(slot: &'static OnceLock<String>, call: &'static str) -> &'static str {
    slot.get_or_init(|| format!("((selector)=>{{{}\nreturn {call};}})", page_bindings()));
    slot.get().expect("page function").as_str()
}

/// Page function `(selector) => Element | null` using the document's open shadow roots.
pub fn composed_query_function() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    cached(&CELL, "composedQuery(document, selector)")
}

/// Page function `(selector) => Element[]` for the last hop of an open-shadow selector.
pub fn composed_query_all_function() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    cached(&CELL, "composedQueryAll(document, selector)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_leaf_script_is_the_page_module_body() {
        assert!(include_str!("page_dom.js").contains(ELEMENT_VISIBILITY_SCRIPT));
    }

    #[test]
    fn composed_query_expression_calls_the_shared_hop() {
        let query = composed_query_function();
        let all = composed_query_all_function();
        assert!(!query.contains("export "));
        assert!(!all.contains("export "));
        assert!(query.contains("return composedQuery(document, selector)"));
        assert!(all.contains("return composedQueryAll(document, selector)"));
        assert!(query.contains(" >>> "));
        assert!(query.starts_with("((selector)=>"));
    }
}
