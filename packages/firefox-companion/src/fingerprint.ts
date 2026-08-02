/** Shared fingerprint apply + storage toggle for companion extensions. */

export const FINGERPRINT_ENABLED_KEY = "fingerprintEnabled";
export const FINGERPRINT_PROFILE_KEY = "fingerprintProfile";

export type FingerprintProfile = {
  sessionId: string;
  sessionSeed: number;
  canvasHash: string;
  canvasNoiseAmplitude: number;
  webgl: { vendor: string; renderer: string; hash: string };
  audioHash: string;
  audioNoiseScale: number;
  fontList: string[];
  screenResolution: {
    width: number;
    height: number;
    availableWidth: number;
    availableHeight: number;
    colorDepth: number;
    pixelRatio: number;
  };
  userAgent: string;
  platform: string;
  locale: string;
  timezoneId: string;
  hardwareConcurrency: number;
  deviceMemory: number;
  maxTouchPoints: number;
};

export const DEFAULT_FINGERPRINT_PROFILE: FingerprintProfile = {
  sessionId: "fp_extension_default",
  sessionSeed: 0xb0b5f1d,
  canvasHash: "0".repeat(64),
  canvasNoiseAmplitude: 2,
  webgl: {
    vendor: "Google Inc. (NVIDIA)",
    renderer:
      "ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 Super Direct3D11 vs_5_0 ps_5_0, D3D11)",
    hash: "0".repeat(64),
  },
  audioHash: "0".repeat(64),
  audioNoiseScale: 1.5e-7,
  fontList: [
    "Arial",
    "Arial Black",
    "Calibri",
    "Cambria",
    "Comic Sans MS",
    "Courier New",
    "Georgia",
    "Impact",
    "Lucida Console",
    "Lucida Sans Unicode",
    "Microsoft Sans Serif",
    "Palatino Linotype",
    "Segoe UI",
    "Tahoma",
    "Times New Roman",
    "Trebuchet MS",
    "Verdana",
  ],
  screenResolution: {
    width: 1920,
    height: 1080,
    availableWidth: 1920,
    availableHeight: 1040,
    colorDepth: 24,
    pixelRatio: 1,
  },
  userAgent:
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
  platform: "Win32",
  locale: "en-US",
  timezoneId: "America/New_York",
  hardwareConcurrency: 8,
  deviceMemory: 8,
  maxTouchPoints: 0,
};

type FingerprintStorage = {
  local: {
    get(keys: readonly string[]): Promise<Record<string, unknown>>;
    set(values: Record<string, unknown>): Promise<void>;
  };
};

export async function getFingerprintEnabled(storage: FingerprintStorage): Promise<boolean> {
  const stored = await storage.local.get([FINGERPRINT_ENABLED_KEY]);
  const value = stored[FINGERPRINT_ENABLED_KEY];
  return value === undefined ? true : value === true;
}

export async function setFingerprintEnabled(
  storage: FingerprintStorage,
  enabled: boolean,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_ENABLED_KEY]: enabled });
}

export async function getFingerprintProfile(
  storage: FingerprintStorage,
): Promise<FingerprintProfile> {
  const stored = await storage.local.get([FINGERPRINT_PROFILE_KEY]);
  const value = stored[FINGERPRINT_PROFILE_KEY];
  if (value && typeof value === "object") {
    return { ...DEFAULT_FINGERPRINT_PROFILE, ...(value as FingerprintProfile) };
  }
  return DEFAULT_FINGERPRINT_PROFILE;
}

export async function setFingerprintProfile(
  storage: FingerprintStorage,
  profile: FingerprintProfile,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_PROFILE_KEY]: profile });
}

/** Apply fingerprint patches into the current window. Idempotent. */
export function applyFingerprintProfile(profile: FingerprintProfile): void {
  const g = globalThis as typeof globalThis & {
    __bobbyFingerprintApplied?: boolean;
  };
  if (g.__bobbyFingerprintApplied) return;
  g.__bobbyFingerprintApplied = true;
  const P = profile;
  const UNMASKED_VENDOR_WEBGL = 0x9245;
  const UNMASKED_RENDERER_WEBGL = 0x9246;

  try {
    Object.defineProperty(Navigator.prototype, "webdriver", {
      get() {
        return undefined;
      },
      configurable: true,
    });
  } catch {
    /* ignore */
  }

  try {
    Object.defineProperty(Navigator.prototype, "userAgent", {
      get() {
        return P.userAgent;
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "platform", {
      get() {
        return P.platform;
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "language", {
      get() {
        return P.locale;
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "languages", {
      get() {
        return Object.freeze([P.locale, P.locale.split("-")[0]]);
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "hardwareConcurrency", {
      get() {
        return P.hardwareConcurrency;
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "deviceMemory", {
      get() {
        return P.deviceMemory;
      },
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, "maxTouchPoints", {
      get() {
        return P.maxTouchPoints;
      },
      configurable: true,
    });
  } catch {
    /* ignore */
  }

  try {
    const w = globalThis as typeof globalThis & { chrome?: { runtime: object } };
    if (!w.chrome) w.chrome = { runtime: {} };
  } catch {
    /* ignore */
  }

  const screenPatch = {
    width: P.screenResolution.width,
    height: P.screenResolution.height,
    availWidth: P.screenResolution.availableWidth,
    availHeight: P.screenResolution.availableHeight,
    colorDepth: P.screenResolution.colorDepth,
    pixelDepth: P.screenResolution.colorDepth,
  };
  try {
    for (const [key, value] of Object.entries(screenPatch)) {
      Object.defineProperty(Screen.prototype, key, {
        get() {
          return value;
        },
        configurable: true,
      });
    }
    Object.defineProperty(window, "devicePixelRatio", {
      get() {
        return P.screenResolution.pixelRatio;
      },
      configurable: true,
    });
  } catch {
    /* ignore */
  }

  const canvasNoise = P.canvasNoiseAmplitude | 0;
  const canvasSeed = P.sessionSeed >>> 0;
  function mix(n: number): number {
    let x = (n + canvasSeed) | 0;
    x = Math.imul(x ^ (x >>> 16), 2246822507);
    x = Math.imul(x ^ (x >>> 13), 3266489909);
    return (x ^ (x >>> 16)) >>> 0;
  }

  try {
    const proto = HTMLCanvasElement.prototype;
    const originalToDataURL = proto.toDataURL;
    proto.toDataURL = function (this: HTMLCanvasElement, ...args: unknown[]) {
      try {
        const ctx = this.getContext("2d");
        if (ctx && canvasNoise > 0) {
          const w = Math.min(this.width || 0, 16);
          const h = Math.min(this.height || 0, 16);
          if (w > 0 && h > 0) {
            const img = ctx.getImageData(0, 0, w, h);
            for (let i = 0; i < img.data.length; i += 4) {
              const n = mix(i) % (canvasNoise + 1);
              img.data[i] = ((img.data[i] ?? 0) + n) & 255;
            }
            ctx.putImageData(img, 0, 0);
          }
        }
      } catch {
        /* ignore */
      }
      return originalToDataURL.apply(this, args as []);
    };
  } catch {
    /* ignore */
  }

  try {
    const patchWebGl = (proto: WebGLRenderingContext) => {
      if (!proto?.getParameter) return;
      const original = proto.getParameter.bind(proto);
      proto.getParameter = function (param: number) {
        if (param === UNMASKED_VENDOR_WEBGL) return P.webgl.vendor;
        if (param === UNMASKED_RENDERER_WEBGL) return P.webgl.renderer;
        return original(param);
      };
    };
    patchWebGl(WebGLRenderingContext.prototype as unknown as WebGLRenderingContext);
    if (typeof WebGL2RenderingContext !== "undefined") {
      patchWebGl(WebGL2RenderingContext.prototype as unknown as WebGLRenderingContext);
    }
  } catch {
    /* ignore */
  }
}
