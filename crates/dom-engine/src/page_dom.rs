use std::sync::OnceLock;

/// DOM observation body for an element bound to `el`.
/// Visibility requires rendered geometry and excludes CSS-hidden elements.
/// The same statement is the body of `renderedLeafVisible` in `page_dom.js`.
pub const ELEMENT_VISIBILITY_SCRIPT: &str = "const s=getComputedStyle(el),r=el.getBoundingClientRect(); return s.visibility!=='hidden'&&s.display!=='none'&&r.width>0&&r.height>0";

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
