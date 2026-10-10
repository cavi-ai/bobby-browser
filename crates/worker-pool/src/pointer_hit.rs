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
