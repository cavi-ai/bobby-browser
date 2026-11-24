//! Pointer hit classification shared by the Chromium and Firefox click adapters.
//!
//! Each adapter evals [`pointer_hit_function`] on the target element and maps
//! [`PointerHit`]. Scrolling the target into view stays in the adapter.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerHit {
    Reachable,
    Obscured,
    OutOfBounds,
    Detached,
}

/// JavaScript function `(element) -> 'ok' | 'obscured' | 'out-of-bounds' | 'detached'`.
pub fn pointer_hit_function() -> &'static str {
    "function(element){if(!(element instanceof Element)||!element.isConnected)return 'detached';const rect=element.getBoundingClientRect();const width=document.documentElement.clientWidth;const height=document.documentElement.clientHeight;if(rect.width<=0||rect.height<=0||rect.right<=0||rect.bottom<=0||rect.left>=width||rect.top>=height)return 'out-of-bounds';const x=Math.min(Math.max(rect.left+rect.width/2,0),width-1);const y=Math.min(Math.max(rect.top+rect.height/2,0),height-1);const root=element.getRootNode();const hit=typeof root.elementFromPoint==='function'?root.elementFromPoint(x,y):document.elementFromPoint(x,y);if(hit===null||(hit!==element&&!element.contains(hit)))return 'obscured';return 'ok';}"
}

/// The element a pointer at the target's center hits instead of the target,
/// or its enclosing dialog.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct CoveringElement {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Tag, id and up to two classes, as `div#id.one.two`.
    #[serde(default)]
    pub descriptor: Option<String>,
}

/// Describes the element covering a target's center, or a hit already
/// identified by CDP in an ancestor frame. An enclosing dialog takes priority.
pub fn covering_element_function() -> &'static str {
    r#"function(element, hit) {
      if (hit === undefined) {
        if (!(element instanceof Element) || !element.isConnected) return null;
        const rect = element.getBoundingClientRect();
        const width = document.documentElement.clientWidth;
        const height = document.documentElement.clientHeight;
        const x = Math.min(Math.max(rect.left + rect.width / 2, 0), width - 1);
        const y = Math.min(Math.max(rect.top + rect.height / 2, 0), height - 1);
        const root = element.getRootNode();
        hit = typeof root.elementFromPoint === 'function'
          ? root.elementFromPoint(x, y)
          : document.elementFromPoint(x, y);
        if (hit === null || hit === element || element.contains(hit)) return null;
      }
      if (hit?.nodeType !== Node.ELEMENT_NODE) hit = hit?.parentElement;
      if (!hit) return null;
      const cover = hit.closest('dialog,[role=dialog],[role=alertdialog],[aria-modal=true]') || hit;
      const clip = value => {
        const text = String(value || '').replace(/\s+/g, ' ').trim();
        return text ? text.slice(0, 60) : null;
      };
      const scope = cover.getRootNode();
      const labelled = (cover.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
        .map(id => scope.getElementById?.(id)?.textContent || '').join(' ');
      const heading = cover.querySelector('h1,h2,h3,h4,h5,h6,[role=heading]');
      const tag = cover.localName;
      const role = cover.getAttribute('role') || (tag === 'dialog' ? 'dialog' : null);
      const name = clip(cover.getAttribute('aria-label')) || clip(labelled)
        || clip(cover.getAttribute('title')) || clip(heading?.textContent) || clip(cover.innerText);
      const classes = [...cover.classList].slice(0, 2).map(name => '.' + name).join('');
      return {role: clip(role), name, descriptor: clip(tag + (cover.id ? '#' + cover.id : '') + classes)};
    }"#
}

pub fn parse_pointer_hit(value: &str) -> Option<PointerHit> {
    match value {
        "ok" => Some(PointerHit::Reachable),
        "obscured" => Some(PointerHit::Obscured),
        "out-of-bounds" => Some(PointerHit::OutOfBounds),
        "detached" => Some(PointerHit::Detached),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_four_verdicts() {
        assert_eq!(parse_pointer_hit("ok"), Some(PointerHit::Reachable));
        assert_eq!(parse_pointer_hit("obscured"), Some(PointerHit::Obscured));
        assert_eq!(
            parse_pointer_hit("out-of-bounds"),
            Some(PointerHit::OutOfBounds)
        );
        assert_eq!(parse_pointer_hit("detached"), Some(PointerHit::Detached));
        assert_eq!(parse_pointer_hit("element"), None);
    }
}
