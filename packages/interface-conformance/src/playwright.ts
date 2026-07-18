import { type CDPSession, type Page } from "playwright-core";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import type { ScenarioDriver } from "./scenario.js";

export function playwrightDriver(page: Page, endpoint: string, token: string, deniedToken: string): ScenarioDriver {
  let automation: Promise<CDPSession> | undefined;
  let eventCursor = 0;
  const automationCdp = () => automation ??= (async () => {
    const browser=page.context().browser(); if(!browser) throw new Error("browser is unavailable"); return browser.newBrowserCDPSession();
  })();
  return {
    navigate: async (url) => { await page.goto(url); const bytes = Buffer.from(url); return evidence("navigation", bytes); },
    completeForm: async () => {
      await page.getByLabel("Name").fill("Ada");
      await page.getByRole("button", { name: "Continue" }).click();
      await page.getByLabel("Company").fill("Analytical Engines");
    },
    submitForm: async () => {
      await page.getByRole("button", { name: "Submit" }).click();
      await page.getByText("Submitted: Ada @ Analytical Engines").waitFor();
    },
    uploadFixture: async (path) => { const bytes = await readFile(path); await page.getByLabel("Resume").setInputFiles(path); return evidence("upload", bytes); },
    observePopup: async () => {
      const popup = page.waitForEvent("popup");
      await page.getByRole("link", { name: "Open details" }).click();
      const opened = await popup;
      if (!opened.url()) throw new Error("popup target did not expose a verified URL");
    },
    screenshot: async () => { const bytes = await page.screenshot(); return evidence("screenshot", bytes); },
    verifyDownload: async () => {
      const browser = page.context().browser();
      if (!browser) throw new Error("browser is unavailable");
      const cdp = await browser.newBrowserCDPSession();
      const completion = new Promise<{ streamId: string; sha256: string; totalBytes: number }>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("download stream identity timed out")), 30_000);
        cdp.on("Browser.downloadProgress", (event: Record<string, unknown>) => {
          if (event.state !== "completed") return;
          clearTimeout(timer);
          const { streamId, sha256, totalBytes } = event;
          if (typeof streamId !== "string" || typeof sha256 !== "string" || typeof totalBytes !== "number") {
            reject(new Error("download completion lacked verified stream metadata"));
            return;
          }
          resolve({ streamId, sha256, totalBytes });
        });
      });
      const download = page.waitForEvent("download");
      await page.getByRole("link", { name: "Download fixture" }).click();
      await download;
      const expected = await completion;
      const response = await fetch(`${endpoint}/v1/streams/${encodeURIComponent(expected.streamId)}`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (!response.ok || !response.body) throw new Error(`download stream failed: ${response.status}`);
      const chunks: Uint8Array[] = [];
      let bytes = 0;
      for await (const chunk of response.body) {
        const value = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
        chunks.push(value);
        bytes += value.byteLength;
      }
      const digest = createHash("sha256");
      for (const chunk of chunks) digest.update(chunk);
      if (bytes !== expected.totalBytes || digest.digest("hex") !== expected.sha256)
        throw new Error("download stream integrity verification failed");
      await cdp.detach();
      return { kind: "download", sha256: expected.sha256, size: expected.totalBytes };
    },
    verifyDeniedCapability: async () => {
      const response=await fetch(`${endpoint}/json/version`,{headers:{Authorization:`Bearer ${deniedToken}`}});
      if(response.status!==403) throw new Error(`missing session:read discovery returned ${response.status}`);
      return response.status;
    },
    checkpoint: async () => (await automationCdp() as unknown as RawSession).send("Automation.checkpointSave"),
    recover: async () => (await automationCdp() as unknown as RawSession).send("Automation.recoveryInspect"),
    readEvents: async () => {
      const batch = await (await automationCdp() as unknown as RawSession).send("Automation.eventsRead", { cursor: eventCursor });
      eventCursor = batch.latestAvailable;
      return batch;
    },
    protocolInventory: async () => (await automationCdp() as unknown as RawSession).send("Automation.protocolInventory"),
  };
}
type RawSession={send(method:string,params?:Record<string,unknown>):Promise<any>};

function evidence(kind: "navigation" | "upload" | "screenshot", bytes: Uint8Array) {
  return { kind, sha256: createHash("sha256").update(bytes).digest("hex"), size: bytes.byteLength };
}
