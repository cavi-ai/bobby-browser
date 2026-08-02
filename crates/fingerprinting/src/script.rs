//! Init / probe JavaScript emitted from a [`crate::FingerprintSession`].

use crate::FingerprintSession;

/// Build the document-start init script that patches fingerprint surfaces.
pub fn build_init_script(session: &FingerprintSession) -> String {
    let profile = serde_json::to_string(session).unwrap_or_else(|_| "{}".to_string());
    format!(
        r#"(function() {{
  "use strict";
  if (globalThis.__bobbyFingerprintApplied) return;
  globalThis.__bobbyFingerprintApplied = true;
  const P = {profile};
  const UNMASKED_VENDOR_WEBGL = 0x9245;
  const UNMASKED_RENDERER_WEBGL = 0x9246;

  try {{
    Object.defineProperty(Navigator.prototype, "webdriver", {{
      get: function () {{ return undefined; }},
      configurable: true,
    }});
  }} catch (_) {{}}

  try {{
    Object.defineProperty(Navigator.prototype, "userAgent", {{
      get: function () {{ return P.userAgent; }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "platform", {{
      get: function () {{ return P.platform; }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "language", {{
      get: function () {{ return P.locale; }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "languages", {{
      get: function () {{ return Object.freeze([P.locale, P.locale.split("-")[0]]); }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "hardwareConcurrency", {{
      get: function () {{ return P.hardwareConcurrency; }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "deviceMemory", {{
      get: function () {{ return P.deviceMemory; }},
      configurable: true,
    }});
    Object.defineProperty(Navigator.prototype, "maxTouchPoints", {{
      get: function () {{ return P.maxTouchPoints; }},
      configurable: true,
    }});
  }} catch (_) {{}}

  try {{
    if (!globalThis.chrome) {{
      globalThis.chrome = {{ runtime: {{}} }};
    }}
  }} catch (_) {{}}

  const screenPatch = {{
    width: P.screenResolution.width,
    height: P.screenResolution.height,
    availWidth: P.screenResolution.availableWidth,
    availHeight: P.screenResolution.availableHeight,
    colorDepth: P.screenResolution.colorDepth,
    pixelDepth: P.screenResolution.colorDepth,
  }};
  try {{
    for (const [key, value] of Object.entries(screenPatch)) {{
      Object.defineProperty(Screen.prototype, key, {{
        get: function () {{ return value; }},
        configurable: true,
      }});
    }}
    Object.defineProperty(window, "devicePixelRatio", {{
      get: function () {{ return P.screenResolution.pixelRatio; }},
      configurable: true,
    }});
  }} catch (_) {{}}

  const fontSet = new Set(P.fontList || []);
  try {{
    if (document.fonts && document.fonts.check) {{
      const originalCheck = document.fonts.check.bind(document.fonts);
      document.fonts.check = function (font, text) {{
        const family = String(font).replace(/^.*?([A-Za-z][A-Za-z0-9 ]+).*$/, "$1").trim();
        if (family && fontSet.size > 0) {{
          for (const allowed of fontSet) {{
            if (font.includes(allowed)) return true;
          }}
          return false;
        }}
        return originalCheck(font, text);
      }};
    }}
  }} catch (_) {{}}

  const canvasNoise = P.canvasNoiseAmplitude | 0;
  const canvasSeed = P.sessionSeed >>> 0;
  function mix(n) {{
    n = (n + canvasSeed) | 0;
    n = Math.imul(n ^ (n >>> 16), 2246822507);
    n = Math.imul(n ^ (n >>> 13), 3266489909);
    return (n ^ (n >>> 16)) >>> 0;
  }}
  function patchCanvasProto(proto) {{
    if (!proto) return;
    const originalToDataURL = proto.toDataURL;
    proto.toDataURL = function () {{
      try {{
        const ctx = this.getContext && this.getContext("2d");
        if (ctx && canvasNoise > 0) {{
          const w = Math.min(this.width || 0, 16);
          const h = Math.min(this.height || 0, 16);
          if (w > 0 && h > 0) {{
            const img = ctx.getImageData(0, 0, w, h);
            for (let i = 0; i < img.data.length; i += 4) {{
              const n = mix(i) % (canvasNoise + 1);
              img.data[i] = (img.data[i] + n) & 255;
            }}
            ctx.putImageData(img, 0, 0);
          }}
        }}
      }} catch (_) {{}}
      return originalToDataURL.apply(this, arguments);
    }};
    const originalGetImageData = CanvasRenderingContext2D.prototype.getImageData;
    CanvasRenderingContext2D.prototype.getImageData = function () {{
      const img = originalGetImageData.apply(this, arguments);
      try {{
        if (canvasNoise > 0) {{
          for (let i = 0; i < img.data.length; i += 4) {{
            const n = mix(i) % (canvasNoise + 1);
            img.data[i] = (img.data[i] + n) & 255;
          }}
        }}
      }} catch (_) {{}}
      return img;
    }};
  }}
  try {{
    patchCanvasProto(HTMLCanvasElement.prototype);
  }} catch (_) {{}}

  function patchWebGl(proto) {{
    if (!proto || !proto.getParameter) return;
    const original = proto.getParameter;
    proto.getParameter = function (param) {{
      if (param === UNMASKED_VENDOR_WEBGL) return P.webgl.vendor;
      if (param === UNMASKED_RENDERER_WEBGL) return P.webgl.renderer;
      return original.apply(this, arguments);
    }};
  }}
  try {{
    patchWebGl(WebGLRenderingContext && WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== "undefined") {{
      patchWebGl(WebGL2RenderingContext.prototype);
    }}
  }} catch (_) {{}}

  const audioScale = Number(P.audioNoiseScale) || 1e-7;
  try {{
    const OriginalOffline = window.OfflineAudioContext || window.webkitOfflineAudioContext;
    if (OriginalOffline) {{
      const OriginalProto = OriginalOffline.prototype;
      const originalStart = OriginalProto.startRendering;
      if (originalStart) {{
        OriginalProto.startRendering = function () {{
          const promise = originalStart.apply(this, arguments);
          return Promise.resolve(promise).then(function (buffer) {{
            try {{
              for (let c = 0; c < buffer.numberOfChannels; c++) {{
                const data = buffer.getChannelData(c);
                for (let i = 0; i < data.length; i++) {{
                  data[i] = data[i] + ((mix(i + c * 1024) / 0xffffffff) - 0.5) * audioScale;
                }}
              }}
            }} catch (_) {{}}
            return buffer;
          }});
        }};
      }}
    }}
  }} catch (_) {{}}

  try {{
    if (window.RTCPeerConnection) {{
      const OriginalRTC = window.RTCPeerConnection;
      window.RTCPeerConnection = function () {{
        throw new DOMException("RTCPeerConnection is disabled", "NotSupportedError");
      }};
      window.RTCPeerConnection.prototype = OriginalRTC.prototype;
    }}
  }} catch (_) {{}}
}})();"#
    )
}

/// Probe script that returns observed fingerprint signals for conformance tests.
pub fn build_probe_script() -> String {
    r##"(async function() {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const ctx = canvas.getContext("2d");
  let canvasHash = null;
  if (ctx) {
    ctx.fillStyle = "#f60";
    ctx.fillRect(0, 0, 64, 64);
    ctx.fillStyle = "#069";
    ctx.font = "16px Arial";
    ctx.fillText("bobby", 4, 32);
    canvasHash = canvas.toDataURL();
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
    webglVendor,
    webglRenderer,
    fingerprintApplied: !!globalThis.__bobbyFingerprintApplied,
  };
})()"##
    .to_string()
}
