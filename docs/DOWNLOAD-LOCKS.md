# Download lock ownership and recovery

The deployed Python game downloader records a schema-1 `.download.lock` with listing title ID, selected artifact ID, PID, process creation identity, hostname, nonce, UTC creation time and `download`/`pipeline` phase. An OS-held `.download.lock.guard` protects the whole job and serializes marker creation, phase changes, normal release and recovery. The guard file remains on disk; its existence alone is not a busy lock. Do not remove it while workers run.

Windows uses a byte-range lock and process creation FILETIME; Linux uses flock plus boot ID/process start ticks. Inaccessible/unsupported process identity is unknown, not dead. A new downloader refuses to start if it cannot establish its own identity. No process command line, credentials or provider links are recorded.

## Commands (from the deployed PS5 workspace)

```powershell
.\ps5-lock diagnose PPSA06774
.\ps5-lock recover PPSA06774
# With a custom output directory:
.\ps5-lock diagnose PPSA06774 -Out 'D:\PS5\downloads\PPSA06774'
```

Python equivalent:

```text
python scripts/ps5_lock.py diagnose PPSA06774 --out path/to/title-output
python scripts/ps5_lock.py recover PPSA06774 --out path/to/title-output
```

`diagnose` is local and read-only: it creates no directories/markers/guard files and does not query the console. It reports recorded owner, live/gone/reused-PID/foreign/unknown status, OS guard availability, pipeline evidence and whether recovery is eligible. PID reuse is checked against process creation identity, not PID alone. Creation timestamps are diagnostic only; age never establishes staleness.

`recover` requires an exclusive OS guard, a same-host owner confirmed gone (or a different creation identity proving the former PID owner exited), valid owner metadata, a matching requested listing title, download phase and no `.pipeline` evidence. It checks again under the guard, durably archives only the old marker as `.download.lock.recovered-<nonce>.json`, then removes that marker. It never removes partials, source files or receipts, and never starts/resubmits a download or upload. Rerun the original command explicitly; the existing resumable downloader still validates URL/entity/range/digest sidecars.

## Cases requiring separate inspection

- Live owner or busy/inaccessible OS guard: leave the job running or diagnose its owner; no forced takeover.
- Legacy empty/malformed, foreign-host, oversized or linked/shared lock metadata: preserve it. This tool cannot infer which process wrote it.
- Pipeline phase or any `.pipeline` footprint: inspect pipeline-state.json, upload-attempt.json and transfer-receipt.json plus engine status/history before deciding next steps. Even a historical completed pipeline footprint conservatively blocks this download-only recovery helper. No age-based cancellation or blind resubmission.

The marker changes to pipeline phase before invoking extraction/planning/upload, so a child can outlive its downloader without being treated as a recoverable abandoned download. Normal cleanup removes only the marker with its own nonce. This protects the Python game downloader; it does not replace Rust per-job locks, existing PS5Upload reconciliation, homebrew ownership or unrelated historical direct-download scripts.

## Verification

117 Python tests passed, including live and abruptly exited real child processes, same PID/different creation identity, unknown/foreign/legacy owners, OS guard exclusion, no-file-change diagnosis, marker replacement, title mismatch and pipeline evidence. Existing extraction/planning tests use synthetic archives and mocked console submission. No real game transfer or console state changed.

The full suite exposed a Windows GNU CLI stack overflow in a small homebrew extraction fixture. `.cargo/config.toml` now reserves an 8 MiB main stack for that target; the local optimized GNU CLI was rebuilt with the same flag. Build from the repository directory to load its Cargo config; if RUSTFLAGS overrides target config, retain `-C link-arg=--stack=8388608`. MSVC targets are unaffected. The full suite passes with the rebuilt CLI. No other Rust code changed.
