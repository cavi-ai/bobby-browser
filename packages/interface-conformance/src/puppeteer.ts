import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { basename } from "node:path";
import puppeteer, { type CDPSession, type Page } from "puppeteer-core";
import type { ScenarioDriver } from "./scenario.js";

const PUPPETEER_RUNTIME_TRANSLATOR = "(operation, selector, value) => globalThis.__automationRuntimePuppeteer(operation, selector, value)";

type RuntimePage = Page & { _client(): CDPSession };

async function semantic(page: Page, operation: string, selector: string, value = ""): Promise<void> {
  await (page as RuntimePage)._client().send("Runtime.callFunctionOn", {
    functionDeclaration: PUPPETEER_RUNTIME_TRANSLATOR,
    executionContextId: 3,
    arguments: [{ value: operation }, { value: selector }, { value }],
    returnByValue: true,
    awaitPromise: true,
    userGesture: true,
  });
}

export function puppeteerDriver(page: Page, endpoint: string, token: string, deniedToken: string): ScenarioDriver {
  return {
    navigate: async (url) => { await page.goto(url); const bytes = Buffer.from(url); return evidence("navigation", bytes); },
    completeForm: async () => {
      await semantic(page, "fill", "label:Name", "Ada");
      await semantic(page, "click", "role:button:Continue");
      await semantic(page, "fill", "label:Company", "Analytical Engines");
    },
    uploadFixture: async (path) => {
      const bytes = await readFile(path);
      await semantic(page, "upload", "label:Resume", JSON.stringify({ name: basename(path), base64: bytes.toString("base64") }));
      return evidence("upload", bytes);
    },
    submitForm: async () => {
      await semantic(page, "click", "role:button:Submit");
    },
    observePopup: async () => {
      await semantic(page, "click", "role:link:Open details");
    },
    screenshot: async () => {
      const result = await (page as RuntimePage)._client().send("Page.captureScreenshot") as { data: string };
      return evidence("screenshot", Buffer.from(result.data, "base64"));
    },
    verifyDownload: async () => {
      const cdp = (page as RuntimePage)._client();
      await cdp.send("Browser.setDownloadBehavior", { behavior: "allowAndName", eventsEnabled: true });
      const completion = new Promise<{ streamId: string; sha256: string; totalBytes: number }>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("download stream identity timed out")), 30_000);
        cdp.on("Browser.downloadProgress", event => {
          if (event.state !== "completed") return;
          clearTimeout(timer);
          const { streamId, sha256, totalBytes } = event as typeof event & {
            streamId?: unknown;
            sha256?: unknown;
            totalBytes?: unknown;
          };
          if (typeof streamId !== "string" || typeof sha256 !== "string" || typeof totalBytes !== "number") {
            reject(new Error("download completion lacked verified stream metadata"));
            return;
          }
          resolve({ streamId, sha256, totalBytes });
        });
      });
      await semantic(page, "click", "role:link:Download fixture");
      const expected = await completion;
      const response = await fetch(`${endpoint}/v1/streams/${encodeURIComponent(expected.streamId)}`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (!response.ok || !response.body) throw new Error(`download stream failed: ${response.status}`);
      const digest = createHash("sha256");
      let bytes = 0;
      for await (const chunk of response.body) {
        const value = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
        bytes += value.byteLength;
        digest.update(value);
      }
      if (bytes !== expected.totalBytes || digest.digest("hex") !== expected.sha256)
        throw new Error("download stream integrity verification failed");
      return { kind: "download", sha256: expected.sha256, size: expected.totalBytes };
    },
    verifyDeniedCapability: async () => {
      const discovery = await fetch(`${endpoint}/json/version`, { headers: { Authorization: `Bearer ${deniedToken}` } });
      const version = await discovery.json() as { webSocketDebuggerUrl: string };
      let denied;
      try {
        denied = await puppeteer.connect({ browserWSEndpoint: version.webSocketDebuggerUrl, headers: { Authorization: `Bearer ${deniedToken}` }, defaultViewport: null });
        await denied.newPage(); return 200;
      } catch { return 403; } finally { denied?.disconnect(); }
    },
  };
}

function evidence(kind: "navigation" | "upload" | "screenshot", bytes: Uint8Array) {
  return { kind, sha256: createHash("sha256").update(bytes).digest("hex"), size: bytes.byteLength };
}
