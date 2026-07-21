import { MAX_COMPANION_PAYLOAD_BYTES } from "./protocol.js";

export const MAX_VISIBLE_TEXT_LENGTH = 64 * 1024;
export const MAX_CONTROL_COUNT = 512;
export const MAX_VISIBLE_TEXT_VISITED_NODES = 4_096;
export const MAX_CONTROL_VISITED_NODES = 4_096;
export const MAX_ELEMENT_TEXT_VISITED_NODES = 512;
export const MAX_CONTROL_FIELD_LENGTH = 256;
export const MAX_SELECTOR_LENGTH = 512;
export const MAX_OBSERVATION_BYTES = MAX_COMPANION_PAYLOAD_BYTES - 64 * 1024;
const MAX_URL_LENGTH = 2 * 1024;
const MAX_TITLE_LENGTH = 1024;
const MAX_ROLE_LENGTH = 64;
const REDACTED = "[redacted]";
const MAX_ANCESTOR_VISITS = 256;

export type PageObservation = {
  url: string;
  title: string;
  visibleText: string;
  controls: Array<{
    cssPath: string;
    role?: string;
    name?: string;
    label?: string;
    value?: string;
    disabled: boolean;
  }>;
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

function isElementHidden(element: Element): boolean {
  let visited = 0;
  for (let current: Element | null = element; current; current = current.parentElement) {
    visited += 1;
    if (visited > MAX_ANCESTOR_VISITS) return true;
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

function visibleText(document: Document): string {
  const root = document.body;
  if (!root) return "";
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

function cssPath(element: Element, allowStableMetadata = true): string {
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
    const tag = current.tagName.toLowerCase();
    const parent: HTMLElement | null = current.parentElement;
    if (!parent) {
      parts.unshift(tag);
      break;
    }
    let peerCount = 0;
    let position = 0;
    for (let childIndex = 0; childIndex < parent.children.length; childIndex += 1) {
      const child: Element | null = parent.children.item(childIndex);
      if (!child) continue;
      if (child.tagName !== current.tagName) continue;
      peerCount += 1;
      if (child === current) position = peerCount;
    }
    parts.unshift(peerCount > 1 ? `${tag}:nth-of-type(${position})` : tag);
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

function labelledByText(element: Element): string | undefined {
  const reference = element.getAttribute("aria-labelledby")?.slice(0, MAX_CONTROL_FIELD_LENGTH * 8);
  if (!reference) return undefined;
  if (containsSensitiveMaterial(reference)) return REDACTED;
  let output = "";
  let outputBytes = 0;
  for (const id of reference.trim().split(/\s+/, 16)) {
    const separatorBytes = output ? 1 : 0;
    const remaining = MAX_CONTROL_FIELD_LENGTH - outputBytes - separatorBytes;
    if (remaining <= 0) break;
    const referenced = element.ownerDocument.getElementById(id);
    const text = referenced ? boundedElementText(referenced, remaining) : undefined;
    if (!text) continue;
    output += `${output ? " " : ""}${text}`;
    outputBytes += separatorBytes + byteLength(text);
  }
  return observationString(output);
}

function labelText(element: Element): string | undefined {
  const id = element.getAttribute("id");
  if (id && byteLength(id) <= MAX_SELECTOR_LENGTH) {
    try {
      const label = element.ownerDocument.querySelector(`label[for="${cssString(id)}"]`);
      const text = label ? boundedElementText(label) : undefined;
      if (text) return text;
    } catch {}
  }
  const closest = element.closest("label");
  return closest ? boundedElementText(closest) : undefined;
}

function boundedElementText(
  element: Element,
  maximum = MAX_CONTROL_FIELD_LENGTH,
): string | undefined {
  let output = "";
  let outputBytes = 0;
  const walker = element.ownerDocument.createTreeWalker(element, 4);
  let visited = 0;
  while (visited < MAX_ELEMENT_TEXT_VISITED_NODES) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    const parent = node.parentElement;
    if (!parent || isElementHidden(parent)) continue;
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

function accessibleName(element: Element, label: string | undefined): string | undefined {
  return (
    observationString(element.getAttribute("aria-label")) ??
    labelledByText(element) ??
    label ??
    observationString(element.getAttribute("alt")) ??
    observationString(element.getAttribute("title")) ??
    boundedElementText(element) ??
    (element.tagName === "INPUT" && !isSensitiveControl(element)
      ? observationString(element.getAttribute("value"))
      : undefined)
  );
}

function isSensitiveControl(element: Element): boolean {
  if (
    element.tagName === "INPUT" &&
    (element.getAttribute("type") ?? "text").toLowerCase() === "password"
  ) {
    return true;
  }
  if (element.attributes.length > 128) return true;
  for (const attribute of element.attributes) {
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

function controlValue(element: Element): string | undefined {
  if (!["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)) return undefined;
  if (isSensitiveControl(element)) return REDACTED;
  const value = (element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement).value;
  return observationString(value);
}

export function observeDocument(document: Document): PageObservation {
  const observation: PageObservation = {
    url: observationUrl(document.URL),
    title: observationString(document.title, MAX_TITLE_LENGTH) ?? "",
    visibleText: visibleText(document),
    controls: [],
  };
  let serializedBytes = byteLength(JSON.stringify(observation));
  const root = document.body;
  if (!root) return observation;
  const walker = document.createTreeWalker(root, 1);
  let visited = 0;
  while (visited < MAX_CONTROL_VISITED_NODES) {
    const node = walker.nextNode();
    if (!node) break;
    visited += 1;
    if (observation.controls.length >= MAX_CONTROL_COUNT) break;
    const element = node as Element;
    if (!element.matches(CONTROL_SELECTOR) || isElementHidden(element)) continue;
    const sensitive = isSensitiveControl(element);
    const observedLabel = labelText(element);
    const observedName = accessibleName(element, observedLabel);
    const label = sensitive && observedLabel ? REDACTED : observedLabel;
    const control = {
      cssPath: cssPath(element, !sensitive),
      role: implicitRole(element, !sensitive),
      name: sensitive && observedName ? REDACTED : observedName,
      label,
      value: controlValue(element),
      disabled:
        element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true",
    };
    const controlBytes = byteLength(JSON.stringify(control)) + (observation.controls.length ? 1 : 0);
    if (serializedBytes + controlBytes > MAX_OBSERVATION_BYTES) break;
    observation.controls.push(control);
    serializedBytes += controlBytes;
  }
  return observation;
}

function actionInput(input: unknown): Record<string, unknown> {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw new Error("content action input must be an object");
  }
  return input as Record<string, unknown>;
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

export function executeContentAction(
  document: Document,
  operation: string,
  input: unknown,
): unknown {
  if (operation === "observe") return observeDocument(document);
  const parsed = actionInput(input);
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
    return executeContentAction(document, message.operation, message.input);
  });
}
