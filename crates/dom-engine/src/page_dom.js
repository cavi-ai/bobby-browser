// Shared page DOM for the Firefox content script and both engine adapters.
// Open-shadow hops, ancestor hiding, and rendered-leaf visibility live here.

export const SHADOW_HOP = " >>> ";

// The element a selector names, entering each host's open shadow root in turn.
// Throws, as querySelector does, on an invalid part. `root` is the document
// the hop starts from; engine adapters pass the page document.
function probeDocument(root) {
  return typeof root.createDocumentFragment === "function" ? root : document;
}

export function composedQuery(root, selector) {
  const parts = selector.split(SHADOW_HOP);
  const probe = probeDocument(root).createDocumentFragment();
  for (const part of parts) probe.querySelector(part);
  let current = root;
  for (let index = 0; index < parts.length; index += 1) {
    const found = current.querySelector(parts[index]);
    if (!found || index === parts.length - 1) return found;
    current = found.shadowRoot;
    if (!current) return null;
  }
  return null;
}

// Every element the last part matches, after the same open-shadow hops.
export function composedQueryAll(root, selector) {
  const parts = selector.split(SHADOW_HOP);
  const probe = probeDocument(root).createDocumentFragment();
  for (const part of parts) probe.querySelector(part);
  let current = root;
  for (let index = 0; index < parts.length - 1; index += 1) {
    const host = current.querySelector(parts[index]);
    current = host && host.shadowRoot;
    if (!current) return [];
  }
  return [...current.querySelectorAll(parts[parts.length - 1])];
}

// Rendered-leaf visibility. The body is `ELEMENT_VISIBILITY_SCRIPT`: geometry
// and CSS hiding of this element only, not ancestors or aria-hidden.
export function renderedLeafVisible(el) {
  const s=getComputedStyle(el),r=el.getBoundingClientRect(); return s.visibility!=='hidden'&&s.display!=='none'&&r.width>0&&r.height>0
}

const MAX_ANCESTOR_VISITS = 256;

function composedParent(element) {
  if (element.assignedSlot) return element.assignedSlot;
  if (element.parentElement) return element.parentElement;
  const parent = element.parentNode;
  return parent && parent.nodeType === 11 ? (parent.host ?? null) : null;
}

function takeAncestorWork(budget) {
  if (!budget) return true;
  if (budget.remaining <= 0) return false;
  budget.remaining -= 1;
  return true;
}

// Hidden from rendering or from assistive technology, walking composed ancestors.
export function isElementHidden(element, budget) {
  let visited = 0;
  for (let current = element; current; current = composedParent(current)) {
    visited += 1;
    if (visited > MAX_ANCESTOR_VISITS || !takeAncestorWork(budget)) return true;
    if (current.hasAttribute("hidden") || current.getAttribute("aria-hidden") === "true") {
      return true;
    }
    const style = current.ownerDocument.defaultView?.getComputedStyle(current);
    if (style?.display === "none" || style?.visibility === "hidden") return true;
  }
  return false;
}

// Hidden from rendering. aria-hidden alone does not count.
export function isRenderedHidden(element) {
  let visited = 0;
  for (let current = element; current; current = composedParent(current)) {
    visited += 1;
    if (visited > MAX_ANCESTOR_VISITS) return true;
    if (current.hasAttribute("hidden")) return true;
    const style = current.ownerDocument.defaultView?.getComputedStyle(current);
    if (style?.display === "none" || style?.visibility === "hidden") return true;
  }
  return false;
}

export function modalDialogOpen(document) {
  for (const dialog of Array.from(document.querySelectorAll('[aria-modal="true"], dialog')).slice(0, 64)) {
    let modal;
    try {
      modal =
        dialog.tagName === "DIALOG"
          ? dialog.matches(":modal")
          : ["dialog", "alertdialog"].includes(dialog.getAttribute("role") ?? "");
    } catch {
      modal = false;
    }
    if (modal && !isElementHidden(dialog)) return true;
  }
  return false;
}
