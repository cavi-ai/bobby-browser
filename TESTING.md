# Testing

- Evidence of real-site behavior: only the live tests that drive real sites through a real signed-in browser (`crates/cli/tests/live_*.rs`, `#[ignore]`, run on demand).
- Every other test in this repository uses synthetic fixtures, local pages or mocks and is suspect: it guards compilation and internal logic only.
- A passing synthetic test is never cited as proof that a site works.
