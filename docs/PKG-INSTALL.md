# PKG branch: inspected API and steps 1–3

Status: explicit PKG selection, download and bounded local inspection only.
No package install API is called by the implementation.

## PS5Upload source evidence

Inspected official main commit
[53047de401e92039db621c910783d0fa47169855](https://github.com/phantomptr/ps5upload/tree/53047de401e92039db621c910783d0fa47169855).
This describes upstream source, not an assertion about the user's running engine.
The local engine at 127.0.0.1:19113 refused the read-only version request during
inspection, so live API compatibility is not verified. Nothing was started.

- [Routes](https://github.com/phantomptr/ps5upload/blob/53047de401e92039db621c910783d0fa47169855/engine/crates/ps5upload-engine/src/pkg_install.rs):
  `POST /api/pkg/install`; `GET /api/pkg/install/status?job=...`;
  `POST /api/pkg/install/stop`; install sessions/history and preflight endpoints.
- [Request/orchestration](https://github.com/phantomptr/ps5upload/blob/53047de401e92039db621c910783d0fa47169855/engine/crates/ps5upload-engine/src/install/mod.rs):
  request has `ps5_addr`, `source`, `content_id`, optional `title_id`,
  `package_app_ver`, `category`, and `options`. Accepted response is
  `{"ok":true,"job":"..."}`. Busy returns `ok:false,error:busy,job:...`.
  A busy job ID belongs to the existing job; never adopt it as our new submission.
- [Delivery](https://github.com/phantomptr/ps5upload/blob/53047de401e92039db621c910783d0fa47169855/engine/crates/ps5upload-engine/src/install/deliver.rs):
  `source:{"host_file":"absolute PC path"}` streams from engine; a
  `console_path` serves an already uploaded console file over loopback.
  The engine chooses delivery; uploading and installer submission are distinct.
  Do not call `/api/pkg/upload` to transfer to the console: that browser endpoint
  uploads a browser file to the PC engine's own storage.
- [Status](https://github.com/phantomptr/ps5upload/blob/53047de401e92039db621c910783d0fa47169855/engine/crates/ps5upload-engine/src/install/status.rs):
  phases `resolve/deliver/install/verify/done/failed`; verdicts
  `installed/may_not_launch/failed`, plus codes/reasons/metrics.
  A job accepted or all bytes served is not an installed result; even installed
  is not user-confirmed playability. Status retention is bounded and process
  memory is not durable; consult per-console history after restart.
- Preflight: `GET /api/pkg/install/preflight` accepts `addr`, `content_id`,
  optional `package_type`, `expected_size`, `package_fingerprint`; checks existing
  content through the engine. This is not proof that all kernel patches are live.
- Fake packages need active kernel patches and compatible firmware. Later work
  must check helper/engine compatibility and patch capability without automatically
  loading/restarting payloads. Some upstream delivery recovery can recycle the
  installer daemon; assess these side effects before integrating submission.
- Keep destructive reinstall, source deletion, insecure TLS, and plain console
  path fallback disabled. Persist intent and the accepted job ID immediately;
  after an ambiguous response inspect status/sessions/history before any resubmit.

## Implemented boundary

`ps5-search -PackageOnly`, `ps5-download -PackageOnly`, and Python
`--package-only` expose single, uncompressed base packages with explicit source
selection. PS4 CUSA and PS5 PPSA titles remain separate. No install or upload
through this branch. Metadata-only listing works offline from cached catalogs.

The independent read-only parser implements common CNT/FIH field layout checks
using [official parser format evidence](https://github.com/phantomptr/ps5upload/blob/53047de401e92039db621c910783d0fa47169855/engine/crates/ps5upload-pkg/src/lib.rs).
It is stricter than upstream's best-effort display parser, but is not a full
package verifier: no signature verification, payload decryption, or compatibility
promise. FIH version 3, 1–1024 entries and matching full content-ID are required.
Header category rejects updates/DLC; supplemental source labels are also excluded.
Firmware/version remain catalog assertions; the bounded parser does not extract
all PARAM.SFO/param.json metadata in this first iteration.

Archives containing packages remain excluded rather than weakening the existing
Rust folder/image selector. Supporting safe extracted PKG selection needs a
separate design/test increment. Installed-size estimation and console free-space
checks belong to step 4; current space checks cover only local download bytes.

## Remaining steps (not authorized in this increment)

4. Console/helper/patch/space/conflict preflight.
5. Installation submission with durable, ambiguity-safe job bookkeeping.
6. Job tracking plus independent installed identity/version verification.
7. Explicit cleanup after verified installation; launch testing by the user.

Test steps 4–6 first with a small freely distributable homebrew package.
