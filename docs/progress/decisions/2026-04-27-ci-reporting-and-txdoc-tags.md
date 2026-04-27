# Decision: CI Reporting And txdoc Tags

**Date:** 2026-04-27

## Decision

- Add `docs/design/00_meta-framework/CI_REPORTING_v1.md` as the CI reporting
  contract.
- Add grep-stable `txdoc:` tags to active design docs.
- Add `cargo xtask ci` as the CI-facing reporter.
- Add `cargo xtask progress validate` for typed progress JSON validation.
- Add a GitHub Actions workflow that runs `cargo xtask ci`.

## Context

CI output needs to be useful to humans and agents: passing checks should be
brief, failures should include details, and every gate should point back to the
design rule it protects. Design docs also need stable references that do not
depend on Markdown heading text.

## Consequences

- `cargo xtask lint docs` now requires every active design doc to carry at least
  one `txdoc:` tag and rejects duplicate tags.
- CI gate output references `docs/design/00_meta-framework/CI_REPORTING_v1.md`
  tags such as `txdoc:CI-GATE-FMT` and `txdoc:CI-GATE-UNIT-TESTS`.
- Future feature work should add tests near the implementation and ensure the
  relevant plan's JSON `verification` array names the check that covers it.

## Alternatives Considered

- Use Markdown heading anchors only. They are readable but drift when headings
  are renamed.
- Print full command logs for passing checks. That makes CI noisy and hides the
  interesting failure details.
