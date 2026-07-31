import { MAX_COMPANION_PAYLOAD_BYTES } from "./protocol.js";

export const MAX_VISIBLE_TEXT_LENGTH = 64 * 1024;
export const MAX_CONTROL_COUNT = 512;
export const MAX_VISIBLE_TEXT_VISITED_NODES = 4_096;
export const MAX_CONTROL_VISITED_NODES = 4_096;
export const MAX_ELEMENT_TEXT_VISITED_NODES = 512;
export const MAX_CONTROL_HELPER_VISITS = 16_384;
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
};

const CONTROL_SELECTOR = [
  "a[href]",
  "button",
  "input",
  "select",
  "textarea",
  "[role]",
  '[contenteditable="true"]',
].join(",");

const SENSITIVE_MARKER =
  /(?:authorization|auth(?:entication)?|bearer|token|secret|password|passwd|api[-_]?key|credential)/i;
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
  if (containsSensitiveMaterial(normalized)) {
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
    return observationString(url.href, MAX_URL_LENGTH) ?? "";
  } catch {
    return observationString(value, MAX_URL_LENGTH) ?? "";
  }
}

function isElementHidden(element: Element, budget?: WorkBudget): boolean {
  let visited = 0;
  for (let current: Element | null = element; current; current = current.parentElement) {
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

function isSensitiveTextContext(element: Element): boolean {
  const control = element.closest(CONTROL_SELECTOR);
  if (control && isSensitiveControl(control)) return true;
  const label = element.closest("label");
  if (!label) return false;
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
    if (byteLength(selector) <= MAX_SELECTOR_LENGTH) return selector;
  }
  const stable = allowStableMetadata ? safeStableAttribute(element) : undefined;
  if (stable) return `[${stable.name}="${cssString(stable.value)}"]`;

  const parts: string[] = [];
  for (let current: Element | null = element; current && parts.length < 8; current = current.parentElement) {
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
      const parentSelector = `#${cssIdentifier(parentId)}`;
      const candidate = [parentSelector, ...parts].join(" > ");
      if (byteLength(candidate) <= MAX_SELECTOR_LENGTH) {
        parts.unshift(parentSelector);
        break;
      }
    }
  }
  const path = parts.join(" > ");
  if (byteLength(path) <= MAX_SELECTOR_LENGTH) return path;
  return parts.at(-1) ?? element.tagName.toLowerCase();
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
  if (tag === "input") {
    const type = (element.getAttribute("type") ?? "text").toLowerCase();
    if (["button", "submit", "reset", "image"].includes(type)) return "button";
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
    const referenced = element.ownerDocument.getElementById(id);
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
  const walker = element.ownerDocument.createTreeWalker(element, 4);
  let visited = 0;
  while (visited < MAX_ELEMENT_TEXT_VISITED_NODES) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    if (!takeWork(budget)) break;
    const parent = node.parentElement;
    if (!parent || isElementHidden(parent, budget)) continue;
    const separatorBytes = output ? 1 : 0;
    const remaining = maximum - outputBytes - separatorBytes;
    if (remaining <= 0) break;
    const text = observationString(node.nodeValue, remaining);
    if (!text) continue;
    output += `${output ? " " : ""}${text}`;
    outputBytes += separatorBytes + byteLength(text);
  }
  return output || undefined;
}

function accessibleName(
  element: Element,
  label: string | undefined,
  budget: WorkBudget,
  sensitive: boolean,
): string | undefined {
  return (
    observationString(element.getAttribute("aria-label")) ??
    labelledByText(element, budget) ??
    label ??
    observationString(element.getAttribute("alt")) ??
    observationString(element.getAttribute("title")) ??
    boundedElementText(element, MAX_CONTROL_FIELD_LENGTH, budget) ??
    (element.tagName === "INPUT" && !sensitive
      ? observationString(element.getAttribute("value"))
      : undefined)
  );
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
    const value = attribute.value.slice(0, MAX_CONTROL_FIELD_LENGTH * 8);
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
  if (sensitive) return REDACTED;
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
  const walker = document.createTreeWalker(root, 1);
  let visited = 0;
  while (visited < MAX_CONTROL_VISITED_NODES && takeWork(helperBudget)) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    const element = node as Element;
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
  for (const element of candidateControls) {
    if (observation.controls.length >= MAX_CONTROL_COUNT) break;
    if (isElementHidden(element, helperBudget)) continue;
    const sensitive = isSensitiveControl(element, helperBudget);
    const observedPath = cssPath(element, helperBudget, siblingPositions, !sensitive);
    if (!observedPath) continue;
    const observedLabel = labelText(element, labelsByControlId, helperBudget);
    const observedName = accessibleName(element, observedLabel, helperBudget, sensitive);
    const label = sensitive && observedLabel ? REDACTED : observedLabel;
    const testId = sensitive
      ? undefined
      : observationString(element.getAttribute("data-testid"));
    const attributes: Record<string, string> = {};
    if (!sensitive) {
      for (const name of ["name", "type", "placeholder", "autocomplete", "pattern", "min", "max", "step", "multiple"] as const) {
        const value = observationString(element.getAttribute(name));
        if (value) attributes[name] = value;
      }
      for (const name of ["required", "readonly", "checked", "multiple"] as const) {
        if ((element as unknown as Record<string, unknown>)[name] === true) attributes[name] = "true";
      }
    }
    const control = {
      cssPath: observedPath,
      ...(testId ? { testId } : {}),
      role: implicitRole(element, !sensitive),
      name: sensitive && observedName ? REDACTED : observedName,
      label,
      value: controlValue(element, sensitive),
      attributes,
      disabled:
        element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true",
    };
    const controlBytes = byteLength(JSON.stringify(control)) + (observation.controls.length ? 1 : 0);
    if (serializedBytes + controlBytes > MAX_OBSERVATION_BYTES) break;
    observation.controls.push(control);
    serializedBytes += controlBytes;
  }
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
    throw new Error("content action input must be an object");
  }
  return input as Record<string, unknown>;
}

function inspectionRoot(document: Document, input: Record<string, unknown>): Element {
  if (typeof input.includeHtml !== "boolean") {
    throw new Error("observe requires includeHtml to be a boolean");
  }
  let selector: string | undefined;
  if (input.selector !== null && input.selector !== undefined) {
    if (
      typeof input.selector !== "string" ||
      input.selector.length === 0 ||
      byteLength(input.selector) > MAX_SELECTOR_LENGTH
    ) {
      throw new Error("observe selector must be a bounded CSS selector");
    }
    selector = input.selector;
  } else if (input.target !== null && input.target !== undefined) {
    if (typeof input.target !== "object" || Array.isArray(input.target)) {
      throw new Error("observe target must be an object");
    }
    const target = input.target as Record<string, unknown>;
    if (typeof target.css === "string" && target.css.length > 0) {
      if (byteLength(target.css) > MAX_SELECTOR_LENGTH) {
        throw new Error("observe target CSS must be bounded");
      }
      selector = target.css;
    } else if (typeof target.testId === "string" && target.testId.length > 0) {
      if (byteLength(target.testId) > MAX_CONTROL_FIELD_LENGTH) {
        throw new Error("observe target test ID must be bounded");
      }
      selector = `[data-testid="${cssString(target.testId)}"]`;
    } else {
      throw new Error("observe target requires a CSS selector or test ID");
    }
  }
  if (!selector) return document.body ?? document.documentElement;
  let root: Element | null;
  try {
    root = document.querySelector(selector);
  } catch {
    throw new Error("observe selector is invalid");
  }
  if (!root || isElementHidden(root)) throw new Error("observe target was not found");
  return root;
}

function target(document: Document, input: Record<string, unknown>): Element {
  if (
    typeof input.cssPath !== "string" ||
    input.cssPath.length === 0 ||
    byteLength(input.cssPath) > MAX_SELECTOR_LENGTH
  ) {
    throw new Error("content action requires a bounded cssPath");
  }
  let element: Element | null;
  try {
    element = document.querySelector(input.cssPath);
  } catch {
    throw new Error("content action cssPath is invalid");
  }
  if (!element || isElementHidden(element)) throw new Error("content action target was not found");
  return element;
}

const A11Y_MAX_DEPTH = 32;
const A11Y_MAX_NODES = 2048;
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

type A11yNode = {
  role?: string;
  name?: string;
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

function a11yTree(document: Document, maxNodesInput: unknown): { nodes: A11yNode[]; truncated: boolean } {
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

  const build = (element: Element, depth: number): A11yNode | undefined => {
    const budget: WorkBudget = { remaining: 64 };
    const sensitive = isSensitiveControl(element, budget);
    const role = implicitRole(element, !sensitive) ?? structuralRole(element);
    const name = sensitive
      ? REDACTED
      : accessibleName(element, labelText(element, labelsByControlId, budget), budget, sensitive);
    const children: A11yNode[] = [];
    if (depth < A11Y_MAX_DEPTH) {
      for (const child of Array.from(element.children).slice(0, 256)) {
        if (state.remaining <= 0) break;
        const built = build(child, depth + 1);
        if (built) children.push(built);
      }
    }
    if (isElementHidden(element)) return undefined;
    if (!role) return children.length ? { children } : undefined;
    if (state.remaining <= 0) {
      state.truncated = true;
      return undefined;
    }
    state.remaining -= 1;
    const node: A11yNode = { role };
    if (name) node.name = name;
    if (["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)) {
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
    }
    if (children.length) node.children = children;
    return node;
  };

  const tree = build(root, 0);
  let nodes = tree ? [tree] : [];
  while (nodes.length === 1) {
    const onlyNode = nodes[0];
    if (!onlyNode || onlyNode.role || onlyNode.name || !onlyNode.children) break;
    nodes = onlyNode.children;
  }
  if (state.remaining <= 0) state.truncated = true;
  return { nodes, truncated: state.truncated };
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
    return a11yTree(document, parsed.maxNodes);
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
        throw new Error("type requires bounded text");
      }
      if (!["INPUT", "TEXTAREA"].includes(element.tagName)) {
        throw new Error("type target must accept text");
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
      throw new Error(`unsupported content operation: ${operation}`);
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
    return Promise.resolve(executeContentAction(document, message.operation, message.input));
  });
}
