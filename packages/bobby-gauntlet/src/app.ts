import { GauntletController } from "./controller.js";
import { CHAMPIONSHIP_COURSE_VERSION, CHAMPIONSHIP_STATION_MANIFEST, createManifest } from "./manifest.js";
import { sha256Hex, type Difficulty, type RegisteredStation, type StationResult } from "./station.js";
import { championshipStation, type ChampionshipState } from "./stations/championship.js";
import { domDriftStation, type DomDriftState } from "./stations/dom-drift.js";
import { downloadStation, type DownloadState } from "./stations/download.js";
import { fileAttachmentStation, type FileAttachmentState } from "./stations/file-attachment.js";
import { iframeStation, type IframeState } from "./stations/iframe.js";
import { popupStation, type PopupState } from "./stations/popup.js";
import { routeStation, type RouteState } from "./stations/route.js";
import { semanticFormStation, type SemanticFormState } from "./stations/semantic-form.js";
import { shadowRootStation, type ShadowRootState } from "./stations/shadow-root.js";
import { validationStation, type ValidationState } from "./stations/validation.js";

export interface FoundationStates {
  route: RouteState;
  "dom-drift": DomDriftState;
  "semantic-form": SemanticFormState;
  validation: ValidationState;
}

export interface ChampionshipStates extends FoundationStates {
  iframe: IframeState;
  "shadow-root": ShadowRootState;
  popup: PopupState;
  "file-attachment": FileAttachmentState;
  download: DownloadState;
  championship: ChampionshipState;
}

export const FOUNDATION_STATIONS = Object.freeze([routeStation, domDriftStation, semanticFormStation, validationStation]);
export const CHAMPIONSHIP_STATIONS = Object.freeze([...FOUNDATION_STATIONS, iframeStation, shadowRootStation, popupStation, fileAttachmentStation, downloadStation, championshipStation]);
const STATION_IDS = new Set(CHAMPIONSHIP_STATIONS.map((station) => station.id));

export function createFoundationController(courseVersion: string, seed: string, difficulty: Difficulty): GauntletController<FoundationStates> {
  if (difficulty !== "foundation") throw new Error("foundation gauntlet does not support this difficulty");
  return new GauntletController(createManifest(courseVersion, seed, difficulty, CHAMPIONSHIP_STATION_MANIFEST.slice(0, 4)), FOUNDATION_STATIONS);
}

export function createChampionshipController(courseVersion: string, seed: string, difficulty: Difficulty): GauntletController<ChampionshipStates> {
  if (courseVersion !== CHAMPIONSHIP_COURSE_VERSION || difficulty !== "foundation") throw new Error("championship gauntlet does not support this course or difficulty");
  return new GauntletController(createManifest(courseVersion, seed, difficulty, CHAMPIONSHIP_STATION_MANIFEST), CHAMPIONSHIP_STATIONS as readonly RegisteredStation[]);
}

/** Mounts direct station routes and the full championship without serializing controller state into page text. */
export function mountGauntlet(root: HTMLElement, pathname: string, search: string): void {
  try {
    const document = root.ownerDocument;
    const seed = browserSeed(document, search);
    const difficulty = browserDifficulty(search);
    const controller = createChampionshipController("course-v1", seed, difficulty);
    root.replaceChildren();
    const main = document.createElement("main");
    const heading = document.createElement("h1");
    heading.textContent = "Bobby Gauntlet";
    main.append(heading);
    if (pathname.replace(/\/+$/, "") === "/championship") {
      for (const descriptor of controller.manifest.stations) main.append(renderSurface(document, descriptor.id as keyof ChampionshipStates, controller, true));
    } else {
      main.append(renderSurface(document, stationIdFor(pathname), controller, false));
    }
    root.append(main);
  } catch {
    root.replaceChildren(Object.assign(root.ownerDocument.createElement("p"), { textContent: "Configuration unavailable." }));
  }
}

function renderSurface(document: Document, stationId: keyof ChampionshipStates, controller: GauntletController<ChampionshipStates>, sharedChampionship: boolean): HTMLElement {
  const station = document.createElement("section");
  station.dataset.stationId = stationId;
  const result = document.createElement("output");
  result.dataset.testid = "result";
  result.setAttribute("aria-live", "polite");
  station.append(result);
  const report = (value: StationResult) => {
    result.textContent = value.passed ? "Passed" : value.failure.guidance;
    station.querySelector("script[data-testid=station-scorecard]")?.remove();
    const receipt = document.createElement("script");
    receipt.type = "application/json";
    receipt.dataset.testid = "station-scorecard";
    receipt.textContent = JSON.stringify(controller.scorecard());
    station.append(receipt);
    if (sharedChampionship && controller.scorecard().passed) {
      document.querySelector("script[data-testid=championship-scorecard]")?.remove();
      const finalReceipt = document.createElement("script");
      finalReceipt.type = "application/json";
      finalReceipt.dataset.testid = "championship-scorecard";
      finalReceipt.textContent = JSON.stringify(controller.finalizeScorecard({
        engine: "app",
        activeSkills: [],
        recoveryCount: 0,
        strategyChanges: [],
        durationMs: 0,
      }));
      station.append(finalReceipt);
    }
  };
  const clearOutcome = () => {
    result.textContent = "";
    station.querySelector("script[data-testid=station-scorecard]")?.remove();
  };
  switch (stationId) {
    case "route": renderRoute(document, station, controller, report, sharedChampionship); break;
    case "dom-drift": renderDomDrift(document, station, controller, report); break;
    case "semantic-form": renderSemanticForm(document, station, controller, report); break;
    case "validation": renderValidation(document, station, controller, report); break;
    case "iframe": renderIframe(document, station, controller, report); break;
    case "shadow-root": renderShadowRoot(document, station, controller, report); break;
    case "popup": renderPopup(document, station, controller, report); break;
    case "file-attachment": renderFileAttachment(document, station, controller, report, clearOutcome); break;
    case "download": renderDownload(document, station, controller, report, clearOutcome); break;
    case "championship": renderChampionship(document, station, controller, report); break;
  }
  return station;
}

function stationIdFor(pathname: string): keyof ChampionshipStates {
  const candidate = pathname.split("/").filter(Boolean).at(-1);
  return candidate !== undefined && STATION_IDS.has(candidate) ? candidate as keyof ChampionshipStates : "route";
}

function renderRoute(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void, sharedChampionship: boolean): void {
  station.prepend(title(document, "Canonical navigation", "Follow the visible navigation control and verify that the canonical route was reached."));
  if (sharedChampionship) {
    const frame = document.createElement("iframe");
    frame.dataset.testid = "route-challenge";
    frame.title = "Canonical navigation challenge";
    frame.src = `/station/route/?seed=${encodeURIComponent(controller.manifest.seed)}&difficulty=${encodeURIComponent(controller.manifest.difficulty)}`;
    frame.addEventListener("load", () => {
      try {
        const child = frame.contentWindow;
        if (child === null) return;
        const path = observedPath(child);
        if (child.location.pathname === "/station/route/complete/") {
          report(controller.verify("route", { url: path }));
        } else if (child.location.pathname !== "/station/route/" && child.location.pathname !== "/station/route/redirect/") {
          report(controller.verify("route", { url: path }));
        }
      } catch {
        report(controller.verify("route", {}));
      }
    });
    station.append(frame);
    return;
  }
  const window = document.defaultView;
  if (window !== null && window.location.pathname === "/station/route/complete/") {
    report(controller.verify("route", { url: observedPath(window) }));
    return;
  }
  const redirect = document.createElement("a");
  redirect.href = "./redirect/";
  redirect.textContent = "Follow canonical redirect";
  redirect.dataset.testid = "route-redirect";
  station.append(redirect);
}

function renderDomDrift(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  station.prepend(title(document, "Delayed replacement", "Wait for the target to be replaced, then act only on the stable target."));
  const initial = buttonFor(document, "Act on initial target");
  initial.dataset.testid = "initial-target";
  initial.addEventListener("click", () => report(controller.verify("dom-drift", { targetId: controller.stateFor("dom-drift").initialTargetId })));
  station.append(initial);
  setTimeout(() => {
    const replacement = buttonFor(document, "Act on stable target");
    replacement.dataset.testid = "replacement-target";
    replacement.addEventListener("click", () => report(controller.verify("dom-drift", { targetId: controller.stateFor("dom-drift").replacementTargetId })));
    initial.replaceWith(replacement);
  }, 10);
}

function renderSemanticForm(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  const state = controller.stateFor("semantic-form");
  const form = document.createElement("form");
  form.append(title(document, "Semantic form", "Complete the form by the meaning of each labelled control."));
  const name = labelledInput(document, "Full name", state.fields.name, "text");
  const email = labelledInput(document, "Email address", state.fields.email, "email");
  const planLabel = document.createElement("label"); planLabel.textContent = "Plan";
  const plan = document.createElement("select"); plan.name = state.fields.plan; plan.setAttribute("aria-label", "Plan");
  for (const value of ["starter", "pro"]) { const option = document.createElement("option"); option.value = value; option.textContent = value === "pro" ? "Professional" : "Starter"; plan.append(option); }
  planLabel.append(plan);
  form.append(name.label, email.label, planLabel, buttonFor(document, "Submit form", "submit"));
  form.addEventListener("submit", (event) => { event.preventDefault(); report(controller.verify("semantic-form", { values: { [state.fields.name]: name.input.value, [state.fields.email]: email.input.value, [state.fields.plan]: plan.value } })); });
  station.prepend(form);
}

function renderValidation(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  const state = controller.stateFor("validation");
  const form = document.createElement("form");
  form.append(title(document, "Validation correction", "Preserve the accepted value and correct the rejected value using a five-digit replacement."));
  const accepted = labelledInput(document, "Accepted reference", state.validField, "text", state.validValue);
  const rejected = labelledInput(document, "Rejected value", state.invalidField, "text", state.invalidValue);
  rejected.input.pattern = "[0-9]{5}"; rejected.input.minLength = 5; rejected.input.required = true;
  const feedback = document.createElement("p"); feedback.setAttribute("role", "alert"); feedback.id = "validation-feedback"; rejected.input.setAttribute("aria-describedby", feedback.id);
  form.append(accepted.label, rejected.label, buttonFor(document, "Correct and submit", "submit"), feedback); form.noValidate = true;
  form.addEventListener("submit", (event) => { event.preventDefault(); rejected.input.setCustomValidity(""); feedback.textContent = ""; if (!rejected.input.checkValidity()) { rejected.input.setCustomValidity("Enter a five-digit correction."); feedback.textContent = "Enter a five-digit correction."; } const browserValid = rejected.input.checkValidity(); const stationResult = controller.verify("validation", { values: { [state.validField]: accepted.input.value, [state.invalidField]: rejected.input.value } }); if (browserValid && stationResult.passed) feedback.textContent = ""; report(stationResult); });
  station.prepend(form);
}

function renderIframe(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  station.prepend(title(document, "Nested iframe", "Complete the action inside the embedded document."));
  const frame = document.createElement("iframe"); frame.dataset.testid = "iframe-challenge"; frame.title = "Embedded Bobby challenge";
  frame.name = "bobby-iframe-challenge";
  station.append(frame);
  let initialized = false;
  const populate = () => {
    if (initialized) return;
    const frameDocument = frame.contentDocument;
    if (frameDocument === null || frameDocument.body === null) return;
    initialized = true;
    const action = buttonFor(frameDocument, "Complete embedded action"); action.dataset.testid = "iframe-submit";
    action.addEventListener("click", () => report(controller.verify("iframe", { action: controller.stateFor("iframe").action })));
    frameDocument.body.replaceChildren(action);
  };
  frame.addEventListener("load", populate, { once: true });
  setTimeout(populate, 0);
  setTimeout(() => { if (!initialized) report(controller.verify("iframe", {})); }, 250);
}

function renderShadowRoot(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  station.prepend(title(document, "Open shadow root", "Inspect the component and complete its visible action."));
  const host = document.createElement("div"); host.dataset.testid = "shadow-host"; const shadow = host.attachShadow({ mode: "open" });
  const action = buttonFor(document, "Complete component action"); action.dataset.testid = "shadow-submit";
  action.addEventListener("click", () => report(controller.verify("shadow-root", { action: controller.stateFor("shadow-root").action })));
  shadow.append(action); station.append(host);
}

function renderPopup(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  station.prepend(title(document, "Popup completion", "Open the companion window and complete its visible action."));
  const opener = buttonFor(document, "Open companion window"); opener.dataset.testid = "popup-open";
  const complete = () => report(controller.verify("popup", { completion: controller.stateFor("popup").completion }));
  opener.addEventListener("click", () => {
    try {
      const opened = document.defaultView?.open("", "bobby-popup", "popup,width=420,height=240");
      if (opened === null || opened === undefined) { report(controller.verify("popup", {})); return; }
      const popup = opened;
      const popupDocument = popup.document; popupDocument.title = "Bobby companion"; popupDocument.body.replaceChildren();
      const action = popupDocument.createElement("button"); action.type = "button"; action.textContent = "Complete popup action";
      action.addEventListener("click", () => { complete(); popup.close(); }, { once: true }); popupDocument.body.append(action);
    } catch { report(controller.verify("popup", {})); }
  });
  station.append(opener);
}

function renderFileAttachment(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void, clearOutcome: () => void): void {
  station.prepend(title(document, "Approved file attachment", "Attach the approved fixture and submit only after browser-side byte verification."));
  const form = document.createElement("form"); const input = document.createElement("input"); input.type = "file"; input.accept = "text/plain"; input.setAttribute("aria-label", "Approved file");
  let attachment: { name: string; digest: string } | undefined;
  input.addEventListener("change", latestFileReceipt(input, (receipt) => { attachment = receipt; }, clearOutcome));
  form.append(input, buttonFor(document, "Submit attachment", "submit"));
  form.addEventListener("submit", (event) => { event.preventDefault(); report(controller.verify("file-attachment", attachment ?? {})); });
  station.append(form);
}

function renderDownload(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void, clearOutcome: () => void): void {
  station.prepend(title(document, "Generated download", "Generate the file, then confirm the generated artifact."));
  const generate = buttonFor(document, "Generate download"); generate.dataset.testid = "download-generate";
  let downloadClicked = false;
  let receiptDigest: string | undefined;
  generate.addEventListener("click", () => {
    const bytes = controller.stateFor("download").bytes;
    const link = document.createElement("a"); link.download = "bobby-artifact.txt"; link.textContent = "Download generated artifact";
    link.href = `data:text/plain;base64,${encodeBase64(bytes)}`;
    link.addEventListener("click", () => { downloadClicked = true; }, { once: true });
    station.append(link);
    const input = document.createElement("input"); input.type = "file"; input.accept = "text/plain"; input.setAttribute("aria-label", "Downloaded artifact");
    const confirm = buttonFor(document, "Confirm generated download"); confirm.dataset.testid = "download-confirm"; confirm.disabled = true;
    input.addEventListener("change", latestFileReceipt(input, (receipt, pending) => {
      receiptDigest = receipt?.digest;
      confirm.disabled = pending || receipt === undefined;
    }, clearOutcome));
    confirm.addEventListener("click", () => {
      if (confirm.disabled || receiptDigest === undefined) return;
      report(controller.verify("download", { downloaded: downloadClicked, digest: receiptDigest }));
    });
    station.append(input, confirm);
  }, { once: true });
  station.append(generate);
}

function renderChampionship(document: Document, station: HTMLElement, controller: GauntletController<ChampionshipStates>, report: (result: StationResult) => void): void {
  station.prepend(title(document, "Combined championship", "Complete every visible step in order before submitting."));
  const state = controller.stateFor("championship"); let index = 0;
  const advance = () => { if (index >= state.steps.length) return; const button = buttonFor(document, index === state.steps.length - 1 ? "Submit championship" : `Complete step ${index + 1}`); button.dataset.testid = `championship-step-${index + 1}`; button.addEventListener("click", () => { index += 1; if (index === state.steps.length) report(controller.verify("championship", { steps: state.steps })); else advance(); }, { once: true }); station.append(button); };
  advance();
}

function title(document: Document, heading: string, instructions: string): DocumentFragment { const fragment = document.createDocumentFragment(); const title = document.createElement("h2"); title.textContent = heading; const copy = document.createElement("p"); copy.textContent = instructions; fragment.append(title, copy); return fragment; }
function labelledInput(document: Document, labelText: string, name: string, type: string, value = ""): { label: HTMLLabelElement; input: HTMLInputElement } { const label = document.createElement("label"); label.textContent = labelText; const input = document.createElement("input"); input.name = name; input.type = type; input.value = value; input.setAttribute("aria-label", labelText); label.append(input); return { label, input }; }
function buttonFor(document: Document, copy: string, type: "button" | "submit" = "button"): HTMLButtonElement { const button = document.createElement("button"); button.type = type; button.textContent = copy; return button; }
function observedPath(window: Window): string { return `${window.location.pathname}${window.location.search}`; }
function browserSeed(document: Document, search: string): string { const supplied = new URLSearchParams(search).get("seed"); const window = document.defaultView; if (supplied !== null) { window?.sessionStorage.setItem("bobby.gauntlet.seed", supplied); return supplied; } return window?.sessionStorage.getItem("bobby.gauntlet.seed") ?? "demo-seed"; }
function browserDifficulty(search: string): Difficulty { const difficulty = new URLSearchParams(search).get("difficulty") ?? "foundation"; if (difficulty !== "foundation") throw new Error("unsupported difficulty"); return difficulty; }
function readFileText(file: File): Promise<string> { if (typeof file.text === "function") return file.text(); return new Promise((resolve, reject) => { const reader = new FileReader(); reader.addEventListener("load", () => resolve(typeof reader.result === "string" ? reader.result : ""), { once: true }); reader.addEventListener("error", () => reject(new Error("file read failed")), { once: true }); reader.readAsText(file); }); }
function latestFileReceipt(input: HTMLInputElement, publish: (receipt: { name: string; digest: string } | undefined, pending: boolean) => void, clearOutcome: () => void): () => Promise<void> {
  let generation = 0;
  return async () => {
    const current = ++generation;
    clearOutcome();
    publish(undefined, true);
    const files = input.files;
    const file = files === null ? undefined : typeof files.item === "function" ? files.item(0) : files[0];
    if (file === null || file === undefined) {
      if (current === generation) publish(undefined, false);
      return;
    }
    try {
      const digest = sha256Hex(await readFileText(file));
      if (current === generation) publish({ name: file.name, digest }, false);
    } catch {
      if (current === generation) publish(undefined, false);
    }
  };
}
function encodeBase64(value: string): string { return btoa(new TextEncoder().encode(value).reduce((encoded, byte) => encoded + String.fromCharCode(byte), "")); }

if (typeof document !== "undefined") mountGauntlet(document.querySelector<HTMLElement>("#app") ?? document.body, window.location.pathname, window.location.search);

