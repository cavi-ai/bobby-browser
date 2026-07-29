# Changelog

## Unreleased

- Scope command outcome events to the authenticated principal across HTTP and MCP transports.
- Require session ownership for checkpoint creation and workflow recovery.
- Prevent workflow checkpoints from being rebound to a different session.
- Revalidate checkpoint session identity while holding the recovery lock.
