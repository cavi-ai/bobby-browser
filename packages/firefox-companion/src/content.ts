import { isExtensionSafeString, isExtensionSafeUrl } from "./native-transport.js";
import { containsSecretMaterial } from "./secret-material.js";
import {
  CONTENT_FAILURE_KEY,
  MAX_COMPANION_PAYLOAD_BYTES,
  type ContentFailureReason,
} from "./protocol.js";

// A content action failure whose reason crosses to the runtime; its message
// stays in the page.
export class ContentActionError extends Error {
  constructor(
    readonly reason: ContentFailureReason,
    message: string,
  ) {
    super(message);
  }
}

export const MAX_VISIBLE_TEXT_LENGTH = 64 * 1024;
export const MAX_CONTROL_COUNT = 512;
export const MAX_VISIBLE_TEXT_VISITED_NODES = 4_096;
export const MAX_CONTROL_VISITED_NODES = 4_096;
export const MAX_ELEMENT_TEXT_VISITED_NODES = 512;
export const MAX_CONTROL_HELPER_VISITS = 16_384;
export const MAX_CONTROL_FILTER_VISITS = 262_144;
const MAX_NAME_CONTENT_DEPTH = 24;
export const MAX_CSS_SIBLING_VISITS = 128;
export const MAX_CONTROL_FIELD_LENGTH = 256;
export const MAX_SELECTOR_LENGTH = 512;
export const MAX_OBSERVATION_BYTES = MAX_COMPANION_PAYLOAD_BYTES - 64 * 1024;
export const MAX_SANITIZED_HTML_LENGTH = 128 * 1024;
const MAX_URL_LENGTH = 2 * 1024;
const MAX_TITLE_LENGTH = 1024;
const MAX_ROLE_LENGTH = 64;
const REDACTED = "[redacted]";
const MAX_ANCESTOR_VISITS = 256;
const SAFE_BOOLEAN_HTML_ATTRIBUTES = new Set([
  "checked",
  "disabled",
  "hidden",
  "multiple",
  "open",
  "readonly",
  "required",
  "selected",
]);
const SAFE_INPUT_TYPES = new Set([
  "button",
  "checkbox",
  "color",
  "date",
  "datetime-local",
  "email",
  "file",
  "hidden",
  "image",
  "month",
  "number",
  "password",
  "radio",
  "range",
  "reset",
  "search",
  "submit",
  "tel",
  "text",
  "time",
  "url",
  "week",
]);
const SAFE_ROLES = new Set([
  "alert",
  "article",
  "banner",
  "button",
  "cell",
  "checkbox",
  "columnheader",
  "combobox",
  "complementary",
  "contentinfo",
  "dialog",
  "document",
  "form",
  "grid",
  "gridcell",
  "group",
  "heading",
  "link",
  "list",
  "listbox",
  "listitem",
  "main",
  "menu",
  "menuitem",
  "navigation",
  "option",
  "progressbar",
  "radio",
  "radiogroup",
  "region",
  "row",
  "rowgroup",
  "rowheader",
  "search",
  "slider",
  "spinbutton",
  "status",
  "switch",
  "tab",
  "table",
  "tablist",
  "tabpanel",
  "textbox",
  "toolbar",
  "tooltip",
  "tree",
  "treeitem",
]);
const SAFE_ARIA_BOOLEAN_ATTRIBUTES = new Set([
  "aria-busy",
  "aria-disabled",
  "aria-expanded",
  "aria-hidden",
  "aria-multiline",
  "aria-multiselectable",
  "aria-pressed",
  "aria-readonly",
  "aria-required",
  "aria-selected",
]);

type WorkBudget = { remaining: number };

export type PageObservation = {
  url: string;
  title: string;
  visibleText: string;
  controls: Array<{
    cssPath: string;
    testId?: string;
    role?: string;
    name?: string;
    label?: string;
    value?: string;
    attributes: Record<string, string>;
    disabled: boolean;
  }>;
  html?: string;
  /** Present and true when the control walk stopped at a bound. */
  controlsTruncated?: boolean;
};

const CONTROL_SELECTOR = [
  "a[href]",
  "button",
  "input",
  "select",
  "textarea",
  "iframe",
  "[role]",
  '[contenteditable="true"]',
].join(",");

const SENSITIVE_MARKER =
  /(?:authorization|auth(?!or(?!i[sz]))|bearer|token|secret|password|passwd|api[-_]?key|credential)/i;
const SECRET_VALUE = /(?:^|\s)(?:bearer|basic)\s+\S+/i;
const textEncoder = new TextEncoder();

function byteLength(value: string): number {
  return textEncoder.encode(value).byteLength;
}

function takeWork(budget: WorkBudget | undefined): boolean {
  if (!budget) return true;
  if (budget.remaining <= 0) return false;
  budget.remaining -= 1;
  return true;
}

function boundedUtf8(value: string, maximum: number): string {
  if (byteLength(value) <= maximum) return value;
  let low = 0;
  let high = Math.min(value.length, maximum);
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (byteLength(value.slice(0, middle)) <= maximum) low = middle;
    else high = middle - 1;
  }
  return value.slice(0, low);
}

function containsSensitiveMaterial(value: string): boolean {
  return SENSITIVE_MARKER.test(value) || SECRET_VALUE.test(value);
}

function observationString(
  value: string | null | undefined,
  maximum = MAX_CONTROL_FIELD_LENGTH,
): string | undefined {
  const normalized = value?.slice(0, maximum * 8).replace(/\s+/g, " ").trim();
  if (!normalized) return undefined;
  if (containsSecretMaterial(normalized) || !isExtensionSafeString(normalized)) {
    return byteLength(REDACTED) <= maximum ? REDACTED : undefined;
  }
  return boundedUtf8(normalized, maximum);
}

function observationUrl(value: string): string {
  try {
    const url = new URL(value.slice(0, MAX_URL_LENGTH * 8));
    url.username = "";
    url.password = "";
    url.search = "";
    url.hash = "";
    let path = url.pathname;
    try {
      path = decodeURIComponent(path);
    } catch {}
    if (containsSensitiveMaterial(path)) url.pathname = "/";
    return observationUrlString(url.href);
  } catch {
    return observationUrlString(value);
  }
}

// A URL-typed field meets the URL rules, not only the free-text ones.
function observationUrlString(value: string): string {
  const bounded = observationString(value, MAX_URL_LENGTH) ?? "";
  return bounded && !isExtensionSafeUrl(bounded) ? REDACTED : bounded;
}

// An element's children in the rendered tree: a host's open shadow root
// content, a slot's assigned nodes or else its fallback content.
function composedChildren(element: Element): Node[] {
  const shadow = element.shadowRoot;
  if (shadow) return Array.from(shadow.childNodes);
  if (element.tagName === "SLOT") {
    const assigned = (element as HTMLSlotElement).assignedNodes({ flatten: true });
    if (assigned.length) return assigned;
  }
  return Array.from(element.childNodes);
}

function composedElementChildren(element: Element): Element[] {
  return composedChildren(element).filter((node): node is Element => node.nodeType === 1);
}

// An element's parent in the rendered tree: its slot, its parent, or the host
// of the shadow root it sits at the top of.
function composedParent(element: Element): Element | null {
  if (element.assignedSlot) return element.assignedSlot;
  if (element.parentElement) return element.parentElement;
  const parent = element.parentNode;
  return parent && parent.nodeType === 11 ? ((parent as ShadowRoot).host ?? null) : null;
}

// Joins the selectors of each shadow host and the element, each in its own
// root, into one selector the action side resolves through open shadow roots.
const SHADOW_HOP = " >>> ";

// The element a selector names, entering each host's open shadow root in turn.
// Throws, as querySelector does, on an invalid part.
function composedQuery(document: Document, selector: string): Element | null {
  const parts = selector.split(SHADOW_HOP);
  const probe = document.createDocumentFragment();
  for (const part of parts) probe.querySelector(part);
  let root: Document | ShadowRoot = document;
  for (let index = 0; index < parts.length; index += 1) {
    const found: Element | null = root.querySelector(parts[index]!);
    if (!found || index === parts.length - 1) return found;
    if (!found.shadowRoot) return null;
    root = found.shadowRoot;
  }
  return null;
}

function isElementHidden(element: Element, budget?: WorkBudget): boolean {
  let visited = 0;
  for (let current: Element | null = element; current; current = composedParent(current)) {
    visited += 1;
    if (visited > MAX_ANCESTOR_VISITS || !takeWork(budget)) return true;
    if (current.hasAttribute("hidden") || current.getAttribute("aria-hidden") === "true") {
      return true;
    }
    const style = current.ownerDocument.defaultView?.getComputedStyle(current);
    if (style?.display === "none" || style?.visibility === "hidden") {
      return true;
    }
  }
  return false;
}

// Hidden from rendering, unlike an element hidden only from assistive
// technology with aria-hidden.
function isRenderedHidden(element: Element): boolean {
  let visited = 0;
  for (let current: Element | null = element; current; current = composedParent(current)) {
    visited += 1;
    if (visited > MAX_ANCESTOR_VISITS) return true;
    if (current.hasAttribute("hidden")) return true;
    const style = current.ownerDocument.defaultView?.getComputedStyle(current);
    if (style?.display === "none" || style?.visibility === "hidden") return true;
  }
  return false;
}

function modalDialogOpen(document: Document): boolean {
  for (const dialog of Array.from(document.querySelectorAll('[aria-modal="true"], dialog')).slice(0, 64)) {
    let modal: boolean;
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

function isSensitiveTextContext(element: Element): boolean {
  const control = element.closest(CONTROL_SELECTOR);
  if (control && isSensitiveControl(control)) return true;
  const label = element.closest("label");
  // A label reading exactly a public control label shows, as the name does.
  if (!label || isPublicControlLabel(label.textContent)) return false;
  const labelled = label.getAttribute("for");
  if (labelled) {
    const target = label.ownerDocument.getElementById(labelled);
    if (target && isSensitiveControl(target)) return true;
  }
  const nested = label.querySelector("input,select,textarea");
  return nested ? isSensitiveControl(nested) : false;
}

function visibleText(document: Document, root: Element): string {
  let output = "";
  let outputBytes = 0;
  const walker = document.createTreeWalker(root, 4);
  let visited = 0;
  while (visited < MAX_VISIBLE_TEXT_VISITED_NODES) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    const parent = node.parentElement;
    if (
      !parent ||
      ["SCRIPT", "STYLE", "TEMPLATE", "NOSCRIPT"].includes(parent.tagName) ||
      isElementHidden(parent)
    ) {
      continue;
    }
    const separatorBytes = output ? 1 : 0;
    const remaining = MAX_VISIBLE_TEXT_LENGTH - outputBytes - separatorBytes;
    if (remaining <= 0) break;
    const text = isSensitiveTextContext(parent)
      ? byteLength(REDACTED) <= remaining
        ? REDACTED
        : undefined
      : observationString(node.nodeValue, remaining);
    if (text) {
      output += `${output ? " " : ""}${text}`;
      outputBytes += separatorBytes + byteLength(text);
    }
  }
  return output;
}

function cssString(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function cssIdentifier(value: string): string {
  return value.replace(/(^-?\d)|[^a-zA-Z0-9_-]/g, (character, startsWithDigit: string) =>
    startsWithDigit ? `\\3${character} ` : `\\${character}`,
  );
}

function safeStableAttribute(element: Element): { name: string; value: string } | undefined {
  for (const name of ["data-testid", "data-test", "data-qa", "name"] as const) {
    const value = element.getAttribute(name);
    if (
      value &&
      !SENSITIVE_MARKER.test(name) &&
      !containsSensitiveMaterial(value) &&
      byteLength(value) <= MAX_SELECTOR_LENGTH &&
      byteLength(`[${name}="${cssString(value)}"]`) <= MAX_SELECTOR_LENGTH
    ) {
      return { name, value };
    }
  }
  return undefined;
}

// The worker re-selects a resolved element with `document.querySelector`, which
// returns the first match in document order: a selector that also matches an
// earlier element (a hidden duplicate sharing an id, name or test id) hands
// the action that other element.
function selectsOnly(element: Element, selector: string): boolean {
  try {
    return (element.getRootNode() as Document | ShadowRoot).querySelector(selector) === element;
  } catch {
    return false;
  }
}

// Positional parts past which a path unanchored by an id is first tried as is.
const CSS_RELATIVE_PATH_PARTS = 8;

// A selector whose first match in the document is `element`, or undefined
// when no such selector fits the bound.
function cssPath(
  element: Element,
  budget: WorkBudget,
  siblingPositions: WeakMap<Element, number>,
  allowStableMetadata = true,
): string | undefined {
  const id = element.getAttribute("id");
  if (
    allowStableMetadata &&
    id &&
    byteLength(id) <= MAX_SELECTOR_LENGTH &&
    !containsSensitiveMaterial(id)
  ) {
    const selector = `#${cssIdentifier(id)}`;
    if (byteLength(selector) <= MAX_SELECTOR_LENGTH && selectsOnly(element, selector)) return selector;
  }
  const stable = allowStableMetadata ? safeStableAttribute(element) : undefined;
  if (stable) {
    const selector = `[${stable.name}="${cssString(stable.value)}"]`;
    if (selectsOnly(element, selector)) return selector;
  }

  const parts: string[] = [];
  for (let current: Element | null = element; current; current = current.parentElement) {
    if (!takeWork(budget)) return undefined;
    const tag = current.tagName.toLowerCase();
    const parent: HTMLElement | null = current.parentElement;
    if (!parent) {
      parts.unshift(tag);
      break;
    }
    let position = siblingPositions.get(current);
    if (position === undefined && current !== element.ownerDocument.body) {
      let peerCount = 0;
      const siblingLimit = Math.min(parent.children.length, MAX_CSS_SIBLING_VISITS);
      for (let childIndex = 0; childIndex < siblingLimit; childIndex += 1) {
        if (!takeWork(budget)) return undefined;
        const child: Element | null = parent.children.item(childIndex);
        if (!child) continue;
        if (child.tagName !== current.tagName) continue;
        peerCount += 1;
        if (child === current) position = peerCount;
      }
    }
    if (position === undefined) parts.unshift(tag);
    else parts.unshift(`${tag}:nth-of-type(${position})`);
    const parentId = parent.getAttribute("id");
    if (
      allowStableMetadata &&
      parentId &&
      byteLength(parentId) <= MAX_SELECTOR_LENGTH &&
      !containsSensitiveMaterial(parentId)
    ) {
      const candidate = [`#${cssIdentifier(parentId)}`, ...parts].join(" > ");
      if (byteLength(candidate) <= MAX_SELECTOR_LENGTH && selectsOnly(element, candidate)) {
        return candidate;
      }
    }
    const relative = parts.join(" > ");
    if (byteLength(relative) > MAX_SELECTOR_LENGTH) return undefined;
    if (parts.length === CSS_RELATIVE_PATH_PARTS && selectsOnly(element, relative)) return relative;
  }
  const path = parts.join(" > ");
  return byteLength(path) <= MAX_SELECTOR_LENGTH && selectsOnly(element, path) ? path : undefined;
}

// A selector that reaches `element` through each open shadow root around it.
function composedCssPath(
  element: Element,
  budget: WorkBudget,
  siblingPositions: WeakMap<Element, number>,
  allowStableMetadata = true,
): string | undefined {
  const parts: string[] = [];
  for (let current: Element | undefined = element; current; ) {
    const path = cssPath(current, budget, siblingPositions, allowStableMetadata);
    if (!path) return undefined;
    parts.unshift(path);
    const root = current.getRootNode();
    current = root.nodeType === 11 ? (root as ShadowRoot).host : undefined;
  }
  const joined = parts.join(SHADOW_HOP);
  return byteLength(joined) <= MAX_SELECTOR_LENGTH ? joined : undefined;
}

function implicitRole(element: Element, allowExplicit = true): string | undefined {
  const explicit = allowExplicit
    ? observationString(element.getAttribute("role"), MAX_ROLE_LENGTH)
    : undefined;
  if (explicit) return explicit;
  const tag = element.tagName.toLowerCase();
  if (tag === "button") return "button";
  if (tag === "a" && element.hasAttribute("href")) return "link";
  if (tag === "select") return element.hasAttribute("multiple") ? "listbox" : "combobox";
  if (tag === "textarea") return "textbox";
  if (tag === "iframe") return "iframe";
  if (tag === "input") {
    const type = (element.getAttribute("type") ?? "text").toLowerCase();
    if (["button", "submit", "reset", "image", "file"].includes(type)) return "button";
    if (type === "search") return "searchbox";
    if (type === "checkbox") return "checkbox";
    if (type === "radio") return "radio";
    if (type === "range") return "slider";
    if (type === "number") return "spinbutton";
    if (type !== "hidden") return "textbox";
  }
  return element.getAttribute("contenteditable") === "true" ? "textbox" : undefined;
}

function labelledByText(element: Element, budget: WorkBudget): string | undefined {
  const reference = element.getAttribute("aria-labelledby")?.slice(0, MAX_CONTROL_FIELD_LENGTH * 8);
  if (!reference) return undefined;
  if (containsSensitiveMaterial(reference)) return REDACTED;
  let output = "";
  let outputBytes = 0;
  for (const id of reference.trim().split(/\s+/, 16)) {
    if (!takeWork(budget)) break;
    const separatorBytes = output ? 1 : 0;
    const remaining = MAX_CONTROL_FIELD_LENGTH - outputBytes - separatorBytes;
    if (remaining <= 0) break;
    const referenced = (element.getRootNode() as Document | ShadowRoot).getElementById(id);
    const text = referenced ? boundedElementText(referenced, remaining, budget) : undefined;
    if (!text) continue;
    output += `${output ? " " : ""}${text}`;
    outputBytes += separatorBytes + byteLength(text);
  }
  return observationString(output);
}

function labelText(
  element: Element,
  labelsByControlId: ReadonlyMap<string, Element>,
  budget: WorkBudget,
): string | undefined {
  const id = element.getAttribute("id");
  if (id && byteLength(id) <= MAX_SELECTOR_LENGTH && takeWork(budget)) {
    const label = labelsByControlId.get(id);
    const text = label ? boundedElementText(label, MAX_CONTROL_FIELD_LENGTH, budget) : undefined;
    if (text) return text;
  }
  for (let current: Element | null = element.parentElement; current; current = current.parentElement) {
    if (!takeWork(budget)) return undefined;
    if (current.tagName === "LABEL") {
      return boundedElementText(current, MAX_CONTROL_FIELD_LENGTH, budget);
    }
  }
  return undefined;
}

function boundedElementText(
  element: Element,
  maximum = MAX_CONTROL_FIELD_LENGTH,
  budget?: WorkBudget,
): string | undefined {
  let output = "";
  let outputBytes = 0;
  let visited = 0;
  const push = (text: string | undefined): boolean => {
    if (!text) return true;
    const separatorBytes = output ? 1 : 0;
    const remaining = maximum - outputBytes - separatorBytes;
    if (remaining <= 0) return false;
    const bounded = observationString(text, remaining);
    if (!bounded) return true;
    output += `${output ? " " : ""}${bounded}`;
    outputBytes += separatorBytes + byteLength(bounded);
    return true;
  };
  const visit = (node: Node, depth: number): boolean => {
    if (visited >= MAX_ELEMENT_TEXT_VISITED_NODES || !takeWork(budget)) return false;
    visited += 1;
    if (node.nodeType === 3) return push(node.nodeValue ?? undefined);
    if (node.nodeType !== 1) return true;
    const child = node as Element;
    if (child !== element) {
      if (["SCRIPT", "STYLE", "TEMPLATE", "NOSCRIPT"].includes(child.tagName)) return true;
      if (isLocallyHidden(child)) return true;
      // An icon's own label stands for its content, as in the accessible
      // name computation: aria-label, an image's alt, an svg's <title>.
      const own = observationString(child.getAttribute("aria-label"));
      if (own) return push(own);
      if (child.tagName === "IMG") return push(child.getAttribute("alt") ?? undefined);
      if (child.tagName.toLowerCase() === "svg") {
        const title = Array.from(child.children).find((entry) => entry.tagName.toLowerCase() === "title");
        const text = title?.textContent ?? child.getAttribute("title") ?? undefined;
        if (text) return push(text);
      }
    }
    if (depth >= MAX_NAME_CONTENT_DEPTH) return true;
    for (const next of composedChildren(child).slice(0, 256)) {
      if (!visit(next, depth + 1)) return false;
    }
    return true;
  };
  try {
    if (!isElementHidden(element)) visit(element, 0);
  } catch {
    // A node that throws during inspection contributes no name.
  }
  return output || undefined;
}

function isLocallyHidden(element: Element): boolean {
  if (element.hasAttribute("hidden") || element.getAttribute("aria-hidden") === "true") return true;
  const style = element.ownerDocument.defaultView?.getComputedStyle(element);
  return style?.display === "none" || style?.visibility === "hidden";
}

const NAME_FROM_CONTENT_EXCLUDED_TAGS = new Set(["TEXTAREA", "SELECT"]);

// Roles ARIA names from content, plus `listitem` on purpose: bounded, and how
// feed posts and message previews are read. Landmarks, lists, tables, dialogs
// and groups take only aria-label / aria-labelledby.
const NAME_FROM_CONTENT_ROLES = new Set([
  "button",
  "link",
  "heading",
  "cell",
  "gridcell",
  "columnheader",
  "rowheader",
  "row",
  "checkbox",
  "radio",
  "switch",
  "menuitem",
  "menuitemcheckbox",
  "menuitemradio",
  "option",
  "tab",
  "treeitem",
  "tooltip",
  "listitem",
]);

function accessibleName(
  element: Element,
  label: string | undefined,
  budget: WorkBudget,
  sensitive: boolean,
  role: string | undefined,
): string | undefined {
  const nameFromContent = role !== undefined && NAME_FROM_CONTENT_ROLES.has(role);
  const textEntry = element.tagName === "INPUT" || element.tagName === "TEXTAREA";
  return (
    labelledByText(element, budget) ??
    observationString(element.getAttribute("aria-label")) ??
    label ??
    observationString(element.getAttribute("alt")) ??
    observationString(element.getAttribute("title")) ??
    (textEntry && !sensitive ? observationString(element.getAttribute("placeholder")) : undefined) ??
    (!nameFromContent || NAME_FROM_CONTENT_EXCLUDED_TAGS.has(element.tagName)
      ? undefined
      : boundedElementText(element, MAX_CONTROL_FIELD_LENGTH, budget)) ??
    (element.tagName === "INPUT" && !sensitive
      ? observationString(element.getAttribute("value"))
      : undefined)
  );
}

const PUBLIC_CONTROL_LABELS = ["Password", "Authentication code"];

function publicControlLabel(element: Element): string | undefined {
  if (!["INPUT", "TEXTAREA", "SELECT"].includes(element.tagName)) return undefined;
  const input = element as HTMLInputElement;
  const names = [element.getAttribute("aria-label"), ...Array.from(input.labels ?? []).map((label) => label.textContent)];
  // Emit only these exact fixed labels; never forward arbitrary text from a
  // sensitive control's metadata, which can include credentials.
  return PUBLIC_CONTROL_LABELS.find((safeName) => names.some((name) => name?.trim() === safeName));
}

function isPublicControlLabel(text: string | null): boolean {
  return PUBLIC_CONTROL_LABELS.includes(text?.trim() ?? "");
}

function isSensitiveControl(element: Element, budget?: WorkBudget): boolean {
  if (
    element.tagName === "INPUT" &&
    (element.getAttribute("type") ?? "text").toLowerCase() === "password"
  ) {
    return true;
  }
  if (element.attributes.length > 128) return true;
  for (const attribute of element.attributes) {
    if (!takeWork(budget)) return true;
    // A frame's permission policy names features such as
    // `identity-credentials-get`; it is never reported and holds no secret.
    if (element.tagName === "IFRAME" && attribute.name === "allow") continue;
    let value = attribute.value.slice(0, MAX_CONTROL_FIELD_LENGTH * 8);
    // The `webauthn` autofill token offers passkeys on an identifier field.
    if (attribute.name === "autocomplete") {
      value = value.split(/\s+/).filter((token) => token.toLowerCase() !== "webauthn").join(" ");
    }
    if (
      SENSITIVE_MARKER.test(attribute.name) ||
      SENSITIVE_MARKER.test(value) ||
      SECRET_VALUE.test(value)
    ) {
      return true;
    }
  }
  return false;
}

function controlValue(element: Element, sensitive = isSensitiveControl(element)): string | undefined {
  if (!["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)) return undefined;
  // An empty sensitive field holds nothing to withhold; it reads empty.
  if (sensitive) {
    return (element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement).value === ""
      ? undefined
      : REDACTED;
  }
  // File inputs expose a browser-supplied local path through `value`; only
  // the selected file names are reported.
  if (element.tagName === "INPUT" && (element as HTMLInputElement).type === "file") {
    const names = Array.from((element as HTMLInputElement).files ?? [], (file) => file.name);
    return observationString(names.join(", "));
  }
  const value = (element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement).value;
  return observationString(value);
}

function safeStructuralAttribute(name: string, value: string): string | undefined {
  const normalizedName = name.toLowerCase();
  const normalizedValue = value.trim().toLowerCase();
  if (SAFE_BOOLEAN_HTML_ATTRIBUTES.has(normalizedName)) return "";
  if (normalizedName === "type" && SAFE_INPUT_TYPES.has(normalizedValue)) return normalizedValue;
  if (normalizedName === "role" && SAFE_ROLES.has(normalizedValue)) return normalizedValue;
  if (
    SAFE_ARIA_BOOLEAN_ATTRIBUTES.has(normalizedName) &&
    ["true", "false", "mixed"].includes(normalizedValue)
  ) {
    return normalizedValue;
  }
  if (normalizedName === "aria-checked" && ["true", "false", "mixed"].includes(normalizedValue)) {
    return normalizedValue;
  }
  if (normalizedName === "scope" && ["row", "col", "rowgroup", "colgroup"].includes(normalizedValue)) {
    return normalizedValue;
  }
  if (["colspan", "rowspan"].includes(normalizedName) && /^\d{1,3}$/.test(normalizedValue)) {
    const span = Number(normalizedValue);
    if (span >= 1 && span <= 100) return String(span);
  }
  return undefined;
}

function sanitizedHtml(root: Element): string {
  const clone = root.cloneNode(true) as Element;
  const pendingNodes = [...clone.childNodes];
  let visitedNodes = 0;
  let exceededNodeBudget = false;
  while (pendingNodes.length > 0) {
    const node = pendingNodes.pop() as Node;
    visitedNodes += 1;
    if (visitedNodes > MAX_CONTROL_VISITED_NODES + MAX_VISIBLE_TEXT_VISITED_NODES) {
      exceededNodeBudget = true;
      clone.textContent = REDACTED;
      break;
    }
    if (node.nodeType === 1) {
      pendingNodes.push(...node.childNodes);
    } else if (node.nodeType !== 3) {
      node.parentNode?.removeChild(node);
    }
  }
  for (const blocked of clone.querySelectorAll("script,style,template,noscript")) blocked.remove();
  const descendants = clone.querySelectorAll("*");
  const exceededElementBudget =
    exceededNodeBudget || descendants.length + 1 > MAX_CONTROL_VISITED_NODES;
  const elements = exceededElementBudget ? [clone] : [clone, ...descendants];
  for (const element of elements) {
    const sensitive = isSensitiveControl(element);
    for (const attribute of [...element.attributes]) {
      const safe = safeStructuralAttribute(attribute.name, attribute.value);
      if (safe === undefined) {
        element.removeAttribute(attribute.name);
      } else {
        element.setAttribute(attribute.name.toLowerCase(), safe);
      }
    }
    if (["INPUT", "SELECT", "TEXTAREA", "OPTION"].includes(element.tagName)) {
      if (element.hasAttribute("value")) element.setAttribute("value", REDACTED);
      if (element.tagName === "TEXTAREA" || element.tagName === "OPTION") {
        element.textContent = REDACTED;
      }
    } else if (sensitive) {
      element.textContent = REDACTED;
    }
  }
  if (exceededElementBudget) {
    clone.textContent = REDACTED;
    return boundedUtf8(clone.outerHTML, MAX_SANITIZED_HTML_LENGTH);
  }
  const walker = clone.ownerDocument.createTreeWalker(clone, 4);
  let visited = 0;
  while (visited < MAX_VISIBLE_TEXT_VISITED_NODES) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    const safe = observationString(node.nodeValue, MAX_CONTROL_FIELD_LENGTH * 8);
    node.nodeValue = safe ?? "";
  }
  if (walker.nextNode()) clone.textContent = REDACTED;
  return boundedUtf8(clone.outerHTML, MAX_SANITIZED_HTML_LENGTH);
}

function observeRoot(document: Document, root: Element, includeHtml: boolean): PageObservation {
  const observation: PageObservation = {
    url: observationUrl(document.URL),
    title: observationString(document.title, MAX_TITLE_LENGTH) ?? "",
    visibleText: visibleText(document, root),
    controls: [],
  };
  let serializedBytes = byteLength(JSON.stringify(observation));
  const helperBudget: WorkBudget = { remaining: MAX_CONTROL_HELPER_VISITS };
  const labelsByControlId = new Map<string, Element>();
  const siblingPositions = new WeakMap<Element, number>();
  const siblingCountsByParent = new WeakMap<Element, Map<string, number>>();
  const candidateControls: Element[] = [];
  if (root.matches(CONTROL_SELECTOR)) candidateControls.push(root);
  // Hidden subtrees are rejected by the walker's filter, so they cost neither
  // the visit cap nor the helper budget. The filter still sees every element
  // that is not inside a rejected subtree, which keeps sibling positions exact.
  let filterCalls = 0;
  let controlsTruncated = false;
  // Each open shadow root met on the way is walked after the tree holding it.
  const roots: Node[] = [root];
  if (root.shadowRoot) roots.push(root.shadowRoot);
  const filter = {
    acceptNode: (node: Node): number => {
      const element = node as Element;
      filterCalls += 1;
      if (filterCalls > MAX_CONTROL_FILTER_VISITS) {
        controlsTruncated = true;
        return 2;
      }
      const parent = element.parentElement;
      if (parent) {
        let counts = siblingCountsByParent.get(parent);
        if (!counts) {
          counts = new Map<string, number>();
          siblingCountsByParent.set(parent, counts);
        }
        const position = (counts.get(element.tagName) ?? 0) + 1;
        counts.set(element.tagName, position);
        siblingPositions.set(element, position);
      }
      try {
        if (element.hasAttribute("hidden") || element.getAttribute("aria-hidden") === "true") {
          return 2;
        }
        const style = element.ownerDocument.defaultView?.getComputedStyle(element);
        if (style?.display === "none") return 2;
        if (style?.visibility === "hidden") return 3;
      } catch {
        return 2;
      }
      return 1;
    },
  };
  let visited = 0;
  for (let index = 0; index < roots.length; index += 1) {
    const walker = document.createTreeWalker(roots[index]!, 1, filter);
    while (visited < MAX_CONTROL_VISITED_NODES && takeWork(helperBudget)) {
      const node = walker.nextNode();
      if (!node) break;
      visited += 1;
      const element = node as Element;
      if (element.shadowRoot) roots.push(element.shadowRoot);
      if (element.tagName === "LABEL") {
        const controlId = element.getAttribute("for");
        if (
          controlId &&
          byteLength(controlId) <= MAX_SELECTOR_LENGTH &&
          !labelsByControlId.has(controlId)
        ) {
          labelsByControlId.set(controlId, element);
        }
      }
      if (element.matches(CONTROL_SELECTOR)) {
        candidateControls.push(element);
      }
    }
  }
  if (visited >= MAX_CONTROL_VISITED_NODES || helperBudget.remaining <= 0) controlsTruncated = true;
  for (const element of candidateControls) {
    if (observation.controls.length >= MAX_CONTROL_COUNT) {
      controlsTruncated = true;
      break;
    }
    if (isElementHidden(element, helperBudget)) continue;
    const sensitive = isSensitiveControl(element, helperBudget);
    const observedPath = composedCssPath(element, helperBudget, siblingPositions, !sensitive);
    if (!observedPath) continue;
    const observedLabel = labelText(element, labelsByControlId, helperBudget);
    const observedName = accessibleName(
      element,
      observedLabel,
      helperBudget,
      sensitive,
      implicitRole(element, !sensitive),
    );
    const publicLabel = publicControlLabel(element);
    const label = publicLabel ?? (sensitive && observedLabel ? REDACTED : observedLabel);
    const testId = sensitive
      ? undefined
      : observationString(element.getAttribute("data-testid"));
    const attributes: Record<string, string> = {};
    // Match a11yTree and locateTarget, including omitted or invalid HTML types.
    // The normalized native kind is structural metadata, even for redacted controls.
    if (element.tagName === "INPUT") attributes.type = (element as HTMLInputElement).type;
    if (!sensitive) {
      for (const name of ["name", "placeholder", "autocomplete", "pattern", "min", "max", "step", "multiple"] as const) {
        const value = observationString(element.getAttribute(name));
        if (value) attributes[name] = value;
      }
      for (const name of ["required", "readonly", "checked", "multiple"] as const) {
        if ((element as unknown as Record<string, unknown>)[name] === true) attributes[name] = "true";
      }
      if (
        ["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName) &&
        !(element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement).validity.valid
      ) {
        attributes["aria-invalid"] = "true";
      }
    }
    const control = {
      cssPath: observedPath,
      ...(testId ? { testId } : {}),
      role: implicitRole(element, !sensitive),
      name: publicLabel ?? (sensitive && observedName ? REDACTED : observedName),
      label,
      value: controlValue(element, sensitive),
      attributes,
      disabled:
        element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true",
    };
    const controlBytes = byteLength(JSON.stringify(control)) + (observation.controls.length ? 1 : 0);
    if (serializedBytes + controlBytes > MAX_OBSERVATION_BYTES) {
      controlsTruncated = true;
      break;
    }
    observation.controls.push(control);
    serializedBytes += controlBytes;
  }
  if (helperBudget.remaining <= 0) controlsTruncated = true;
  if (controlsTruncated) observation.controlsTruncated = true;
  if (includeHtml) {
    const html = sanitizedHtml(root);
    const overhead = byteLength(JSON.stringify({ html: "" })) - 2;
    const remaining = Math.max(
      0,
      Math.min(MAX_SANITIZED_HTML_LENGTH, MAX_OBSERVATION_BYTES - serializedBytes - overhead),
    );
    observation.html = boundedUtf8(html, remaining);
  }
  return observation;
}

export function observeDocument(document: Document): PageObservation {
  const root = document.body ?? document.documentElement;
  return root
    ? observeRoot(document, root, false)
    : { url: observationUrl(document.URL), title: "", visibleText: "", controls: [] };
}

function actionInput(input: unknown): Record<string, unknown> {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw new ContentActionError("invalidInput", "content action input must be an object");
  }
  return input as Record<string, unknown>;
}

function inspectionRoot(document: Document, input: Record<string, unknown>): Element {
  if (typeof input.includeHtml !== "boolean") {
    throw new ContentActionError("invalidInput", "observe requires includeHtml to be a boolean");
  }
  const unresolvable = (message: string) => new ContentActionError("scopeUnresolvable", message);
  let selector: string | undefined;
  if (input.selector !== null && input.selector !== undefined) {
    if (
      typeof input.selector !== "string" ||
      input.selector.length === 0 ||
      byteLength(input.selector) > MAX_SELECTOR_LENGTH
    ) {
      throw unresolvable("observe selector must be a bounded CSS selector");
    }
    selector = input.selector;
  } else if (input.target !== null && input.target !== undefined) {
    if (typeof input.target !== "object" || Array.isArray(input.target)) {
      throw unresolvable("observe target must be an object");
    }
    const target = input.target as Record<string, unknown>;
    if (typeof target.css === "string" && target.css.length > 0) {
      if (byteLength(target.css) > MAX_SELECTOR_LENGTH) {
        throw unresolvable("observe target CSS must be bounded");
      }
      selector = target.css;
    } else if (typeof target.testId === "string" && target.testId.length > 0) {
      if (byteLength(target.testId) > MAX_CONTROL_FIELD_LENGTH) {
        throw unresolvable("observe target test ID must be bounded");
      }
      selector = `[data-testid="${cssString(target.testId)}"]`;
    } else {
      throw unresolvable("observe target requires a CSS selector or test ID");
    }
  }
  if (!selector) return document.body ?? document.documentElement;
  let root: Element | null;
  try {
    root = composedQuery(document, selector);
  } catch {
    throw unresolvable("observe selector is invalid");
  }
  if (!root || isElementHidden(root)) {
    throw new ContentActionError("targetNotFound", "observe target was not found");
  }
  return root;
}

function target(document: Document, input: Record<string, unknown>): Element {
  if (
    typeof input.cssPath !== "string" ||
    input.cssPath.length === 0 ||
    byteLength(input.cssPath) > MAX_SELECTOR_LENGTH
  ) {
    throw new ContentActionError("invalidInput", "content action requires a bounded cssPath");
  }
  let element: Element | null;
  try {
    element = composedQuery(document, input.cssPath);
  } catch {
    throw new ContentActionError("invalidInput", "content action cssPath is invalid");
  }
  if (!element || isElementHidden(element)) {
    throw new ContentActionError("targetNotFound", "content action target was not found");
  }
  return element;
}

const A11Y_MAX_DEPTH = 32;
// The result sits at depth 3 of the actionCompleted event; each node adds an
// object and a children array, and its target adds one more object level.
const A11Y_MAX_NODE_LEVEL = 13;
const A11Y_MAX_VALUES = 19_000;
const A11Y_MAX_NODES = 2048;
const A11Y_MAX_SCOPE_VISITS = 100_000;
const A11Y_MAX_CHILDREN = 256;
// Elements whose text is never page text: unrendered content, and form
// controls whose text is their value.
const A11Y_TEXT_EXCLUDED_TAGS = new Set(["SCRIPT", "STYLE", "TEMPLATE", "NOSCRIPT", "IFRAME", "TEXTAREA", "SELECT"]);
const A11Y_STRUCTURAL_ROLES = new Set([
  "banner",
  "navigation",
  "main",
  "contentinfo",
  "complementary",
  "form",
  "search",
  "region",
  "heading",
  "list",
  "listitem",
  "table",
  "row",
  "cell",
  "columnheader",
  "rowheader",
  "img",
  "figure",
  "dialog",
  "alert",
  "status",
  "progressbar",
  "separator",
]);

export type LocatedTarget = {
  found: boolean;
  ambiguous: boolean;
  cssPath?: string;
  role?: string;
  inputType?: string;
  name?: string;
  disabled?: boolean;
};

type A11yTarget = {
  role: string;
  accessibleName: string;
  ordinal?: number;
  framePath?: Array<{ role: string; accessibleName: string; ordinal?: number }>;
};

type A11yNode = {
  role?: string;
  inputType?: string;
  name?: string;
  target?: A11yTarget;
  value?: string;
  description?: string;
  required?: boolean;
  disabled?: boolean;
  readOnly?: boolean;
  invalid?: boolean;
  checked?: boolean;
  autocomplete?: string;
  valueMin?: string;
  valueMax?: string;
  children?: A11yNode[];
};

const A11Y_ACTIONABLE_ROLES = new Set([
  "button",
  "checkbox",
  "combobox",
  "link",
  "listbox",
  "radio",
  "searchbox",
  "slider",
  "spinbutton",
  "switch",
  "textbox",
  "iframe",
]);

function a11yTree(
  document: Document,
  maxNodesInput: unknown,
  targetInput?: unknown,
  locateOnly = false,
  includeText = false,
): { nodes: A11yNode[]; truncated: boolean; located?: LocatedTarget } {
  let maxNodes = 256;
  if (typeof maxNodesInput === "number" && Number.isSafeInteger(maxNodesInput)) {
    maxNodes = Math.min(Math.max(1, maxNodesInput), A11Y_MAX_NODES);
  }
  const root = document.documentElement;
  const state = { remaining: maxNodes, truncated: false };
  if (!root) return { nodes: [], truncated: false };
  const labelsByControlId = new Map<string, Element>();
  for (const label of Array.from(document.querySelectorAll("label[for]")).slice(0, 4_096)) {
    const controlId = label.getAttribute("for");
    if (controlId && byteLength(controlId) <= MAX_SELECTOR_LENGTH && !labelsByControlId.has(controlId)) {
      labelsByControlId.set(controlId, label);
    }
  }

  const structuralRole = (element: Element): string | undefined => {
    const tag = element.tagName.toLowerCase();
    const landmark: Record<string, string> = {
      header: "banner",
      nav: "navigation",
      main: "main",
      footer: "contentinfo",
      aside: "complementary",
      form: "form",
      section: "region",
      ul: "list",
      ol: "list",
      li: "listitem",
      table: "table",
      tr: "row",
      td: "cell",
      th: "columnheader",
      img: "img",
      figure: "figure",
      dialog: "dialog",
      hr: "separator",
    };
    if (landmark[tag]) return landmark[tag];
    if (/^h[1-6]$/.test(tag)) return "heading";
    return undefined;
  };

  const semantics = (element: Element): { role?: string; name?: string; sensitive: boolean } => {
    const budget: WorkBudget = { remaining: 256 };
    const sensitive = isSensitiveControl(element, budget);
    const role = implicitRole(element, !sensitive) ?? structuralRole(element);
    const name = sensitive
      ? publicControlLabel(element) ?? REDACTED
      : accessibleName(
          element,
          labelText(element, labelsByControlId, budget),
          budget,
          sensitive,
          role,
        );
    return { role, name, sensitive };
  };

  const unresolvable = (message: string) => new ContentActionError("scopeUnresolvable", message);
  const notFound = () => new ContentActionError("targetNotFound", "a11y target was not found");
  const resolveScope = (spec: unknown): Element => {
    if (typeof spec !== "object" || spec === null || Array.isArray(spec)) {
      throw unresolvable("a11y target must be an object");
    }
    const { css, testId, role, accessibleName, ordinal } = spec as Record<string, unknown>;
    let selector: string | undefined;
    if (typeof css === "string" && css.length > 0) {
      selector = css;
    } else if (typeof testId === "string" && testId.length > 0) {
      selector = `[data-testid="${cssString(testId)}"]`;
    }
    if (selector !== undefined) {
      if (byteLength(selector) > MAX_SELECTOR_LENGTH) throw unresolvable("a11y target selector must be bounded");
      let found: Element | null;
      try {
        found = composedQuery(document, selector);
      } catch {
        throw unresolvable("a11y target selector is invalid");
      }
      if (!found || isElementHidden(found)) throw notFound();
      return found;
    }
    if (typeof role !== "string" || role.length === 0) {
      throw unresolvable("a11y target requires a role, CSS selector, or test ID");
    }
    if (accessibleName !== null && accessibleName !== undefined && typeof accessibleName !== "string") {
      throw unresolvable("a11y target accessibleName must be a string");
    }
    if (
      ordinal !== null &&
      ordinal !== undefined &&
      !(typeof ordinal === "number" && Number.isSafeInteger(ordinal) && ordinal >= 0)
    ) {
      throw unresolvable("a11y target ordinal must be a non-negative integer");
    }
    const wanted = typeof ordinal === "number" ? ordinal : 0;
    // Without an ordinal a second match makes the target ambiguous, so the
    // walk only needs to reach two matches; with one it needs ordinal + 1.
    const needed = typeof ordinal === "number" ? wanted + 1 : 2;
    const search = (hidden: (element: Element) => boolean): { matches: Element[]; visited: number } => {
      const matches: Element[] = [];
      let visited = 0;
      const walk = (element: Element, depth: number): void => {
        visited += 1;
        if (visited > A11Y_MAX_SCOPE_VISITS) return;
        try {
          if (hidden(element)) return;
          const found = semantics(element);
          if (
            found.role === role &&
            (typeof accessibleName !== "string" || found.name === accessibleName)
          ) {
            matches.push(element);
          }
        } catch {
          return;
        }
        if (depth < A11Y_MAX_DEPTH) {
          for (const child of composedElementChildren(element)) {
            if (matches.length >= needed || visited > A11Y_MAX_SCOPE_VISITS) return;
            walk(child, depth + 1);
          }
        }
      };
      walk(root, 0);
      return { matches, visited };
    };
    const { matches, visited } = search(isElementHidden);
    const picked = matches[wanted];
    if (!picked && visited > A11Y_MAX_SCOPE_VISITS) {
      throw new ContentActionError("budgetExhausted", "a11y target search exceeded its visit bound");
    }
    // An open modal dialog hides the rest of the page from assistive
    // technology; a target hidden only that way is behind the dialog.
    if (!picked && modalDialogOpen(document) && search(isRenderedHidden).matches[wanted]) {
      throw new ContentActionError("targetObscured", "a11y target is behind an open modal dialog");
    }
    if (typeof ordinal === "number") {
      if (!picked) throw notFound();
      return picked;
    }
    if (matches.length === 0) throw notFound();
    if (matches.length > 1) throw new ContentActionError("targetAmbiguous", "a11y target is ambiguous");
    return matches[0]!;
  };
  if (locateOnly) {
    if (targetInput === null || targetInput === undefined) {
      throw new ContentActionError("invalidInput", "locateTarget requires a target");
    }
    let element: Element;
    try {
      element = resolveScope(targetInput);
    } catch (error) {
      const reason = error instanceof ContentActionError ? error.reason : undefined;
      if (reason === "targetAmbiguous") {
        return { nodes: [], truncated: false, located: { found: false, ambiguous: true } };
      }
      if (reason === "targetNotFound") {
        return { nodes: [], truncated: false, located: { found: false, ambiguous: false } };
      }
      throw error;
    }
    const { role, name, sensitive } = semantics(element);
    // The page-wide walk stops at its visit bound, so sibling positions are
    // counted here, exactly, for the element and the ancestors its path uses.
    const siblingPositions = new WeakMap<Element, number>();
    const budget: WorkBudget = { remaining: A11Y_MAX_SCOPE_VISITS };
    for (
      let current: Element | null = element, depth = 0;
      current?.parentElement && depth < A11Y_MAX_DEPTH;
      current = current.parentElement, depth += 1
    ) {
      let position = 0;
      for (const sibling of Array.from(current.parentElement.children)) {
        if (!takeWork(budget)) break;
        if (sibling.tagName === current.tagName) position += 1;
        if (sibling === current) {
          siblingPositions.set(current, position);
          break;
        }
      }
    }
    const cssPathValue = composedCssPath(element, { remaining: A11Y_MAX_SCOPE_VISITS }, siblingPositions, !sensitive);
    if (!cssPathValue) return { nodes: [], truncated: false, located: { found: false, ambiguous: false } };
    return {
      nodes: [],
      truncated: false,
      located: {
        found: true,
        ambiguous: false,
        cssPath: cssPathValue,
        ...(role ? { role } : {}),
        ...(element.tagName === "INPUT" ? { inputType: (element as HTMLInputElement).type } : {}),
        ...(name ? { name } : {}),
        disabled: element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true",
      },
    };
  }
  const scope: Element =
    targetInput === null || targetInput === undefined ? root : resolveScope(targetInput);

  const targetTotals = new Map<string, number>();
  const targetKey = (role: string, name: string): string => `${role}\u0000${name}`;
  // Ordinals stay page-wide so a scoped snapshot's targets resolve exactly as
  // the same nodes' targets from a full snapshot do: the running per-key
  // counts at the moment the page-wide walk reaches the scope seed them.
  let scopeSeen: Map<string, number> | undefined;
  const countTargets = (element: Element, depth: number): void => {
    try {
      if (isElementHidden(element)) return;
      if (element === scope) scopeSeen = new Map(targetTotals);
      const { role, name } = semantics(element);
      if (role && name && name !== REDACTED && A11Y_ACTIONABLE_ROLES.has(role)) {
        const key = targetKey(role, name);
        targetTotals.set(key, (targetTotals.get(key) ?? 0) + 1);
      }
    } catch {
      // A node that throws during inspection (hostile DOM, detached style
      // context) is skipped, never fatal to the snapshot.
      return;
    }
    if (depth < A11Y_MAX_DEPTH) {
      for (const child of composedElementChildren(element).slice(0, 256)) {
        countTargets(child, depth + 1);
      }
    }
  };
  countTargets(root, 0);

  // A same-origin frame's tree is built from its own document; each target in
  // it carries the hop that re-resolves the iframe element, so it passes to
  // the click path verbatim. Cross-origin frames stay a leaf.
  const frameSeen = new Map<string, number>();
  const frameNodes = (frame: HTMLIFrameElement, name: string): A11yNode[] => {
    const key = targetKey("iframe", name);
    const seen = frameSeen.get(key) ?? 0;
    frameSeen.set(key, seen + 1);
    let frameDocument: Document | null = null;
    try {
      frameDocument = frame.contentDocument;
    } catch {
      return [];
    }
    if (!frameDocument?.documentElement) return [];
    if (state.remaining <= 0) {
      state.truncated = true;
      return [];
    }
    const inner = a11yTree(frameDocument, state.remaining, undefined, false, includeText);
    if (inner.truncated) state.truncated = true;
    const count = (nodes: A11yNode[]): number =>
      nodes.reduce((total, node) => total + 1 + count(node.children ?? []), 0);
    state.remaining = Math.max(0, state.remaining - count(inner.nodes));
    const hop = {
      role: "iframe",
      accessibleName: name,
      ...(targetTotals.get(key)! > 1 ? { ordinal: seen } : {}),
    };
    const stamp = (nodes: A11yNode[]): void => {
      for (const node of nodes) {
        if (node.target) node.target.framePath = [hop, ...(node.target.framePath ?? [])];
        stamp(node.children ?? []);
      }
    };
    stamp(inner.nodes);
    return inner.nodes;
  };

  // A visible text run outside any named node, reported as Chromium does.
  const staticText = (parent: Element, text: Node, level: number): A11yNode[] => {
    let name: string | undefined;
    try {
      name = isSensitiveTextContext(parent) ? REDACTED : observationString(text.nodeValue);
    } catch {
      return [];
    }
    if (!name) return [];
    if (level > A11Y_MAX_NODE_LEVEL) {
      state.truncated = true;
      return [];
    }
    state.remaining -= 1;
    return [{ role: "StaticText", name }];
  };

  const build = (element: Element, depth: number, level: number, covered: boolean): A11yNode[] => {
    let role: string | undefined;
    let name: string | undefined;
    let sensitive = false;
    try {
      ({ role, name, sensitive } = semantics(element));
    } catch {
      // Skip nodes that throw during inspection; never fatal.
      return [];
    }
    // A hidden subtree is discarded whole, so it must be rejected before any
    // descendant spends the node budget.
    let hidden = false;
    try {
      hidden = isElementHidden(element);
    } catch {
      hidden = true;
    }
    if (hidden) return [];
    if (role && level > A11Y_MAX_NODE_LEVEL) {
      state.truncated = true;
      return [];
    }
    // A node reserves its own slot before its descendants do, so truncation
    // drops later and deeper nodes and never an ancestor of kept ones.
    if (role) {
      if (state.remaining <= 0) {
        state.truncated = true;
        return [];
      }
      state.remaining -= 1;
    }
    // Text a node's own name or value already carries is not repeated.
    const textCovered =
      covered ||
      (role !== undefined && NAME_FROM_CONTENT_ROLES.has(role)) ||
      A11Y_TEXT_EXCLUDED_TAGS.has(element.tagName);
    const childLevel = role ? level + 1 : level;
    const children: A11yNode[] = [];
    if (depth < A11Y_MAX_DEPTH) {
      const childNodes = composedChildren(element);
      if (childNodes.filter((child) => child.nodeType === 1).length > A11Y_MAX_CHILDREN) {
        state.truncated = true;
      }
      let elements = 0;
      for (const child of childNodes) {
        if (state.remaining <= 0) {
          state.truncated = true;
          break;
        }
        if (child.nodeType === 1) {
          if (elements >= A11Y_MAX_CHILDREN) break;
          elements += 1;
          children.push(...build(child as Element, depth + 1, childLevel, textCovered));
        } else if (child.nodeType === 3 && includeText && !textCovered) {
          children.push(...staticText(element, child, childLevel));
        }
      }
    }
    if (!role) return children;
    if (element.tagName === "IFRAME" && name && name !== REDACTED) {
      children.push(...frameNodes(element as HTMLIFrameElement, name));
    }
    const node: A11yNode = { role };
    if (name) node.name = name;
    if (["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)) {
      try {
        const control = element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;
        node.value = controlValue(element, sensitive) ?? "";
        node.required = control.required;
        node.disabled = control.disabled;
        node.invalid = !control.validity.valid;
        if (element.tagName !== "SELECT") {
          node.readOnly = (control as HTMLInputElement | HTMLTextAreaElement).readOnly;
        }
        if (element.tagName === "INPUT") {
          const input = control as HTMLInputElement;
          node.inputType = input.type;
          if (["checkbox", "radio"].includes(input.type)) node.checked = input.checked;
          const autocomplete = observationString(input.autocomplete);
          if (autocomplete) node.autocomplete = autocomplete;
          const valueMin = observationString(input.min);
          const valueMax = observationString(input.max);
          if (valueMin) node.valueMin = valueMin;
          if (valueMax) node.valueMax = valueMax;
        }
        const description = observationString(element.getAttribute("aria-description"));
        if (description) node.description = description;
      } catch {
        // Control detail enrichment is best-effort: a hostile control drops
        // the extras, never the node.
      }
    }
    if (["true", "grammar", "spelling"].includes(element.getAttribute("aria-invalid")?.trim().toLowerCase() ?? "")) {
      node.invalid = true;
    }
    if (children.length) node.children = children;
    return [node];
  };

  const nodes = build(scope, 0, 0, false);
  const targetSeen = new Map<string, number>(scopeSeen ?? []);
  const sendBudget = { values: A11Y_MAX_VALUES, bytes: MAX_OBSERVATION_BYTES };
  const annotateTargets = (candidates: A11yNode[]): A11yNode[] => {
    const kept: A11yNode[] = [];
    for (const node of candidates) {
      if (
        !node.target &&
        node.role &&
        node.name &&
        node.name !== REDACTED &&
        A11Y_ACTIONABLE_ROLES.has(node.role)
      ) {
        const key = targetKey(node.role, node.name);
        const ordinal = targetSeen.get(key) ?? 0;
        node.target = {
          role: node.role,
          accessibleName: node.name,
          ...(targetTotals.get(key)! > 1 ? { ordinal } : {}),
        };
        targetSeen.set(key, ordinal + 1);
      }
      const { children, ...own } = node;
      const values =
        2 + Object.keys(own).length + (own.target ? Object.keys(own.target).length : 0);
      const bytes = byteLength(JSON.stringify(own)) + 4;
      if (sendBudget.values < values || sendBudget.bytes < bytes) {
        state.truncated = true;
        sendBudget.values = 0;
        break;
      }
      sendBudget.values -= values;
      sendBudget.bytes -= bytes;
      if (children) {
        const keptChildren = annotateTargets(children);
        if (keptChildren.length) node.children = keptChildren;
        else delete node.children;
      }
      kept.push(node);
    }
    return kept;
  };
  const sent = annotateTargets(nodes);
  if (state.remaining <= 0) state.truncated = true;
  return { nodes: sent, truncated: state.truncated };
}

export function executeContentAction(
  document: Document,
  operation: string,
  input: unknown,
): unknown {
  const parsed = actionInput(input);
  if (operation === "observe") {
    return observeRoot(document, inspectionRoot(document, parsed), parsed.includeHtml as boolean);
  }
  if (operation === "a11yTree") {
    return a11yTree(document, parsed.maxNodes, parsed.target, false, parsed.includeText === true);
  }
  if (operation === "locateTarget") {
    return a11yTree(document, 1, parsed.target, true).located;
  }
  const element = target(document, parsed);
  switch (operation) {
    case "click":
      (element as HTMLElement).click();
      return { clicked: true };
    case "focus":
      (element as HTMLElement).focus();
      return { focused: true };
    case "type": {
      if (typeof parsed.text !== "string" || parsed.text.length > MAX_VISIBLE_TEXT_LENGTH) {
        throw new ContentActionError("invalidInput", "type requires bounded text");
      }
      if (!["INPUT", "TEXTAREA"].includes(element.tagName)) {
        throw new ContentActionError("invalidInput", "type target must accept text");
      }
      (element as HTMLInputElement | HTMLTextAreaElement).value = parsed.text;
      const EventConstructor = document.defaultView?.Event;
      if (EventConstructor) {
        element.dispatchEvent(new EventConstructor("input", { bubbles: true }));
        element.dispatchEvent(new EventConstructor("change", { bubbles: true }));
      }
      return { typed: true };
    }
    default:
      throw new ContentActionError("invalidInput", `unsupported content operation: ${operation}`);
  }
}

// The reply to a content action: its output, or the reason it failed. Only
// the reason and a thrown error's name leave the page, never its message.
export function contentActionReply(document: Document, operation: string, input: unknown): unknown {
  try {
    return executeContentAction(document, operation, input);
  } catch (error) {
    if (error instanceof ContentActionError) {
      return { [CONTENT_FAILURE_KEY]: { reason: error.reason } };
    }
    const errorName = error instanceof Error ? error.name : undefined;
    return {
      [CONTENT_FAILURE_KEY]: {
        reason: "scriptException",
        ...(typeof errorName === "string" ? { errorName: errorName.slice(0, 64) } : {}),
      },
    };
  }
}

type ContentBrowserApi = {
  runtime: {
    sendMessage(message: unknown): Promise<unknown>;
    onMessage: {
      addListener(listener: (message: unknown) => unknown): void;
    };
  };
};

declare const browser: ContentBrowserApi | undefined;

// Fingerprint spoofing is applied via BiDi preload (worker) or a registered
// document_start content script (extension toggle) — not here — to avoid the
// isolated-world + async-storage race at document_start.

if (typeof browser !== "undefined") {
  void browser.runtime.sendMessage({ type: "companionFrameReady" }).catch(() => undefined);
  browser.runtime.onMessage.addListener((message) => {
    if (
      typeof message !== "object" ||
      message === null ||
      !("type" in message) ||
      message.type !== "companionAction" ||
      !("operation" in message) ||
      typeof message.operation !== "string" ||
      !("input" in message)
    ) {
      return undefined;
    }
    return Promise.resolve(contentActionReply(document, message.operation, message.input));
  });
}
