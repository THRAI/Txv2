# Decision: Fine-Grained txdoc Anchors

**Date:** 2026-04-27

## Decision

- Active design docs need section-level `txdoc:` anchors, not only file-level
  anchors.
- `cargo xtask lint docs` rejects active design docs with fewer than two
  `txdoc:` tags and still rejects duplicate or malformed tags.

## Context

A top-of-file anchor is useful for locating a document, but it does not give CI,
plans, reviews, or handoffs a precise reference to the design rule that a check
protects. Section anchors make references grep-stable without depending on
Markdown heading text.

## Consequences

- Active design docs now carry fine-grained anchors across their load-bearing
  sections.
- Future docs must add both a file-level anchor and at least one section-level
  anchor before `cargo xtask lint docs` will pass.
- CI output and implementation plans should prefer the most specific applicable
  `txdoc:` tag.

## Verification

- Active design docs: 38.
- Total active `txdoc:` tags after the pass: 1521.
- Duplicate `txdoc:` tags: none found.
- `cargo xtask ci`: 9 passed, 1 skipped, 0 failed.
