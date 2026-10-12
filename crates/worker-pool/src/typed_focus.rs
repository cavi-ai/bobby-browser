//! Page scripts that follow focus when a page swaps a field for another
//! editable control as it takes focus, such as a placeholder that loads an
//! editor.

/// Function body with `doc` and `el` bound; `el` is null once the page removed
/// the field. Resolves once focus has rested on one element for 50 ms (500 ms
/// at most), to whether focus is on another editable control than `el`.
pub const FOCUS_SETTLE_SCRIPT: &str = "const focused=()=>{let a=doc.activeElement;while(a&&a.shadowRoot&&a.shadowRoot.activeElement)a=a.shadowRoot.activeElement;return a};const editable=a=>a.isContentEditable||a.tagName==='INPUT'||a.tagName==='TEXTAREA';const swapped=a=>Boolean(a&&editable(a)&&(!el||!el.isConnected||(a!==el&&!el.contains(a))));return new Promise(resolve=>{const start=performance.now();let last=focused(),since=start;const poll=()=>{const now=focused(),at=performance.now();if(now!==last){last=now;since=at}if(at-since>=50||at-start>=500){resolve(swapped(now));return}setTimeout(poll,10)};setTimeout(poll,10)});";

/// Expression that resolves once the page has run its pending work, at most
/// 100 ms later.
pub const PAGE_IDLE_SCRIPT: &str =
    "new Promise(resolve=>requestIdleCallback(()=>resolve(true),{timeout:100}))";

/// Function body with `doc` bound: the value of the focused input or text
/// area, or the text of the focused editable region.
pub const FOCUSED_TEXT_SCRIPT: &str = "let a=doc.activeElement;while(a&&a.shadowRoot&&a.shadowRoot.activeElement)a=a.shadowRoot.activeElement;if(!a)return '';if('value'in a&&(a.tagName==='INPUT'||a.tagName==='TEXTAREA'))return String(a.value);return a.isContentEditable?String(a.innerText||a.textContent||''):'';";
