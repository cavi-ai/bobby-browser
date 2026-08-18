interface RecaptchaApi {
  render(container: HTMLElement, options: { sitekey: string; callback: (token: string) => void }): number;
  getResponse(widgetId: number): string;
  ready(callback: () => void): void;
}

type RecaptchaWindow = Window & { grecaptcha?: RecaptchaApi };

export class RecaptchaController {
  private widgetId: number | undefined;
  private token = "";

  async mount(container: HTMLElement, siteKey: string): Promise<void> {
    const api = await loadRecaptcha(container.ownerDocument);
    // With `render=explicit` the script load only installs a stub; `render`
    // is not callable until the real API arrives — gate on `ready`.
    await new Promise<void>((resolve) => api.ready(resolve));
    this.widgetId = api.render(container, {
      sitekey: siteKey,
      callback: (token) => { this.token = token; },
    });
  }

  response(): string {
    const api = recaptchaApi(globalThis.window);
    if (api !== undefined && this.widgetId !== undefined) {
      return api.getResponse(this.widgetId) || this.token;
    }
    return this.token;
  }
}

function loadRecaptcha(document: Document): Promise<RecaptchaApi> {
  const source = "https://www.google.com/recaptcha/api.js?render=explicit";
  let script = document.querySelector<HTMLScriptElement>(`script[src='${source}']`);
  if (script === null) {
    script = document.createElement("script");
    script.src = source;
    script.async = true;
    script.defer = true;
    document.head.append(script);
  }
  const existing = recaptchaApi(document.defaultView);
  if (existing !== undefined) return Promise.resolve(existing);
  return new Promise((resolve, reject) => {
    script?.addEventListener("load", () => {
      const loaded = recaptchaApi(document.defaultView);
      if (loaded === undefined) reject(new Error("reCAPTCHA loaded without a browser API"));
      else resolve(loaded);
    }, { once: true });
    script?.addEventListener("error", () => reject(new Error("reCAPTCHA could not be loaded")), { once: true });
  });
}

function recaptchaApi(window: Window | null | undefined): RecaptchaApi | undefined {
  return (window as RecaptchaWindow | null | undefined)?.grecaptcha;
}
