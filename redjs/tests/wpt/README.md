# Vendored web-platform-tests: IndexedDB

Test fixtures only. Nothing here is compiled into redjs or shipped with any
build; `tests/wpt_idb.rs` reads these files at test time.

## Where it came from

- Repository: https://github.com/web-platform-tests/wpt
- Commit: `3b3e763e92976d6df99ad9972c9b29c1b7bf2cef` (fetched 2026-10-28)
- Files, unmodified:
  - `IndexedDB/*.any.js` — every `.any.js` test in `IndexedDB/` at that commit
    except `idlharness.any.js` (needs WebIDLParser/idlharness, P3) and
    `storage-buckets.https.any.js` (Storage Buckets, not implemented: IDB-D7)
  - `IndexedDB/crashtests/create-index.any.js`
  - `IndexedDB/resources/*.js` — the shared helpers, minus the two worker
    scripts (`idbworker.js`, `transaction-inflight-worker-terminate-worker.js`):
    redjs has no Workers yet
  - `IndexedDB/META.yml`, `IndexedDB/README.md`
  - `resources/testharness.js`
  - `common/subset-tests.js` (`structured-clone.any.js` loads it)

## License

The web-platform-tests project's 3-Clause BSD License; the full text is in
[`LICENSE.md`](LICENSE.md), copied from the repository root at the same commit.

## How it is run

`tests/wpt_idb.rs`: a fresh context per file, `document` deleted so
testharness picks its shell environment, `// META: script=` lines expanded,
pumped with the browser-exact loop (a checkpoint per task) until the
completion callback fires or the per-file limit passes (10 s; 60 s for
`timeout=long`). Every subtest is compared with `expectations.txt`: anything
not listed there must PASS, anything listed must NOT pass — a regression and
an unexpected pass both fail the run.

- `cargo test --release --test wpt_idb` — the smoke tier (the per-change gate).
- `cargo test --release --test wpt_idb -- --ignored` — the gate tier (every
  vendored file).
- `REDJS_WPT_REPORT=<file>` writes a per-file summary there.
- `REDJS_WPT_ONLY=<substring>` runs only matching files (either tier).

To update: re-fetch the same paths at a new commit, update the commit above,
and re-baseline `expectations.txt` from a gate run.
