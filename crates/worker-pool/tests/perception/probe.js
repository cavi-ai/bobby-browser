// Perception ground truth read from the DOM at screenshot time: visible,
// unoccluded interactables, single-line text runs, and the challenge class
// named by deterministic markers. Boxes are CSS pixels in the viewport.
// Text-entry values are never read.
(() => {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const clean = (text) => (text || "").replace(/\s+/g, " ").trim();
  const box = (r) => ({ x: r.left, y: r.top, w: r.width, h: r.height });
  const inside = (r) => r.left >= 0 && r.top >= 0 && r.right <= vw && r.bottom <= vh;
  const shown = (el) => {
    for (let e = el; e; e = e.parentElement) {
      const s = getComputedStyle(e);
      if (s.display === "none" || s.visibility !== "visible" || Number(s.opacity) === 0) return false;
    }
    return true;
  };
  const topmost = (el, r) => {
    const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return !!hit && (hit === el || el.contains(hit) || hit.contains(el));
  };

  const INPUT_ROLES = {
    checkbox: "checkbox", radio: "radio", button: "button", submit: "button",
    reset: "button", range: "slider", search: "searchbox",
  };
  const roleOf = (el) => {
    const explicit = clean(el.getAttribute("role")).split(" ")[0];
    if (explicit) return explicit;
    switch (el.tagName) {
      case "A": return el.hasAttribute("href") ? "link" : null;
      case "BUTTON": case "SUMMARY": return "button";
      case "SELECT": return "combobox";
      case "TEXTAREA": return "textbox";
      case "INPUT": {
        const type = (el.getAttribute("type") || "text").toLowerCase();
        return type === "hidden" ? null : INPUT_ROLES[type] || "textbox";
      }
      default: return null;
    }
  };
  const INTERACTIVE = new Set([
    "button", "link", "checkbox", "radio", "switch", "tab", "menuitem", "option",
    "combobox", "textbox", "searchbox", "slider", "spinbutton",
  ]);
  const isField = (el) => ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName);
  const nameOf = (el) => {
    const aria = clean(el.getAttribute("aria-label"));
    if (aria) return [aria, "aria"];
    const labelledBy = clean(el.getAttribute("aria-labelledby"));
    if (labelledBy) {
      const text = clean(labelledBy.split(" ").map((id) => document.getElementById(id)?.innerText || "").join(" "));
      if (text) return [text, "labelledby"];
    }
    if (el.labels && el.labels.length) {
      const text = clean([...el.labels].map((label) => label.innerText).join(" "));
      if (text) return [text, "label"];
    }
    const type = (el.getAttribute("type") || "").toLowerCase();
    if (el.tagName === "INPUT" && ["button", "submit", "reset"].includes(type)) {
      return [clean(el.value), "text"];
    }
    if (!isField(el)) {
      const text = clean(el.innerText);
      if (text) return [text, "text"];
    }
    const title = clean(el.getAttribute("title"));
    if (title) return [title, "title"];
    return [clean(el.getAttribute("placeholder")), "placeholder"];
  };

  const elements = [];
  const interactive = document.querySelectorAll(
    "a[href], button, input, select, textarea, summary, [role], [tabindex]:not([tabindex='-1'])"
  );
  for (const el of interactive) {
    const role = roleOf(el);
    if (!INTERACTIVE.has(role)) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4 || !inside(r) || !shown(el) || !topmost(el, r)) continue;
    const [name, nameSource] = nameOf(el);
    elements.push({ role, name, nameSource, bbox: box(r) });
    if (elements.length >= 200) break;
  }

  const TRANSFORMS = {
    none: (t) => t,
    uppercase: (t) => t.toUpperCase(),
    lowercase: (t) => t.toLowerCase(),
  };
  const textRuns = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  const range = document.createRange();
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const parent = node.parentElement;
    const raw = clean(node.textContent);
    if (!raw || !parent || parent.closest("script, style, noscript, select, textarea")) continue;
    const style = getComputedStyle(parent);
    const transform = TRANSFORMS[style.textTransform];
    if (!transform || !shown(parent)) continue;
    range.selectNodeContents(node);
    const rects = [...range.getClientRects()].filter((r) => r.width > 0 && r.height > 0);
    if (rects.length !== 1 || !inside(rects[0]) || !topmost(parent, rects[0])) continue;
    textRuns.push({
      text: transform(raw),
      bbox: box(rects[0]),
      fontSizePx: parseFloat(style.fontSize),
    });
    if (textRuns.length >= 400) break;
  }

  const visible = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && shown(el);
  };
  const frames = [...document.querySelectorAll("iframe")].filter(visible).map((f) => f.getAttribute("src") || "");
  const anchors = frames.filter((src) => /\/recaptcha\/(api2|enterprise)\/anchor/.test(src));
  const challenge = (() => {
    if (frames.some((src) => /\/recaptcha\/(api2|enterprise)\/bframe/.test(src))) {
      return { kind: "imageGridCaptcha", blocking: true };
    }
    if (anchors.some((src) => !/[?&]size=invisible/.test(src))) {
      return { kind: "recaptchaV2Checkbox", blocking: true };
    }
    if (anchors.length) return { kind: "recaptchaV3", blocking: false };
    const otp = [...document.querySelectorAll("input[autocomplete='one-time-code']")].filter(visible);
    if (otp.length) return { kind: "mfaCodeEntry", blocking: true };
    const marked = (el) => /captcha/i.test([el.id, el.className, el.getAttribute("name"), el.getAttribute("alt")].join(" "));
    const image = [...document.querySelectorAll("img, svg, canvas")].filter((el) => visible(el) && marked(el));
    const entry = [...document.querySelectorAll("input")].filter((el) => visible(el) && marked(el));
    if (image.length && entry.length) return { kind: "textCaptcha", blocking: true };
    return { kind: "none", blocking: false };
  })();

  return {
    viewport: { w: vw, h: vh, dpr: window.devicePixelRatio },
    elements,
    textRuns,
    challenge,
  };
})()
