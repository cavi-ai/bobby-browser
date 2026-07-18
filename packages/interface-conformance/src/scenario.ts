export type ScenarioProof = {
  submitted: boolean;
  popupObserved: boolean;
  downloadVerified: boolean;
};

export interface ScenarioDriver {
  navigate(url: string): Promise<void>;
  completeForm(): Promise<void>;
  uploadFixture(path: string): Promise<void>;
  submitForm(): Promise<void>;
  observePopup(): Promise<void>;
  screenshot(): Promise<void>;
  verifyDownload(): Promise<void>;
}

export async function runCanonicalScenario(
  driver: ScenarioDriver,
  baseUrl: string,
  fixturePath: string,
): Promise<ScenarioProof> {
  await driver.navigate(baseUrl);
  await driver.completeForm();
  await driver.uploadFixture(fixturePath);
  await driver.submitForm();
  await driver.observePopup();
  await driver.screenshot();
  await driver.verifyDownload();
  return { submitted: true, popupObserved: true, downloadVerified: true };
}
