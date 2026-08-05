export function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  options: { className?: string; text?: string; ariaLabel?: string } = {},
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (options.className !== undefined) node.className = options.className;
  if (options.text !== undefined) node.textContent = options.text;
  if (options.ariaLabel !== undefined) node.setAttribute("aria-label", options.ariaLabel);
  return node;
}

export function status(document: Document, message: string): HTMLElement {
  const node = element(document, "p", { className: "notice", text: message });
  node.setAttribute("role", "status");
  return node;
}

export function pageHeader(document: Document, eyebrow: string, title: string, description: string): HTMLElement {
  const header = element(document, "header", { className: "page-header" });
  header.append(
    element(document, "p", { className: "eyebrow", text: eyebrow }),
    element(document, "h1", { text: title }),
    element(document, "p", { className: "lede", text: description }),
  );
  return header;
}
