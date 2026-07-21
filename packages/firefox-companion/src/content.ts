export const MAX_VISIBLE_TEXT_LENGTH = 64 * 1024;
export const MAX_CONTROL_COUNT = 512;
const MAX_CONTROL_FIELD_LENGTH = 2 * 1024;

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
const SECRET_VALUE = /^(?:bearer|basic)\s+\S+$/i;

function bounded(value: string | null | undefined, maximum = MAX_CONTROL_FIELD_LENGTH): string | undefined {
  const normalized = value?.replace(/\s+/g, " ").trim();
  return normalized ? normalized.slice(0, maximum) : undefined;
}

function isElementHidden(element: Element): boolean {
  for (let current: Element | null = element; current; current = current.parentElement) {
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

function visibleText(document: Document): string {
  const root = document.body;
  if (!root) return "";
  const texts: string[] = [];
  const walker = document.createTreeWalker(root, 4);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const parent = node.parentElement;
    if (
      !parent ||
      ["SCRIPT", "STYLE", "TEMPLATE", "NOSCRIPT"].includes(parent.tagName) ||
      isElementHidden(parent)
    ) {
      continue;
    }
    const text = bounded(node.nodeValue, MAX_VISIBLE_TEXT_LENGTH);
    if (text) texts.push(text);
  }
  return texts.join(" ").replace(/\s+/g, " ").trim().slice(0, MAX_VISIBLE_TEXT_LENGTH);
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
      !SENSITIVE_MARKER.test(value) &&
      !SECRET_VALUE.test(value)
    ) {
      return { name, value };
    }
  }
  return undefined;
}

function cssPath(element: Element): string {
  const id = element.getAttribute("id");
  if (id && !SECRET_VALUE.test(id)) return `#${cssIdentifier(id)}`;
  const stable = safeStableAttribute(element);
  if (stable) return `[${stable.name}="${cssString(stable.value)}"]`;

  const parts: string[] = [];
  for (let current: Element | null = element; current && parts.length < 8; current = current.parentElement) {
    const tag = current.tagName.toLowerCase();
    const parent = current.parentElement;
    if (!parent) {
      parts.unshift(tag);
      break;
    }
    const peers = Array.from(parent.children).filter((child) => child.tagName === current?.tagName);
    const position = peers.indexOf(current) + 1;
    parts.unshift(peers.length > 1 ? `${tag}:nth-of-type(${position})` : tag);
    const parentId = parent.getAttribute("id");
    if (parentId && !SECRET_VALUE.test(parentId)) {
      parts.unshift(`#${cssIdentifier(parentId)}`);
      break;
    }
  }
  return parts.join(" > ").slice(0, MAX_CONTROL_FIELD_LENGTH);
}

function implicitRole(element: Element): string | undefined {
  const explicit = bounded(element.getAttribute("role"));
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
  const ids = element.getAttribute("aria-labelledby")?.trim().split(/\s+/) ?? [];
  return bounded(
    ids
      .map((id) => element.ownerDocument.getElementById(id)?.textContent ?? "")
      .filter(Boolean)
      .join(" "),
  );
}

function labelText(element: Element): string | undefined {
  const id = element.getAttribute("id");
  if (id) {
    const labels = Array.from(element.ownerDocument.querySelectorAll("label")).filter(
      (label) => label.getAttribute("for") === id,
    );
    const label = bounded(labels.map((item) => item.textContent ?? "").join(" "));
    if (label) return label;
  }
  return bounded(element.closest("label")?.textContent);
}

function accessibleName(element: Element, label: string | undefined): string | undefined {
  return (
    bounded(element.getAttribute("aria-label")) ??
    labelledByText(element) ??
    label ??
    bounded(element.getAttribute("alt")) ??
    bounded(element.getAttribute("title")) ??
    bounded(element.textContent) ??
    (element.tagName === "INPUT" && !isSensitiveControl(element)
      ? bounded(element.getAttribute("value"))
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
  return Array.from(element.attributes).some(
    ({ name, value }) =>
      SENSITIVE_MARKER.test(name) || SENSITIVE_MARKER.test(value) || SECRET_VALUE.test(value),
  );
}

function controlValue(element: Element): string | undefined {
  if (!["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)) return undefined;
  if (isSensitiveControl(element)) return "[redacted]";
  const value = (element as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement).value;
  return bounded(value);
}

export function observeDocument(document: Document): PageObservation {
  const controls = Array.from(document.querySelectorAll(CONTROL_SELECTOR))
    .filter((element) => !isElementHidden(element))
    .slice(0, MAX_CONTROL_COUNT)
    .map((element) => {
      const label = labelText(element);
      return {
        cssPath: cssPath(element),
        role: implicitRole(element),
        name: accessibleName(element, label),
        label,
        value: controlValue(element),
        disabled:
          element.hasAttribute("disabled") || element.getAttribute("aria-disabled") === "true",
      };
    });

  return {
    url: document.URL,
    title: bounded(document.title) ?? "",
    visibleText: visibleText(document),
    controls,
  };
}

function actionInput(input: unknown): Record<string, unknown> {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw new Error("content action input must be an object");
  }
  return input as Record<string, unknown>;
}

function target(document: Document, input: Record<string, unknown>): Element {
  if (typeof input.cssPath !== "string" || input.cssPath.length === 0 || input.cssPath.length > 2048) {
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
    onMessage: {
      addListener(listener: (message: unknown) => unknown): void;
    };
  };
};

declare const browser: ContentBrowserApi | undefined;

if (typeof browser !== "undefined") {
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
