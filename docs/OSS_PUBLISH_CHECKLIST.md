# OSS publish checklist

Prep checklist before a **human** flips this repository to public. Completing
this list does **not** change GitHub visibility.

## Docs artifact

- [ ] `pnpm docs:build` succeeds
- [ ] `pnpm docs:verify` succeeds
- [ ] `pnpm docs:test` succeeds
- [ ] `docs/bobby-browser/CONSUMER.md` matches the built `v0.2.0` paths
- [ ] Host (cavi-home) can copy `docs/bobby-browser/v0.2.0` after validation
      (host route wiring is a separate change)

## Public-ready tree

- [ ] No agent instruction clones tracked (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `QODER.md`)
- [ ] No `docs/superpowers/plans/` or `docs/superpowers/specs/` in the public tree
- [ ] No secrets, private absolute paths, or bearer tokens in committed files
- [ ] `LICENSE`, `SECURITY.md`, `CONTRIBUTING.md`, thin `README.md` present
- [ ] Full product source (crates / packages / schemas) still present

## Final human step (out of scope for prep automation)

- [ ] Maintainer flips GitHub repository visibility to public
- [ ] Confirm CI public-only workflow behaves as expected on the public repo
- [ ] Confirm online docs link `https://cavi-ai.xyz/docs/bobby-browser` resolves
      after host wiring
