import type { Page } from "playwright-core";
import type { ScenarioDriver } from "./scenario.js";

export function playwrightDriver(page: Page): ScenarioDriver {
  return {
    navigate: async (url) => { await page.goto(url); },
    completeForm: async () => {
      await page.getByLabel("Name").fill("Ada");
      await page.getByRole("button", { name: "Continue" }).click();
      await page.getByLabel("Company").fill("Analytical Engines");
    },
    submitForm: async () => {
      await page.getByRole("button", { name: "Submit" }).click();
      await page.getByText("Submitted: Ada @ Analytical Engines").waitFor();
    },
    uploadFixture: async (path) => { await page.getByLabel("Resume").setInputFiles(path); },
    observePopup: async () => {
      const popup = page.waitForEvent("popup");
      await page.getByRole("link", { name: "Open details" }).click();
      const opened = await popup;
      if (!opened.url()) throw new Error("popup target did not expose a verified URL");
    },
    screenshot: async () => { await page.screenshot(); },
    verifyDownload: async () => {
      const download = page.waitForEvent("download");
      await page.getByRole("link", { name: "Download fixture" }).click();
      await (await download).createReadStream();
    },
  };
}
