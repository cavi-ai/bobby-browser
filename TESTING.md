# Testing

- Tests in this repository use synthetic fixtures, local pages or mocks: they guard compilation and internal logic only.
- A passing synthetic test is not proof that a given real site works.
- `make test-browsers-setup`: run once; builds bobby and the companion, creates `.tmp/test-browsers` (Firefox profile, companion copy scoped to the host `com.bobby_browser.companion.scope_7465737462726f77`), and installs only that test native host manifest, never `com.bobby_browser.companion.json`.
- `make test-browsers`: runs `site_regressions_*` and `journeys_*` headless on real Chromium, then on Firefox (Developer Edition on macOS) against the test profile; the Firefox part exits with "run make test-browsers-setup" when the test host manifest is absent.
