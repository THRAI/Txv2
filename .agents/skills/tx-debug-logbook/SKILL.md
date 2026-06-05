---
name: tx-debug-logbook
description: Use after debugging a txKernel failure, regression, hang, CI failure, LTP/OSComp case, or rebase fallout to write a detailed local debug log under /home/msp/learning/Txv2/msp plus a concise repo progress summary when needed.
---

# tx-debug-logbook

Use this skill when a debug loop reaches a conclusion: fixed, intentionally
deferred, or proven to be an external/harness blocker. The detailed log belongs
under `/home/msp/learning/Txv2/msp/`, which is intentionally untracked. Keep the
repository docs to concise progress ledgers and stable conclusions.

## Read First

- `docs/progress/STATUS.md`
- `.agents/skills/tx-progress-memory/SKILL.md`
- For LTP network work: `docs/LTP/ltp-network-syscall-progress.md`

## What To Record

Every debug log or status entry must include:

- date, branch, and relevant commit if known
- exact witness command
- serial/log path
- local judge or test output summary
- first failing case or last progress line before a hang
- observed symptom: errno, `TFAIL`, `TBROK`, timeout, trap, panic, or SIGSEGV
- root cause, with file/function pointers when possible
- fix made, or reason no kernel fix is correct
- verification run after the fix
- next step and blocker

If the issue is a known environment or unsupported-protocol gate, say that
plainly. Do not leave it as "failed".

## Where To Write

- Detailed debug logs: write
  `/home/msp/learning/Txv2/msp/debug-logs/YYYY-MM-DD-short-title.md`.
  Create the directory if needed. Do not `git add msp/`.
- Short current-state checkpoint: update `docs/progress/STATUS.md` only when
  behavior changed, a blocker was classified, or the result affects future
  work.
- LTP socket/network score ledger: update
  `docs/LTP/ltp-network-syscall-progress.md` when a case score, blocker, or
  recommended run changes.
- Broader workflow or architecture choice: add a repo decision/research note
  only when the conclusion is a stable project contract that belongs in review.
  Scratch logs, command transcripts, rebase notes, and long root-cause
  narratives stay in `msp/debug-logs/`.

Use `docs/progress/research/2026-05-21-recvmmsg-musl-wrapper-blocker.md` as a
model for the shape of a "not a kernel semantic bug" write-up, but put new
ordinary debug write-ups in `msp/debug-logs/` unless the user explicitly wants a
repo-tracked research note.

## Minimal Template

```md
# <short title>

Date: YYYY-MM-DD
Branch: <branch>

## Witness

- Command: `<command>`
- Log: `<path>`
- Score/output: `<judge or test summary>`

## Symptom

<first failing case, last progress line, errno, trap, timeout, or SIGSEGV>

## Root Cause

<why it happened and which layer owns it>

## Resolution

<semantic fix, or why no kernel fix is appropriate>

## Verification

- `<host unit test or build>`
- `<focused LTP/OSComp witness>`

## Next

<next focused case or blocker>
```

## Rules

- Do not rely on memory or chat-only conclusions.
- Do not add or commit `msp/`; it is the local scratch/debug-log area.
- Do not record `FAIL LTP CASE ... : 0` as failure without checking the local
  judge; that line can be an OSComp end marker.
- Do not hide unsupported protocol/environment blockers behind fake success.
- Keep the log useful for rebase repair: name the semantic owner and the test
  that would catch regression.
