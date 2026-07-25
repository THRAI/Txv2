# Development Environment Storage Audit

## Scope

Read-only audit of the macOS Data volume and the txKernel checkout at
`/Users/3y/Downloads/Tx`. No files were deleted or modified by the audit.

## Findings

- The Data volume is critically full: `460 GiB` total, about `420 GiB` used,
  and only `4.3-4.4 GiB` available (`99%`). APFS reports about `5.6 GB`
  container free space.
- The Tx checkout uses about `114 GiB`. The dominant regenerable build outputs
  are `target/debug/deps` (`69 GiB`) and `target/debug/incremental` (`15 GiB`).
  The incremental tree contains about 970 historical state directories, with
  repeated `tx_subsystems`, `tx_shims`, `tx_kernel`, and related test builds.
- Other Tx-local measured usage is comparatively small: `.git` (`1019 MiB`),
  `.codegraph` (`157 MiB`), `target/oscomp` (`1.2 GiB`), `target/images`
  (`166 MiB`), and `target/rootfs` (`108 MiB`).
- Other large development/user artifacts include Docker Desktop's sparse
  `Docker.raw` (460 GiB apparent, 21 GiB allocated), Colima disks (about 2.2
  GiB allocated), Android emulator userdata (about 13.3 GiB), a Homebrew
  MacTeX package (6.4 GiB), Whisper models (4.3 GiB), and a Baidu Netdisk
  cache database (1.2 GiB).
- At the directory level, `Downloads` is about `115 GiB`, `Library/Containers`
  at least `51 GiB`, `Library/Application Support` at least `29 GiB`, and
  `Library/Caches` at least `23 GiB`. Some Library paths are protected by
  macOS privacy controls, so these are lower bounds.
- No large deleted-open file was found. Docker is not currently reachable via
  the Colima socket. Three non-purgeable Apple OS update snapshots are present;
  their contribution was not independently sized and they should not be
  removed as part of a project-cache cleanup.

## Verification

- `df -h /Users/3y/Downloads/Tx`
- `gdu -x -h -s` on the checkout and targeted build/cache paths
- `git count-objects -vH`
- `diskutil info /System/Volumes/Data`
- `diskutil apfs listSnapshots /` and `tmutil listlocalsnapshots /`
- `lsof -nP +L1`
- Direct `du`/`stat` checks for sparse VM disks and large files

## Next action

After active builds stop, reclaim space in this order: stale Tx `target/debug`
outputs (especially `deps` and `incremental`), unused Android emulator userdata,
the cached MacTeX installer, and unused Whisper/Baidu caches. Docker/Colima
storage should be cleaned through their own lifecycle tools or UI, not by
deleting disk files directly. Re-measure with `df -h` after each bounded batch.

## Blockers and cautions

- The checkout is heavily dirty with user changes and untracked files; do not
  use broad Git cleanup or remove source-side artifacts without explicit review.
- A generic `cargo clean` would reclaim much of `target/` but would discard all
  local build state; it was not run. macOS privacy restrictions prevent a
  complete Library attribution from this shell session.

## Cleanup Follow-up

After confirming that no `cargo`, `rustc`, or `rustdoc` process was active, all
entries under `target/debug/deps` were removed while keeping the directory. The
directory fell from `69 GiB` to `0`, and Data-volume free space rose from about
`4.3 GiB` to `68 GiB`. The subsequent `cargo xtask lint docs` and `cargo xtask
progress validate` checks rebuilt only the required host artifacts, leaving
`target/debug/deps` at `225 MiB` with 520 entries. `target/debug/incremental`
was intentionally retained at `15 GiB` for local rebuild reuse.

The recommended policy for this checkout is:

- Keep one default `target/` for QEMU paths that explicitly consume root
  `target/` artifacts.
- Use the existing `target/host-cargo` for host-side OSComp submission lanes.
- Use `CARGO_INCREMENTAL=0` for CI, one-shot sweeps, and disk-constrained
  verification, preferably with a disposable `CARGO_TARGET_DIR`.
- Keep incremental compilation enabled only for the active local edit loop;
  periodically remove `target/debug/incremental` when it grows beyond a chosen
  budget or after a large branch/configuration change.
- Never clean a target directory while a compiler process is using it. Prefer
  `cargo clean --profile dev --target-dir <dir>` for a whole profile cleanup,
  and remove disposable target directories only after their logs/artifacts have
  been retained elsewhere.
