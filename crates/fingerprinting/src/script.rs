//! Init / probe JavaScript emitted from a [`crate::FingerprintSession`].

use crate::error::FingerprintApplyError;
use crate::FingerprintSession;

/// Placeholder replaced with session JSON inside [`INIT_SCRIPT_TEMPLATE`].
pub const PROFILE_PLACEHOLDER: &str = "__BOBBY_FP_PROFILE__";

/// Engine-agnostic apply script template. Hosts and the companion extension
/// must use this exact source so masks cannot drift.
pub const INIT_SCRIPT_TEMPLATE: &str = r#"(function() {
  "use strict";
  var APPLIED = Symbol.for("bobby.fp.applied");
  if (globalThis[APPLIED]) return;
  const isPage = typeof window !== "undefined" && typeof document !== "undefined";
  if (!isPage) return;
  Object.defineProperty(globalThis, APPLIED, {
    value: true,
    configurable: false,
    enumerable: false,
    writable: false,
  });
  const P = __BOBBY_FP_PROFILE__;
  const UNMASKED_VENDOR_WEBGL = 0x9245;
  const UNMASKED_RENDERER_WEBGL = 0x9246;

  const nativeFns = new WeakSet();
  function cloak(fn) { try { nativeFns.add(fn); } catch (_) {} return fn; }
  const _toString = Function.prototype.toString;
  Function.prototype.toString = cloak(function () {
    if (nativeFns.has(this)) {
      const n = this.name ? this.name : "";
      return "function " + n + "() { [native code] }";
    }
    return _toString.call(this);
  });

  try {
    Object.defineProperty(Navigator.prototype, "webdriver", {
      get: cloak(function webdriver() { return false; }),
      configurable: true,
    });
  } catch (_) {}

  try {
    Object.defineProperty(Navigator.prototype, "userAgent", {
      get: cloak(function userAgent() { return P.userAgent; }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "platform", {
      get: cloak(function platform() { return P.platform; }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "language", {
      get: cloak(function language() { return P.locale; }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "languages", {
      get: cloak(function languages() { return Object.freeze([P.locale, P.locale.split("-")[0]]); }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "hardwareConcurrency", {
      get: cloak(function hardwareConcurrency() { return P.hardwareConcurrency; }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "deviceMemory", {
      get: cloak(function deviceMemory() { return P.deviceMemory; }),
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
      get: cloak(function maxTouchPoints() { return P.maxTouchPoints; }),
      configurable: true,
    });
  } catch (_) {}

  if (P.injectChrome) {
    try {
      if (!globalThis.chrome) {
        globalThis.chrome = {};
      }
      globalThis.chrome.runtime = {
        id: undefined,
        connect: cloak(function connect() {}),
        sendMessage: cloak(function sendMessage() {}),
      };
      if (!globalThis.chrome.app) {
        globalThis.chrome.app = {
          isInstalled: false,
          InstallState: { DISABLED: "disabled", INSTALLED: "installed", NOT_INSTALLED: "not_installed" },
          RunningState: { CANNOT_RUN: "cannot_run", READY_TO_RUN: "ready_to_run", RUNNING: "running" },
          getDetails: cloak(function getDetails() { return null; }),
          getIsInstalled: cloak(function getIsInstalled() { return false; }),
        };
      }
    } catch (_) {}
  }

  let availW = P.screenResolution.availableWidth;
  let availH = P.screenResolution.availableHeight;
  const scrW = P.screenResolution.width;
  const scrH = P.screenResolution.height;
  if (availH >= scrH) availH = scrH - 40;
  if (availW >= scrW) availW = scrW;
  const screenPatch = {
    width: scrW,
    height: scrH,
    availWidth: availW,
    availHeight: availH,
    colorDepth: P.screenResolution.colorDepth,
    pixelDepth: P.screenResolution.colorDepth,
  };
  try {
    for (const [key, value] of Object.entries(screenPatch)) {
      Object.defineProperty(Screen.prototype, key, {
        get: function () { return value; },
        configurable: true,
      });
    }
    Object.defineProperty(window, "devicePixelRatio", {
      get: function () { return P.screenResolution.pixelRatio; },
      configurable: true,
    });
  } catch (_) {}

  try {
    const originalGCS = window.getComputedStyle.bind(window);
    window.getComputedStyle = cloak(function getComputedStyle(el, pseudo) {
      const style = originalGCS(el, pseudo);
      const desktopTouch = P.maxTouchPoints === 0;
      try {
        const cssText = (el && el.style && el.style.cssText) || "";
        const attr = (el && el.getAttribute && el.getAttribute("style")) || "";
        const activeText = /ActiveText/i.test(cssText + attr);
        if (!activeText && !desktopTouch) return style;
        return new Proxy(style, {
          get(target, prop, receiver) {
            if (prop === "backgroundColor" || prop === "color") {
              if (!activeText) {
                return Reflect.get(target, prop, receiver);
              }
              const v = Reflect.get(target, prop, receiver);
              if (v === "rgb(255, 0, 0)" || v === "rgba(255, 0, 0, 1)") return "rgb(0, 0, 0)";
              return v;
            }
            if (prop === "getPropertyValue") {
              return function (name) {
                const v = target.getPropertyValue(name);
                const key = String(name).toLowerCase();
                if (activeText && /color/i.test(key) && (v === "rgb(255, 0, 0)" || v === "rgba(255, 0, 0, 1)")) {
                  return "rgb(0, 0, 0)";
                }
                if (desktopTouch) {
                  if ((key === "--any-pointer" || key === "--pointer") && v === "coarse") return "fine";
                  if ((key === "--any-hover" || key === "--hover") && v === "none") return "hover";
                }
                return v;
              };
            }
            const val = Reflect.get(target, prop, receiver);
            return typeof val === "function" ? val.bind(target) : val;
          },
        });
      } catch (_) {}
      return style;
    });
  } catch (_) {}

  const fontSet = new Set(P.fontList || []);
  const FONT_GENERICS = new Set([
    "serif", "sans-serif", "monospace", "cursive", "fantasy", "system-ui",
    "ui-serif", "ui-sans-serif", "ui-monospace", "ui-rounded", "emoji", "math",
    "fangsong", "inherit", "initial", "unset", "default",
  ]);
  function normalizeFontName(name) {
    return String(name || "").replace(/^["']+|["']+$/g, "").trim();
  }
  function fontAllowed(name) {
    const n = normalizeFontName(name);
    if (!n) return true;
    if (FONT_GENERICS.has(n.toLowerCase())) return true;
    if (fontSet.size === 0) return true;
    if (fontSet.has(n)) return true;
    const lower = n.toLowerCase();
    for (const allowed of fontSet) {
      if (String(allowed).toLowerCase() === lower) return true;
    }
    return false;
  }
  function rewriteFontFamilyList(familyList) {
    const parts = String(familyList || "").split(",");
    const kept = [];
    for (let i = 0; i < parts.length; i++) {
      const raw = parts[i].trim();
      if (!raw) continue;
      if (fontAllowed(raw)) kept.push(raw);
    }
    return kept.length ? kept.join(", ") : "monospace";
  }
  function rewriteCssFont(font) {
    const s = String(font || "");
    const m = s.match(/^((?:(?:normal|italic|oblique|small-caps|bold|bolder|lighter|\d{1,4})\s+)*)(\d+(?:\.\d+)?(?:px|pt|em|rem|%))(\/\S+)?\s+(.+)$/i);
    if (!m) return rewriteFontFamilyList(s);
    return m[1] + m[2] + (m[3] || "") + " " + rewriteFontFamilyList(m[4]);
  }
  function primaryFamilies(font) {
    const s = String(font || "");
    const m = s.match(/\d+(?:\.\d+)?(?:px|pt|em|rem|%)(?:\/\S+)?\s+(.+)$/i);
    const familyPart = m ? m[1] : s;
    return familyPart.split(",").map(normalizeFontName).filter(Boolean);
  }
  try {
    if (document.fonts && document.fonts.check) {
      const originalCheck = document.fonts.check.bind(document.fonts);
      document.fonts.check = cloak(function check(font, text) {
        if (fontSet.size > 0) {
          const families = primaryFamilies(font);
          for (let i = 0; i < families.length; i++) {
            if (!fontAllowed(families[i])) return false;
          }
          // Allowlisted only — defer to real install state (avoid claiming missing Windows fonts).
          return originalCheck(font, text);
        }
        return originalCheck(font, text);
      });
    }
  } catch (_) {}
  try {
    function patchMeasureText(proto) {
      if (!proto || !proto.measureText) return;
      const original = proto.measureText;
      proto.measureText = cloak(function measureText(text) {
        const prev = this.font;
        try {
          const next = rewriteCssFont(prev);
          if (next !== prev) this.font = next;
          return original.call(this, text);
        } finally {
          try { this.font = prev; } catch (_) {}
        }
      });
    }
    function patchTextDraw(proto) {
      if (!proto) return;
      ["fillText", "strokeText"].forEach(function (method) {
        if (typeof proto[method] !== "function") return;
        const original = proto[method];
        proto[method] = cloak(function () {
          const prev = this.font;
          try {
            const next = rewriteCssFont(prev);
            if (next !== prev) this.font = next;
            return original.apply(this, arguments);
          } finally {
            try { this.font = prev; } catch (_) {}
          }
        });
      });
    }
    if (typeof CanvasRenderingContext2D !== "undefined") {
      patchMeasureText(CanvasRenderingContext2D.prototype);
      patchTextDraw(CanvasRenderingContext2D.prototype);
    }
    if (typeof OffscreenCanvasRenderingContext2D !== "undefined") {
      patchMeasureText(OffscreenCanvasRenderingContext2D.prototype);
      patchTextDraw(OffscreenCanvasRenderingContext2D.prototype);
    }
  } catch (_) {}
  try {
    if (document.fonts && document.fonts.load) {
      const originalLoad = document.fonts.load.bind(document.fonts);
      document.fonts.load = cloak(function load(font, text) {
        if (fontSet.size > 0) {
          const families = primaryFamilies(font);
          for (let i = 0; i < families.length; i++) {
            if (!fontAllowed(families[i])) return Promise.resolve([]);
          }
        }
        return originalLoad(font, text);
      });
    }
  } catch (_) {}
  try {
    if (typeof FontFace !== "undefined" && FontFace.prototype && FontFace.prototype.load) {
      const originalFontFaceLoad = FontFace.prototype.load;
      FontFace.prototype.load = cloak(function load() {
        const self = this;
        const family = normalizeFontName(self && self.family);
        if (fontSet.size > 0 && family && !fontAllowed(family)) {
          return Promise.reject(
            new DOMException("A network error occurred.", "NetworkError")
          );
        }
        return originalFontFaceLoad.apply(self, arguments).catch(function (err) {
          // Allowlisted Windows fonts may be absent on the host — fulfill so
          // collectors classify the persona OS instead of leaking "no fonts".
          if (fontSet.size > 0 && family && fontAllowed(family)) {
            return self;
          }
          return Promise.reject(err);
        });
      });
    }
  } catch (_) {}
  try {
    function withSanitizedInlineFont(el, fn) {
      if (!el || !el.style) return fn();
      const style = el.style;
      const prevFamily = style.fontFamily;
      const prevFont = style.font;
      let computedFamily = "";
      try {
        if (typeof getComputedStyle === "function") {
          computedFamily = getComputedStyle(el).fontFamily || "";
        }
      } catch (_) {}
      const sourceFamily = prevFamily || computedFamily;
      let mode = null;
      try {
        if (sourceFamily) {
          const next = rewriteFontFamilyList(sourceFamily);
          if (next !== sourceFamily) {
            style.fontFamily = next;
            mode = "family";
          }
        } else if (prevFont) {
          const next = rewriteCssFont(prevFont);
          if (next !== prevFont) {
            style.font = next;
            mode = "font";
          }
        }
        return fn();
      } finally {
        if (mode === "family") style.fontFamily = prevFamily;
        if (mode === "font") style.font = prevFont;
      }
    }
    function patchBox(proto, prop) {
      const desc = Object.getOwnPropertyDescriptor(proto, prop);
      if (!desc || typeof desc.get !== "function") return;
      Object.defineProperty(proto, prop, {
        configurable: true,
        enumerable: desc.enumerable,
        get: cloak(function () {
          const self = this;
          return withSanitizedInlineFont(self, function () {
            return desc.get.call(self);
          });
        }),
      });
    }
    patchBox(HTMLElement.prototype, "offsetWidth");
    patchBox(HTMLElement.prototype, "offsetHeight");
    patchBox(HTMLElement.prototype, "clientWidth");
    patchBox(HTMLElement.prototype, "clientHeight");
    patchBox(Element.prototype, "scrollWidth");
    patchBox(Element.prototype, "scrollHeight");
    const originalGbr = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = cloak(function getBoundingClientRect() {
      const self = this;
      return withSanitizedInlineFont(self, function () {
        return originalGbr.call(self);
      });
    });
    if (typeof Range !== "undefined" && Range.prototype) {
      const originalRangeGbr = Range.prototype.getBoundingClientRect;
      Range.prototype.getBoundingClientRect = cloak(function getBoundingClientRect() {
        const self = this;
        let el = null;
        try {
          el = self.commonAncestorContainer;
          if (el && el.nodeType === 3) el = el.parentElement;
        } catch (_) {}
        if (el && el.style) {
          return withSanitizedInlineFont(el, function () {
            return originalRangeGbr.call(self);
          });
        }
        return originalRangeGbr.call(self);
      });
      const originalRangeGcr = Range.prototype.getClientRects;
      Range.prototype.getClientRects = cloak(function getClientRects() {
        const self = this;
        let el = null;
        try {
          el = self.commonAncestorContainer;
          if (el && el.nodeType === 3) el = el.parentElement;
        } catch (_) {}
        if (el && el.style) {
          return withSanitizedInlineFont(el, function () {
            return originalRangeGcr.call(self);
          });
        }
        return originalRangeGcr.call(self);
      });
    }
  } catch (_) {}

  const canvasNoise = P.canvasNoiseAmplitude | 0;
  function digestSeed(hex, fallback) {
    if (typeof hex === "string" && hex.length >= 8) {
      return parseInt(hex.slice(0, 8), 16) >>> 0;
    }
    return fallback >>> 0;
  }
  const canvasSeed = digestSeed(P.canvasHash, P.sessionSeed >>> 0);
  const audioMixSeed = digestSeed(P.audioHash, (P.sessionSeed >>> 0) ^ 0xa0d10);
  function mix(n) {
    n = (n + canvasSeed) | 0;
    n = Math.imul(n ^ (n >>> 16), 2246822507);
    n = Math.imul(n ^ (n >>> 13), 3266489909);
    return (n ^ (n >>> 16)) >>> 0;
  }
  function audioMix(n) {
    n = (n + audioMixSeed) | 0;
    n = Math.imul(n ^ (n >>> 16), 2246822507);
    n = Math.imul(n ^ (n >>> 13), 3266489909);
    return (n ^ (n >>> 16)) >>> 0;
  }
  function noiseImageData(img) {
    if (!img || !img.data || canvasNoise <= 0) return img;
    for (let i = 0; i < img.data.length; i += 4) {
      const n = mix(i) % (canvasNoise + 1);
      img.data[i] = (img.data[i] + n) & 255;
    }
    return img;
  }
  function patchCanvasProto(proto) {
    if (!proto) return;
    const originalToDataURL = proto.toDataURL;
    proto.toDataURL = cloak(function toDataURL() {
      try {
        const ctx = this.getContext && this.getContext("2d");
        if (ctx && canvasNoise > 0 && this.width > 0 && this.height > 0) {
          const clone = document.createElement("canvas");
          clone.width = this.width;
          clone.height = this.height;
          const cloneCtx = clone.getContext("2d");
          if (cloneCtx) {
            cloneCtx.drawImage(this, 0, 0);
            const w = Math.min(clone.width, 16);
            const h = Math.min(clone.height, 16);
            const img = cloneCtx.getImageData(0, 0, w, h);
            noiseImageData(img);
            cloneCtx.putImageData(img, 0, 0);
            return originalToDataURL.apply(clone, arguments);
          }
        }
      } catch (_) {}
      return originalToDataURL.apply(this, arguments);
    });
    if (typeof CanvasRenderingContext2D !== "undefined") {
      const originalGetImageData = CanvasRenderingContext2D.prototype.getImageData;
      CanvasRenderingContext2D.prototype.getImageData = cloak(function getImageData() {
        const img = originalGetImageData.apply(this, arguments);
        try {
          return noiseImageData(img);
        } catch (_) {
          return img;
        }
      });
    }
  }
  try {
    patchCanvasProto(HTMLCanvasElement.prototype);
  } catch (_) {}

  function patchWebGl(proto) {
    if (!proto || !proto.getParameter) return;
    const original = proto.getParameter;
    const maxTex = (P.webgl && P.webgl.maxTextureSize) || 16384;
    const MAX_TEXTURE_SIZE = proto.MAX_TEXTURE_SIZE || 0x0D33;
    const MAX_RENDERBUFFER_SIZE = proto.MAX_RENDERBUFFER_SIZE || 0x84E8;
    const MAX_VERTEX_ATTRIBS = proto.MAX_VERTEX_ATTRIBS || 0x8869;
    const MAX_VIEWPORT_DIMS = proto.MAX_VIEWPORT_DIMS || 0x0D3A;
    const ALIASED_LINE_WIDTH_RANGE = proto.ALIASED_LINE_WIDTH_RANGE || 0x846E;
    const ALIASED_POINT_SIZE_RANGE = proto.ALIASED_POINT_SIZE_RANGE || 0x846D;
    proto.getParameter = cloak(function getParameter(param) {
      if (param === UNMASKED_VENDOR_WEBGL) return P.webgl.vendor;
      if (param === UNMASKED_RENDERER_WEBGL) return P.webgl.renderer;
      if (param === MAX_TEXTURE_SIZE) return maxTex;
      if (param === MAX_RENDERBUFFER_SIZE) return maxTex;
      if (param === MAX_VERTEX_ATTRIBS) return 16;
      if (param === MAX_VIEWPORT_DIMS) return new Int32Array([maxTex, maxTex]);
      if (param === ALIASED_LINE_WIDTH_RANGE) return new Float32Array([1, 1]);
      if (param === ALIASED_POINT_SIZE_RANGE) return new Float32Array([1, 1024]);
      return original.apply(this, arguments);
    });
  }
  try {
    patchWebGl(WebGLRenderingContext && WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== "undefined") {
      patchWebGl(WebGL2RenderingContext.prototype);
    }
  } catch (_) {}

  const audioScale = Number(P.audioNoiseScale) || 1e-7;
  try {
    const OriginalOffline = window.OfflineAudioContext || window.webkitOfflineAudioContext;
    if (OriginalOffline) {
      const OriginalProto = OriginalOffline.prototype;
      const originalStart = OriginalProto.startRendering;
      if (originalStart) {
        OriginalProto.startRendering = function () {
          const promise = originalStart.apply(this, arguments);
          return Promise.resolve(promise).then(function (buffer) {
            try {
              for (let c = 0; c < buffer.numberOfChannels; c++) {
                const data = buffer.getChannelData(c);
                for (let i = 0; i < data.length; i++) {
                  data[i] = data[i] + ((audioMix(i + c * 1024) / 0xffffffff) - 0.5) * audioScale;
                }
              }
            } catch (_) {}
            return buffer;
          });
        };
      }
    }
  } catch (_) {}

  function patchRtcConstructor(name) {
    const Original = window[name];
    if (!Original) return;
    const Wrapped = cloak(function Wrapped(config) {
      const cfg = Object.assign({}, config || {});
      cfg.iceTransportPolicy = "relay";
      return new Original(cfg);
    });
    Wrapped.prototype = Original.prototype;
    Object.setPrototypeOf(Wrapped, Original);
    try {
      window[name] = Wrapped;
    } catch (_) {}
  }
  try {
    patchRtcConstructor("RTCPeerConnection");
    patchRtcConstructor("webkitRTCPeerConnection");
    patchRtcConstructor("mozRTCPeerConnection");
  } catch (_) {}

  try {
    if (navigator.mediaDevices) {
      const seed = (P.sessionSeed >>> 0).toString(16).padStart(8, "0");
      const stableDevices = [
        { deviceId: seed + "-audio-in-default", groupId: seed + "-grp-audio", kind: "audioinput", label: "" },
        { deviceId: seed + "-audio-in-comm", groupId: seed + "-grp-audio", kind: "audioinput", label: "" },
        { deviceId: seed + "-audio-out-default", groupId: seed + "-grp-audio-out", kind: "audiooutput", label: "" },
        { deviceId: seed + "-video-in-default", groupId: seed + "-grp-video", kind: "videoinput", label: "" },
      ];
      navigator.mediaDevices.enumerateDevices = cloak(function enumerateDevices() {
        return Promise.resolve(stableDevices.map(function (d) { return Object.assign({}, d); }));
      });
      const denied = function () {
        return Promise.reject(new DOMException("Permission denied", "NotAllowedError"));
      };
      navigator.mediaDevices.getUserMedia = denied;
      navigator.mediaDevices.getDisplayMedia = denied;
    }
  } catch (_) {}

  try {
    if (window.speechSynthesis && window.speechSynthesis.getVoices) {
      const makeVoice = function (name, lang, def) {
        return { name: name, lang: lang, default: !!def, localService: true, voiceURI: name };
      };
      let voices;
      if (P.platform === "Win32") {
        voices = [
          makeVoice("Microsoft David Desktop - English (United States)", "en-US", true),
          makeVoice("Microsoft Zira Desktop - English (United States)", "en-US", false),
        ];
      } else if (P.platform === "MacIntel") {
        voices = [makeVoice("Samantha", "en-US", true)];
      } else {
        voices = [makeVoice("Google US English", "en-US", true)];
      }
      window.speechSynthesis.getVoices = function () { return voices.slice(); };
      try {
        setTimeout(function () {
          try {
            window.speechSynthesis.dispatchEvent(new Event("voiceschanged"));
          } catch (_) {}
        }, 0);
      } catch (_) {}
    }
  } catch (_) {}

  try {
    if (navigator.getBattery) {
      const batteryObj = {
        charging: true,
        chargingTime: 0,
        dischargingTime: Infinity,
        level: 1,
        addEventListener: function () {},
        removeEventListener: function () {},
        dispatchEvent: function () { return false; },
        onchargingchange: null,
        onchargingtimechange: null,
        ondischargingtimechange: null,
        onlevelchange: null,
      };
      navigator.getBattery = cloak(function getBattery() { return Promise.resolve(batteryObj); });
    }
  } catch (_) {}

  try {
    const conn = {
      effectiveType: "4g",
      rtt: 50,
      downlink: 10,
      saveData: false,
      addEventListener: function () {},
      removeEventListener: function () {},
    };
    Object.defineProperty(Navigator.prototype, "connection", {
      get: function () { return conn; },
      configurable: true,
    });
  } catch (_) {}

  try {
    const hints = P.clientHints || {};
    const brands = hints.brands || [];
    const fullVersionList = hints.fullVersionList || [];
    const uaData = {
      brands: brands.slice(),
      mobile: !!hints.mobile,
      platform: hints.platform || "",
      getHighEntropyValues: function (hintsList) {
        const out = {
          brands: brands.slice(),
          mobile: !!hints.mobile,
          platform: hints.platform || "",
        };
        const wanted = Array.isArray(hintsList) ? hintsList : [];
        for (let i = 0; i < wanted.length; i++) {
          const key = wanted[i];
          if (key === "architecture") out.architecture = hints.architecture || "";
          if (key === "bitness") out.bitness = hints.bitness || "";
          if (key === "model") out.model = hints.model || "";
          if (key === "platformVersion") out.platformVersion = hints.platformVersion || "";
          if (key === "uaFullVersion" || key === "fullVersion") out.uaFullVersion = hints.fullVersion || "";
          if (key === "fullVersionList") out.fullVersionList = fullVersionList.slice();
          if (key === "wow64") out.wow64 = false;
          if (key === "formFactors") out.formFactors = ["Desktop"];
        }
        return Promise.resolve(out);
      },
      toJSON: function () {
        return { brands: brands.slice(), mobile: !!hints.mobile, platform: hints.platform || "" };
      },
    };
    Object.defineProperty(Navigator.prototype, "userAgentData", {
      get: cloak(function userAgentData() { return uaData; }),
      configurable: true,
    });
    try {
      const nativeUad = navigator.userAgentData;
      try { delete navigator.userAgentData; } catch (_) {}
      try {
        Object.defineProperty(navigator, "userAgentData", {
          get: cloak(function userAgentData() { return uaData; }),
          configurable: true,
          enumerable: true,
        });
      } catch (_) {}
      // Chrome may keep a non-configurable own getter — patch the native object.
      if (navigator.userAgentData !== uaData && nativeUad) {
        try {
          if (typeof nativeUad.getHighEntropyValues === "function") {
            nativeUad.getHighEntropyValues = cloak(function getHighEntropyValues(hintsList) {
              return Promise.resolve(uaData.getHighEntropyValues(hintsList));
            });
          }
          Object.defineProperty(nativeUad, "brands", {
            get: cloak(function brands() { return brands.slice(); }),
            configurable: true,
          });
          Object.defineProperty(nativeUad, "platform", {
            get: cloak(function platform() { return hints.platform || ""; }),
            configurable: true,
          });
          Object.defineProperty(nativeUad, "mobile", {
            get: cloak(function mobile() { return !!hints.mobile; }),
            configurable: true,
          });
        } catch (_) {}
      }
    } catch (_) {}
  } catch (_) {}

  try {
    const makePlugin = function (name, filename, description) {
      const plugin = { name: name, filename: filename, description: description, length: 1 };
      plugin[0] = { type: "application/pdf", suffixes: "pdf", description: description };
      return plugin;
    };
    const plugins = [
      makePlugin("PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
      makePlugin("Chrome PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
      makePlugin("Chromium PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
      makePlugin("Microsoft Edge PDF Viewer", "internal-pdf-viewer", "Portable Document Format"),
      makePlugin("WebKit built-in PDF", "internal-pdf-viewer", "Portable Document Format"),
    ];
    plugins.item = function (i) { return this[i] || null; };
    plugins.namedItem = function (name) {
      for (let i = 0; i < this.length; i++) if (this[i].name === name) return this[i];
      return null;
    };
    plugins.refresh = function () {};
    Object.defineProperty(Navigator.prototype, "plugins", {
      get: function () { return plugins; },
      configurable: true,
    });
    const mimeTypes = [{ type: "application/pdf", suffixes: "pdf", description: "Portable Document Format" }];
    mimeTypes.item = function (i) { return this[i] || null; };
    mimeTypes.namedItem = function (name) {
      for (let i = 0; i < this.length; i++) if (this[i].type === name) return this[i];
      return null;
    };
    Object.defineProperty(Navigator.prototype, "mimeTypes", {
      get: function () { return mimeTypes; },
      configurable: true,
    });
  } catch (_) {}

  try {
    const originalQuery = navigator.permissions && navigator.permissions.query
      ? navigator.permissions.query.bind(navigator.permissions)
      : null;
    if (originalQuery) {
      navigator.permissions.query = function (desc) {
        const name = desc && desc.name;
        if (name === "notifications" || name === "push") {
          return Promise.resolve({ state: Notification.permission || "default", onchange: null });
        }
        if (name === "camera" || name === "microphone" || name === "geolocation") {
          return Promise.resolve({ state: "prompt", onchange: null });
        }
        return originalQuery(desc);
      };
    }
  } catch (_) {}

  try {
    if (typeof OffscreenCanvas !== "undefined") {
      const proto = OffscreenCanvas.prototype;
      if (proto.convertToBlob) {
        const originalConvert = proto.convertToBlob;
        proto.convertToBlob = function () {
          const self = this;
          return Promise.resolve().then(function () {
            try {
              const ctx = self.getContext && self.getContext("2d");
              if (ctx && canvasNoise > 0 && self.width > 0 && self.height > 0) {
                const w = Math.min(self.width, 16);
                const h = Math.min(self.height, 16);
                const img = ctx.getImageData(0, 0, w, h);
                noiseImageData(img);
                ctx.putImageData(img, 0, 0);
              }
            } catch (_) {}
            return originalConvert.apply(self, arguments);
          });
        };
      }
    }
    if (HTMLCanvasElement.prototype.toBlob) {
      const originalToBlob = HTMLCanvasElement.prototype.toBlob;
      HTMLCanvasElement.prototype.toBlob = function (callback) {
        try {
          const dataUrl = this.toDataURL.apply(this, Array.prototype.slice.call(arguments, 2));
          const parts = dataUrl.split(",");
          const bin = atob(parts[1] || "");
          const arr = new Uint8Array(bin.length);
          for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
          callback(new Blob([arr], { type: (parts[0].match(/:(.*?);/) || [])[1] || "image/png" }));
        } catch (_) {
          return originalToBlob.apply(this, arguments);
        }
      };
    }
  } catch (_) {}

  try {
    const tz = P.timezoneId;
    if (tz && typeof Intl !== "undefined" && Intl.DateTimeFormat) {
      const OriginalDTF = Intl.DateTimeFormat;
      Intl.DateTimeFormat = function (locales, options) {
        const opts = Object.assign({}, options || {});
        if (!opts.timeZone) opts.timeZone = tz;
        return new OriginalDTF(locales, opts);
      };
      Intl.DateTimeFormat.prototype = OriginalDTF.prototype;
      Intl.DateTimeFormat.supportedLocalesOf = OriginalDTF.supportedLocalesOf.bind(OriginalDTF);
    }
  } catch (_) {}

  try {
    if (P.userAgent && P.userAgent.indexOf("Chrome/") >= 0) {
      const derivedAppVersion = P.userAgent.indexOf("Mozilla/") === 0
        ? P.userAgent.slice(8)
        : P.userAgent;
      Object.defineProperty(Navigator.prototype, "vendor", {
        get: cloak(function vendor() { return "Google Inc."; }),
        configurable: true,
      });
      Object.defineProperty(Navigator.prototype, "product", {
        get: cloak(function product() { return "Gecko"; }),
        configurable: true,
      });
      Object.defineProperty(Navigator.prototype, "productSub", {
        get: cloak(function productSub() { return "20030107"; }),
        configurable: true,
      });
      Object.defineProperty(Navigator.prototype, "appVersion", {
        get: cloak(function appVersion() { return derivedAppVersion; }),
        configurable: true,
      });
    }
  } catch (_) {}

  try {
    Object.defineProperty(Navigator.prototype, "pdfViewerEnabled", {
      get: cloak(function pdfViewerEnabled() { return true; }),
      configurable: true,
    });
  } catch (_) {}

  try {
    if (P.maxTouchPoints === 0) {
      try {
        const originalCreateEvent = Document.prototype.createEvent;
        Document.prototype.createEvent = cloak(function createEvent(type) {
          if (String(type) === "TouchEvent" || String(type).toLowerCase() === "touchevent") {
            throw new DOMException("NOT_SUPPORTED_ERR", "NotSupportedError");
          }
          return originalCreateEvent.call(this, type);
        });
      } catch (_) {}
      try { delete window.ontouchstart; } catch (_) {}
      try { delete Document.prototype.ontouchstart; } catch (_) {}
    }
    if (P.maxTouchPoints === 0 && window.matchMedia) {
      const originalMatchMedia = window.matchMedia.bind(window);
      const desktopMediaResult = function (query, matches) {
        return {
          matches: !!matches,
          media: query,
          onchange: null,
          addListener: function () {},
          removeListener: function () {},
          addEventListener: function () {},
          removeEventListener: function () {},
          dispatchEvent: function () { return false; },
        };
      };
      window.matchMedia = cloak(function matchMedia(query) {
        const q = String(query).toLowerCase().replace(/\s/g, "");
        if (q.includes("pointer:coarse") || q.includes("any-pointer:coarse")) {
          return desktopMediaResult(query, false);
        }
        if (q.includes("pointer:fine") || q.includes("any-pointer:fine")) {
          return desktopMediaResult(query, true);
        }
        if (q.includes("any-hover:none") || q === "(hover:none)" || q.endsWith("hover:none)")) {
          return desktopMediaResult(query, false);
        }
        if (q.includes("any-hover:hover") || q === "(hover:hover)" || q.endsWith("hover:hover)")) {
          return desktopMediaResult(query, true);
        }
        return originalMatchMedia(query);
      });
    }
  } catch (_) {}

  try {
    const ow = P.screenResolution.width;
    const oh = P.screenResolution.height;
    const iw = P.screenResolution.availableWidth || ow;
    const ih = P.screenResolution.availableHeight || oh;
    Object.defineProperty(window, "outerWidth", {
      get: cloak(function outerWidth() { return ow; }),
      configurable: true,
    });
    Object.defineProperty(window, "outerHeight", {
      get: cloak(function outerHeight() { return oh; }),
      configurable: true,
    });
    Object.defineProperty(window, "innerWidth", {
      get: cloak(function innerWidth() { return iw; }),
      configurable: true,
    });
    Object.defineProperty(window, "innerHeight", {
      get: cloak(function innerHeight() { return ih; }),
      configurable: true,
    });
  } catch (_) {}

  const workerProfile = {
    userAgent: P.userAgent,
    platform: P.platform,
    locale: P.locale,
    hardwareConcurrency: P.hardwareConcurrency,
    deviceMemory: P.deviceMemory,
    maxTouchPoints: P.maxTouchPoints,
    timezoneId: P.timezoneId,
    webgl: {
      vendor: (P.webgl && P.webgl.vendor) || "",
      renderer: (P.webgl && P.webgl.renderer) || "",
      maxTextureSize: (P.webgl && P.webgl.maxTextureSize) || 16384,
    },
    clientHints: P.clientHints || {},
    injectChrome: false,
  };
  const workerBootstrap = [
    "(function(){",
    "\"use strict\";",
    "var APPLIED=Symbol.for(\"bobby.fp.worker\");",
    "if(globalThis[APPLIED])return;",
    "Object.defineProperty(globalThis,APPLIED,{value:true,configurable:false,enumerable:false,writable:false});",
    "const P=",
  ].join("") + JSON.stringify(workerProfile) + ";" + [
    "const UNMASKED_VENDOR_WEBGL=0x9245;",
    "const UNMASKED_RENDERER_WEBGL=0x9246;",
    "function patchNav(key,getFn){",
    "try{Object.defineProperty(Navigator.prototype,key,{get:getFn,configurable:true});}catch(_){}",
    "try{if(typeof navigator!==\"undefined\")Object.defineProperty(navigator,key,{get:getFn,configurable:true});}catch(_){}",
    "}",
    "patchNav(\"webdriver\",function webdriver(){return false;});",
    "patchNav(\"userAgent\",function userAgent(){return P.userAgent;});",
    "patchNav(\"platform\",function platform(){return P.platform;});",
    "patchNav(\"language\",function language(){return P.locale;});",
    "patchNav(\"languages\",function languages(){return Object.freeze([P.locale,P.locale.split(\"-\")[0]]);});",
    "patchNav(\"hardwareConcurrency\",function hardwareConcurrency(){return P.hardwareConcurrency;});",
    "patchNav(\"deviceMemory\",function deviceMemory(){return P.deviceMemory;});",
    "patchNav(\"maxTouchPoints\",function maxTouchPoints(){return P.maxTouchPoints;});",
    "try{",
    "const hints=P.clientHints||{};",
    "const brands=hints.brands||[];",
    "const fullVersionList=hints.fullVersionList||[];",
    "const uaData={",
    "brands:brands.slice(),",
    "mobile:!!hints.mobile,",
    "platform:hints.platform||\"\",",
    "getHighEntropyValues:function(hintsList){",
    "const out={brands:brands.slice(),mobile:!!hints.mobile,platform:hints.platform||\"\"};",
    "const wanted=Array.isArray(hintsList)?hintsList:[];",
    "for(let i=0;i<wanted.length;i++){",
    "const key=wanted[i];",
    "if(key===\"architecture\")out.architecture=hints.architecture||\"\";",
    "if(key===\"bitness\")out.bitness=hints.bitness||\"\";",
    "if(key===\"model\")out.model=hints.model||\"\";",
    "if(key===\"platformVersion\")out.platformVersion=hints.platformVersion||\"\";",
    "if(key===\"uaFullVersion\"||key===\"fullVersion\")out.uaFullVersion=hints.fullVersion||\"\";",
    "if(key===\"fullVersionList\")out.fullVersionList=fullVersionList.slice();",
    "if(key===\"wow64\")out.wow64=false;",
    "if(key===\"formFactors\")out.formFactors=[\"Desktop\"];",
    "}",
    "return Promise.resolve(out);",
    "},",
    "toJSON:function(){return{brands:brands.slice(),mobile:!!hints.mobile,platform:hints.platform||\"\"};},",
    "};",
    // Workers (esp. SharedWorker) often have no Navigator global — patch instance.
    "try{if(typeof Navigator!==\"undefined\")Object.defineProperty(Navigator.prototype,\"userAgentData\",{get:function(){return uaData;},configurable:true});}catch(_){}",
    "try{",
    "const nav=typeof navigator!==\"undefined\"?navigator:null;",
    "if(nav){",
    "const nativeUad=nav.userAgentData;",
    "try{delete nav.userAgentData;}catch(_){}",
    "try{Object.defineProperty(nav,\"userAgentData\",{get:function(){return uaData;},configurable:true,enumerable:true});}catch(_){}",
    "function patchUad(target){",
    "if(!target)return;",
    "try{Object.defineProperty(target,\"getHighEntropyValues\",{value:function(hintsList){return Promise.resolve(uaData.getHighEntropyValues(hintsList));},writable:true,configurable:true});}catch(_){}",
    "try{target.getHighEntropyValues=function(hintsList){return Promise.resolve(uaData.getHighEntropyValues(hintsList));};}catch(_){}",
    "try{Object.defineProperty(target,\"brands\",{get:function(){return brands.slice();},configurable:true});}catch(_){}",
    "try{Object.defineProperty(target,\"platform\",{get:function(){return hints.platform||\"\";},configurable:true});}catch(_){}",
    "try{Object.defineProperty(target,\"mobile\",{get:function(){return !!hints.mobile;},configurable:true});}catch(_){}",
    "}",
    "if(nav.userAgentData!==uaData){",
    "patchUad(nativeUad);",
    "try{patchUad(Object.getPrototypeOf(nativeUad));}catch(_){}",
    "patchUad(nav.userAgentData);",
    "try{patchUad(Object.getPrototypeOf(nav.userAgentData));}catch(_){}",
    "}",
    "}",
    "}catch(_){}",
    "}catch(_){}",
    "try{",
    "const tz=P.timezoneId;",
    "if(tz&&typeof Intl!==\"undefined\"&&Intl.DateTimeFormat){",
    "const OriginalDTF=Intl.DateTimeFormat;",
    "Intl.DateTimeFormat=function(locales,options){",
    "const opts=Object.assign({},options||{});",
    "if(!opts.timeZone)opts.timeZone=tz;",
    "return new OriginalDTF(locales,opts);",
    "};",
    "Intl.DateTimeFormat.prototype=OriginalDTF.prototype;",
    "Intl.DateTimeFormat.supportedLocalesOf=OriginalDTF.supportedLocalesOf.bind(OriginalDTF);",
    "}",
    "}catch(_){}",
    "function patchWebGl(proto){",
    "if(!proto||!proto.getParameter)return;",
    "const original=proto.getParameter;",
    "const maxTex=(P.webgl&&P.webgl.maxTextureSize)||16384;",
    "const MAX_TEXTURE_SIZE=proto.MAX_TEXTURE_SIZE||0x0D33;",
    "const MAX_RENDERBUFFER_SIZE=proto.MAX_RENDERBUFFER_SIZE||0x84E8;",
    "const MAX_VERTEX_ATTRIBS=proto.MAX_VERTEX_ATTRIBS||0x8869;",
    "const MAX_VIEWPORT_DIMS=proto.MAX_VIEWPORT_DIMS||0x0D3A;",
    "const ALIASED_LINE_WIDTH_RANGE=proto.ALIASED_LINE_WIDTH_RANGE||0x846E;",
    "const ALIASED_POINT_SIZE_RANGE=proto.ALIASED_POINT_SIZE_RANGE||0x846D;",
    "proto.getParameter=function getParameter(param){",
    "if(param===UNMASKED_VENDOR_WEBGL)return P.webgl.vendor;",
    "if(param===UNMASKED_RENDERER_WEBGL)return P.webgl.renderer;",
    "if(param===MAX_TEXTURE_SIZE)return maxTex;",
    "if(param===MAX_RENDERBUFFER_SIZE)return maxTex;",
    "if(param===MAX_VERTEX_ATTRIBS)return 16;",
    "if(param===MAX_VIEWPORT_DIMS)return new Int32Array([maxTex,maxTex]);",
    "if(param===ALIASED_LINE_WIDTH_RANGE)return new Float32Array([1,1]);",
    "if(param===ALIASED_POINT_SIZE_RANGE)return new Float32Array([1,1024]);",
    "return original.apply(this,arguments);",
    "};",
    "}",
    "try{",
    "patchWebGl(typeof WebGLRenderingContext!==\"undefined\"&&WebGLRenderingContext.prototype);",
    "if(typeof WebGL2RenderingContext!==\"undefined\")patchWebGl(WebGL2RenderingContext.prototype);",
    "}catch(_){}",
    "})();",
  ].join("");

  function resolveUrl(scriptURL) {
    try { return new URL(scriptURL, location.href).href; } catch (_) { return String(scriptURL); }
  }
  function loadScriptSourceSync(scriptURL) {
    const abs = resolveUrl(scriptURL);
    try {
      const xhr = new XMLHttpRequest();
      xhr.open("GET", abs, false);
      xhr.send(null);
      if (xhr.status === 200 || xhr.status === 0) {
        return xhr.responseText;
      }
    } catch (_) {}
    return null;
  }
  function wrapWorkerScriptUrl(scriptURL, options) {
    const abs = resolveUrl(scriptURL);
    const isModule = options && options.type === "module";
    const inline = !isModule ? loadScriptSourceSync(abs) : null;
    let body;
    if (isModule) {
      body = workerBootstrap + "\nimport " + JSON.stringify(abs) + ";\n";
    } else if (inline) {
      body = workerBootstrap + "\n" + inline + "\n";
    } else {
      body = workerBootstrap + "\ntry { importScripts(" + JSON.stringify(abs) + "); } catch (e) { throw e; }\n";
    }
    const blob = new Blob([body], { type: isModule ? "text/javascript" : "application/javascript" });
    return URL.createObjectURL(blob);
  }

  function installWorkerWrapper(name, OriginalCtor) {
    if (!OriginalCtor || OriginalCtor.__bobbyWrapped) return;
    const Wrapped = cloak(function Wrapped(scriptURL, options) {
      const wrappedUrl = wrapWorkerScriptUrl(scriptURL, options);
      if (name === "SharedWorker" && typeof options === "string") {
        return new OriginalCtor(wrappedUrl, options);
      }
      return new OriginalCtor(wrappedUrl, options);
    });
    Wrapped.prototype = OriginalCtor.prototype;
    Wrapped.__bobbyWrapped = true;
    Object.defineProperty(Wrapped, "name", { value: name });
    const desc = Object.getOwnPropertyDescriptor(globalThis, name);
    if (desc && desc.configurable === false) {
      try {
        Object.defineProperty(globalThis, name, { value: Wrapped, configurable: true, writable: true });
      } catch (_) {}
    } else {
      try { globalThis[name] = Wrapped; } catch (_) {}
    }
    try {
      if (typeof window !== "undefined") window[name] = Wrapped;
    } catch (_) {}
  }

  function deferWorkerWrap(name) {
    function tryInstall() {
      try {
        if (typeof globalThis[name] !== "undefined") {
          installWorkerWrapper(name, globalThis[name]);
          return true;
        }
      } catch (_) {}
      return false;
    }
    if (tryInstall()) return;
    try {
      let current = globalThis[name];
      Object.defineProperty(globalThis, name, {
        configurable: true,
        enumerable: true,
        get: function () { return current; },
        set: function (next) {
          current = next;
          installWorkerWrapper(name, next);
        },
      });
    } catch (_) {}
    try {
      const timer = setInterval(function () {
        if (tryInstall()) clearInterval(timer);
      }, 0);
      setTimeout(function () { clearInterval(timer); }, 5000);
    } catch (_) {}
  }

  try { deferWorkerWrap("Worker"); } catch (_) {}
  try { deferWorkerWrap("SharedWorker"); } catch (_) {}
})();"#;

/// Build the document-start init script that patches fingerprint surfaces.
pub fn build_init_script(session: &FingerprintSession) -> Result<String, FingerprintApplyError> {
    let profile = serde_json::to_string(session).map_err(|error| {
        FingerprintApplyError::Host(format!("fingerprint profile serialize failed: {error}"))
    })?;
    if !INIT_SCRIPT_TEMPLATE.contains(PROFILE_PLACEHOLDER) {
        return Err(FingerprintApplyError::Host(
            "init script template missing profile placeholder".into(),
        ));
    }
    Ok(INIT_SCRIPT_TEMPLATE.replace(PROFILE_PLACEHOLDER, &profile))
}

/// Probe script that returns observed fingerprint signals for conformance tests.
pub fn build_probe_script() -> String {
    r##"(async function() {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const ctx = canvas.getContext("2d");
  let canvasHash = null;
  let canvasHash2 = null;
  if (ctx) {
    ctx.fillStyle = "#f60";
    ctx.fillRect(0, 0, 64, 64);
    ctx.fillStyle = "#069";
    ctx.font = "16px Arial";
    ctx.fillText("bobby", 4, 32);
    canvasHash = canvas.toDataURL();
    canvasHash2 = canvas.toDataURL();
  }
  let webglVendor = null;
  let webglRenderer = null;
  try {
    const glCanvas = document.createElement("canvas");
    const gl = glCanvas.getContext("webgl") || glCanvas.getContext("experimental-webgl");
    if (gl) {
      const ext = gl.getExtension("WEBGL_debug_renderer_info");
      if (ext) {
        webglVendor = gl.getParameter(ext.UNMASKED_VENDOR_WEBGL);
        webglRenderer = gl.getParameter(ext.UNMASKED_RENDERER_WEBGL);
      }
    }
  } catch (_) {}
  let uaData = null;
  try {
    if (navigator.userAgentData) {
      uaData = {
        brands: navigator.userAgentData.brands,
        mobile: navigator.userAgentData.mobile,
        platform: navigator.userAgentData.platform,
        highEntropy: await navigator.userAgentData.getHighEntropyValues([
          "architecture",
          "bitness",
          "platformVersion",
          "fullVersionList",
          "uaFullVersion",
        ]),
      };
    }
  } catch (_) {}
  let pluginCount = 0;
  try { pluginCount = navigator.plugins ? navigator.plugins.length : 0; } catch (_) {}
  let timezone = null;
  try { timezone = Intl.DateTimeFormat().resolvedOptions().timeZone; } catch (_) {}
  let rtcConstructible = false;
  try {
    const pc = new RTCPeerConnection({ iceServers: [] });
    rtcConstructible = true;
    pc.close();
  } catch (_) {}
  let mediaDeviceCount = 0;
  try {
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      const devices = await navigator.mediaDevices.enumerateDevices();
      mediaDeviceCount = devices.length;
    }
  } catch (_) {}
  let speechVoiceCount = 0;
  try {
    if (window.speechSynthesis && speechSynthesis.getVoices) {
      speechVoiceCount = speechSynthesis.getVoices().length;
    }
  } catch (_) {}
  let batteryLevel = null;
  try {
    if (navigator.getBattery) {
      const bat = await navigator.getBattery();
      batteryLevel = bat.level;
    }
  } catch (_) {}
  let connectionEffectiveType = null;
  try {
    if (navigator.connection) {
      connectionEffectiveType = navigator.connection.effectiveType;
    }
  } catch (_) {}
  let webglMaxTextureSize = null;
  try {
    const glCanvas2 = document.createElement("canvas");
    const gl2 = glCanvas2.getContext("webgl") || glCanvas2.getContext("experimental-webgl");
    if (gl2) {
      webglMaxTextureSize = gl2.getParameter(gl2.MAX_TEXTURE_SIZE);
    }
  } catch (_) {}
  return {
    userAgent: navigator.userAgent,
    platform: navigator.platform,
    language: navigator.language,
    languages: Array.from(navigator.languages || []),
    hardwareConcurrency: navigator.hardwareConcurrency,
    deviceMemory: navigator.deviceMemory,
    maxTouchPoints: navigator.maxTouchPoints,
    webdriver: navigator.webdriver,
    screen: {
      width: screen.width,
      height: screen.height,
      availWidth: screen.availWidth,
      availHeight: screen.availHeight,
      colorDepth: screen.colorDepth,
      pixelRatio: window.devicePixelRatio,
    },
    canvasHash,
    canvasHashStable: canvasHash === canvasHash2,
    webglVendor,
    webglRenderer,
    userAgentData: uaData,
    pluginCount,
    timezone,
    rtcConstructible,
    mediaDeviceCount,
    speechVoiceCount,
    batteryLevel,
    connectionEffectiveType,
    webglMaxTextureSize,
    fingerprintApplied: !!globalThis[Symbol.for("bobby.fp.applied")],
    hasBobbyMarker: typeof globalThis.__bobbyFingerprintApplied !== "undefined",
  };
})()"##
        .to_string()
}

/// Collector-oriented probe that checks common detection tells (not session JSON).
pub fn build_collector_probe_script() -> String {
    r##"(async function() {
  const fails = [];
  const checks = {};
  function check(name, ok, detail) {
    checks[name] = ok;
    if (!ok) fails.push({ check: name, detail: detail || name });
  }

  check("fingerprintApplied", !!globalThis[Symbol.for("bobby.fp.applied")]);
  check("noBobbyMarker", typeof globalThis.__bobbyFingerprintApplied === "undefined");
  check("webdriverFalse", navigator.webdriver === false);

  const ua = navigator.userAgent || "";
  check("uaMatchesProfile", ua.length > 0 && ua.indexOf("Chrome/") >= 0);
  check("platformMatches", !!navigator.platform);

  let uaChOk = true;
  try {
    if (ua.indexOf("Chrome/") >= 0) {
      uaChOk = !!navigator.userAgentData && !!navigator.userAgentData.platform;
    }
  } catch (_) { uaChOk = false; }
  check("uaChPlatform", uaChOk);

  let canvasStable = false;
  try {
    const c = document.createElement("canvas");
    c.width = 64;
    c.height = 64;
    const ctx = c.getContext("2d");
    if (ctx) {
      ctx.fillStyle = "#f60";
      ctx.fillRect(0, 0, 64, 64);
      const h1 = c.toDataURL();
      const h2 = c.toDataURL();
      canvasStable = h1 === h2;
    }
  } catch (_) {}
  check("canvasStable", canvasStable);

  let webglOk = false;
  try {
    const glc = document.createElement("canvas");
    const gl = glc.getContext("webgl") || glc.getContext("experimental-webgl");
    if (gl) {
      const ext = gl.getExtension("WEBGL_debug_renderer_info");
      if (ext) {
        webglOk = !!gl.getParameter(ext.UNMASKED_VENDOR_WEBGL);
      } else {
        webglOk = true;
      }
    } else {
      webglOk = true;
    }
  } catch (_) {}
  check("webglVendorMatch", webglOk);

  let rtcOk = false;
  try {
    const pc = new RTCPeerConnection({ iceServers: [] });
    rtcOk = true;
    pc.close();
  } catch (_) {}
  check("rtcConstructible", rtcOk);

  let pluginsOk = false;
  try {
    pluginsOk = navigator.plugins && navigator.plugins.length >= 1;
  } catch (_) {}
  check("pluginsPresent", pluginsOk);

  let toStringOk = false;
  try {
    const desc = Object.getOwnPropertyDescriptor(Navigator.prototype, "webdriver");
    if (desc && desc.get) {
      toStringOk = Function.prototype.toString.call(desc.get).indexOf("[native code]") >= 0;
    }
  } catch (_) {}
  check("toStringNativeWebdriver", toStringOk);

  let vendorOk = true;
  try {
    if (ua.indexOf("Chrome/") >= 0) {
      vendorOk = navigator.vendor === "Google Inc.";
    }
  } catch (_) { vendorOk = false; }
  check("vendorGoogle", vendorOk);

  let pdfOk = false;
  try {
    pdfOk = navigator.pdfViewerEnabled === true;
  } catch (_) {}
  check("pdfViewerEnabled", pdfOk);

  try {
    if (globalThis.chrome && !globalThis.chrome.runtime) {
      fails.push({ check: "chromeRuntime", detail: "chrome exists but runtime missing" });
    }
  } catch (_) {}

  return {
    passed: fails.length === 0,
    failCount: fails.length,
    fails: fails,
    checks: checks,
  };
})()"##
        .to_string()
}

/// Probe Worker and SharedWorker navigator signals against the session profile.
pub fn build_worker_probe_script() -> String {
    r##"(async function(){
  const workerSrc = `
    (async function() {
      let highEntropy = null;
      try {
        if (navigator.userAgentData && navigator.userAgentData.getHighEntropyValues) {
          highEntropy = await navigator.userAgentData.getHighEntropyValues([
            "architecture", "bitness", "platformVersion", "fullVersionList", "uaFullVersion"
          ]);
        }
      } catch (_) {}
      const payload = {
        ua: navigator.userAgent,
        platform: navigator.platform,
        webdriver: navigator.webdriver,
        uaDataPlatform: navigator.userAgentData ? navigator.userAgentData.platform : null,
        uaDataBrands: navigator.userAgentData ? navigator.userAgentData.brands : null,
        highEntropy: highEntropy,
        bootstrapApplied: !!globalThis[Symbol.for("bobby.fp.worker")]
      };
      if (typeof postMessage === "function") postMessage(payload);
    })();
  `;
  const sharedSrc = `
    onconnect = function(e) {
      const port = e.ports[0];
      (async function() {
        let highEntropy = null;
        try {
          if (navigator.userAgentData && navigator.userAgentData.getHighEntropyValues) {
            highEntropy = await navigator.userAgentData.getHighEntropyValues([
              "architecture", "bitness", "platformVersion", "fullVersionList", "uaFullVersion"
            ]);
          }
        } catch (_) {}
        port.postMessage({
          ua: navigator.userAgent,
          platform: navigator.platform,
          webdriver: navigator.webdriver,
          uaDataPlatform: navigator.userAgentData ? navigator.userAgentData.platform : null,
          uaDataBrands: navigator.userAgentData ? navigator.userAgentData.brands : null,
          highEntropy: highEntropy,
          bootstrapApplied: !!globalThis[Symbol.for("bobby.fp.worker")]
        });
      })();
    };
  `;
  function workerUa() {
    return new Promise(function(resolve, reject) {
      const blob = new Blob([workerSrc], {type: "application/javascript"});
      const w = new Worker(URL.createObjectURL(blob));
      w.onmessage = function(e) { resolve(e.data); };
      w.onerror = function(e) { reject(e.message || "worker error"); };
      setTimeout(function() { reject("timeout"); }, 5000);
    });
  }
  function sharedUa() {
    return new Promise(function(resolve, reject) {
      if (typeof SharedWorker === "undefined") {
        reject("SharedWorker unavailable");
        return;
      }
      const blob = new Blob([sharedSrc], {type: "application/javascript"});
      const sw = new SharedWorker(URL.createObjectURL(blob));
      sw.port.onmessage = function(e) { resolve(e.data); };
      sw.onerror = function(e) { reject(e.message || "shared worker error"); };
      setTimeout(function() { reject("timeout"); }, 5000);
    });
  }
  const w = await workerUa();
  let s = null;
  try { s = await sharedUa(); } catch (_) {}
  let highEntropy = null;
  try {
    if (navigator.userAgentData && navigator.userAgentData.getHighEntropyValues) {
      highEntropy = await navigator.userAgentData.getHighEntropyValues([
        "architecture", "bitness", "platformVersion", "fullVersionList", "uaFullVersion"
      ]);
    }
  } catch (_) {}
  const page = {
    ua: navigator.userAgent,
    platform: navigator.platform,
    webdriver: navigator.webdriver,
    uaDataPlatform: navigator.userAgentData ? navigator.userAgentData.platform : null,
    uaDataBrands: navigator.userAgentData ? navigator.userAgentData.brands : null,
    highEntropy: highEntropy
  };
  return { page: page, worker: w, shared: s };
})()"##
        .to_string()
}

/// Probe DOM/canvas font detection against the session allowlist.
pub fn build_font_probe_script() -> String {
    r##"(async function(){
  function widthFor(family) {
    const span = document.createElement("span");
    span.style.cssText = "position:absolute;left:-9999px;top:0;font-size:72px;line-height:normal;font-family:" + family;
    span.textContent = "mmmmmmmmmmlli";
    document.body.appendChild(span);
    const w = span.offsetWidth;
    span.remove();
    return w;
  }
  function measureFor(font) {
    const c = document.createElement("canvas");
    const ctx = c.getContext("2d");
    if (!ctx) return null;
    ctx.font = font;
    return ctx.measureText("mmmmmmmmmmlli").width;
  }
  const baseFamily = "monospace";
  const base = widthFor(baseFamily);
  const helvetica = widthFor('"Helvetica Neue", monospace');
  const pingfang = widthFor('"PingFang SC", monospace');
  const arial = widthFor("Arial, monospace");
  const baseMeasure = measureFor("72px monospace");
  const helveticaMeasure = measureFor('72px "Helvetica Neue"');
  const arialMeasure = measureFor("72px Arial");
  let checkHelvetica = null;
  let checkArial = null;
  let fontFaceHelvetica = null;
  let fontFaceArial = null;
  try {
    checkHelvetica = document.fonts.check('72px "Helvetica Neue"');
    checkArial = document.fonts.check("72px Arial");
  } catch (_) {}
  try {
    await new FontFace("Helvetica Neue", 'local("Helvetica Neue")').load();
    fontFaceHelvetica = true;
  } catch (_) {
    fontFaceHelvetica = false;
  }
  try {
    await new FontFace("Arial", 'local("Arial")').load();
    fontFaceArial = true;
  } catch (_) {
    fontFaceArial = false;
  }
  return {
    fingerprintApplied: !!globalThis[Symbol.for("bobby.fp.applied")],
    offset: {
      base: base,
      helvetica: helvetica,
      pingfang: pingfang,
      arial: arial,
      helveticaHidden: helvetica === base,
      pingfangHidden: pingfang === base
    },
    measureText: {
      base: baseMeasure,
      helvetica: helveticaMeasure,
      arial: arialMeasure,
      helveticaHidden: helveticaMeasure === baseMeasure
    },
    fontsCheck: {
      helvetica: checkHelvetica,
      arial: checkArial
    },
    fontFaceLoad: {
      helvetica: fontFaceHelvetica,
      arial: fontFaceArial
    },
    touch: (function () {
      let createEventTouch = null;
      try {
        document.createEvent("TouchEvent");
        createEventTouch = true;
      } catch (_) {
        createEventTouch = false;
      }
      let anyPointerCoarse = null;
      let anyPointerFine = null;
      try {
        anyPointerCoarse = matchMedia("(any-pointer: coarse)").matches;
        anyPointerFine = matchMedia("(any-pointer: fine)").matches;
      } catch (_) {}
      return {
        maxTouchPoints: navigator.maxTouchPoints,
        ontouchstartInWindow: "ontouchstart" in window,
        createEventTouch: createEventTouch,
        creepHasTouch: ("ontouchstart" in window) && createEventTouch,
        anyPointerCoarse: anyPointerCoarse,
        anyPointerFine: anyPointerFine
      };
    })()
  };
})()"##
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FingerprintConfig;

    #[test]
    fn init_script_fails_closed_and_scrubs_markers() {
        let session = crate::create_session(&FingerprintConfig::default().with_session_seed(1));
        let script = build_init_script(&session).unwrap();
        assert!(!script.contains("__bobbyFingerprintApplied"));
        assert!(script.contains("Symbol.for(\"bobby.fp.applied\")"));
        assert!(script.contains("return false"));
        assert!(script.contains("createElement(\"canvas\")"));
        assert!(!script.contains(
            "ctx.putImageData(img, 0, 0);\n            return originalToDataURL.apply(this"
        ));
        assert!(script.contains(&session.user_agent));
        assert!(script.contains("iceTransportPolicy"));
        assert!(script.contains("enumerateDevices"));
        assert!(script.contains("getBattery"));
        assert!(script.contains("maxTextureSize"));
    }

    #[test]
    fn template_placeholder_round_trips() {
        assert!(INIT_SCRIPT_TEMPLATE.contains(PROFILE_PLACEHOLDER));
        let session = crate::create_session(&FingerprintConfig::default().with_session_seed(2));
        let script = build_init_script(&session).unwrap();
        assert!(!script.contains(PROFILE_PLACEHOLDER));
    }

    #[test]
    fn extension_template_matches_rust_template() {
        let ts = include_str!("../../../packages/firefox-companion/src/init-script-template.ts");
        let marker = "export const INIT_SCRIPT_TEMPLATE = ";
        let start = ts.find(marker).expect("template export") + marker.len();
        let end = ts[start..].find(";\n").expect("template terminator") + start;
        let encoded = &ts[start..end];
        let decoded: String = serde_json::from_str(encoded).expect("template JSON string");
        assert_eq!(decoded, INIT_SCRIPT_TEMPLATE);
    }
}
