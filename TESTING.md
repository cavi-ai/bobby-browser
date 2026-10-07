# Testing

- Evidence of real-site behavior: only the live tests that drive real sites through a real signed-in browser (`crates/cli/tests/live_*.rs`, `#[ignore]`, run on demand).
- Every other test in this repository uses synthetic fixtures, local pages or mocks and is suspect: it guards compilation and internal logic only.
- A passing synthetic test is never cited as proof that a site works.
- `make test-browsers-setup`: run once; builds bobby and the companion, creates `.tmp/test-browsers` (Firefox profile, companion copy scoped to the host `com.bobby_browser.companion.scope_7465737462726f77`), and installs only that test native host manifest, never `com.bobby_browser.companion.json`.
- `make test-browsers`: runs `site_regressions_*` and `journeys_*` headless on real Chromium, then on Firefox (Developer Edition on macOS) against the test profile; the Firefox part exits with "run make test-browsers-setup" when the test host manifest is absent.
