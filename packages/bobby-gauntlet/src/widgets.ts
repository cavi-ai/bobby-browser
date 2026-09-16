import { element } from "./components.js";

export function combobox(
  document: Document,
  options: {
    label: string;
    placeholder?: string;
    onQuery: (value: string) => Promise<ReadonlyArray<{ id: string; label: string }>>;
    onSelect: (id: string, label: string) => void;
    onSearch?: (value: string) => void;
  },
): HTMLElement {
  const root = element(document, "div", { className: "combobox", ariaLabel: options.label });
  const input = element(document, "input", { ariaLabel: options.label });
  input.type = "search";
  input.placeholder = options.placeholder ?? "";
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-autocomplete", "list");
  input.setAttribute("aria-expanded", "false");
  const list = element(document, "ul", { className: "combobox-list", ariaLabel: `${options.label} suggestions` });
  list.setAttribute("role", "listbox");
  list.hidden = true;
  const search = element(document, "button", { text: "Search" });
  search.type = "button";
  root.append(input, search, list);
  let suggestionGeneration = 0;

  const render = async () => {
    const generation = ++suggestionGeneration;
    const items = await options.onQuery(input.value);
    if (generation !== suggestionGeneration) return;
    list.replaceChildren();
    for (const item of items) {
      const option = element(document, "li", { text: item.label });
      option.setAttribute("role", "option");
      option.dataset.id = item.id;
      option.tabIndex = 0;
      option.addEventListener("click", () => {
        input.value = item.label;
        list.hidden = true;
        input.setAttribute("aria-expanded", "false");
        options.onSelect(item.id, item.label);
      });
      list.append(option);
    }
    list.hidden = items.length === 0;
    input.setAttribute("aria-expanded", items.length === 0 ? "false" : "true");
  };

  input.addEventListener("input", () => { void render(); });
  search.addEventListener("click", () => {
    options.onSearch?.(input.value);
    void render();
  });
  return root;
}

export function listbox(
  document: Document,
  options: {
    label: string;
    value: string;
    choices: ReadonlyArray<readonly [string, string]>;
    onChange: (value: string) => void;
  },
): HTMLElement {
  const root = element(document, "div", { className: "listbox-field" });
  const button = element(document, "button", { text: options.choices.find(([value]) => value === options.value)?.[1] ?? options.value, ariaLabel: options.label });
  button.type = "button";
  button.setAttribute("role", "combobox");
  button.setAttribute("aria-expanded", "false");
  const list = element(document, "ul", { className: "listbox-options", ariaLabel: options.label });
  list.setAttribute("role", "listbox");
  list.hidden = true;
  for (const [value, label] of options.choices) {
    const option = element(document, "li", { text: label });
    option.setAttribute("role", "option");
    option.dataset.value = value;
    option.tabIndex = 0;
    option.addEventListener("click", () => {
      button.textContent = label;
      list.hidden = true;
      button.setAttribute("aria-expanded", "false");
      options.onChange(value);
    });
    list.append(option);
  }
  button.addEventListener("click", () => {
    list.hidden = !list.hidden;
    button.setAttribute("aria-expanded", list.hidden ? "false" : "true");
  });
  root.append(button, list);
  return root;
}
