# Approved catalog and download-recovery improvements - October 26, 2026

The previous local-only PKG selection/inspection changes were committed and pushed
as `a19a5e7` on `feat/local-extraction`. This increment implements the two approved
priorities: version/base-aware Overdrive feed ingestion and bounded stalled-download
recovery. No PKG upload/installation, parallel transport or automatic cleanup added.

## Catalog updates

`ps5-catalog refresh overdrive` resolves the official catalog branch to a pinned
commit, checks its version against the official release tag/static declaration,
and compares full local metadata with the exact release's Git tree. Exact Windows
LF/CRLF conversion is accepted only when normalized bytes match the official blob.
It never pulls, edits or resets the Overdrive checkout.

Materialization validates complete games/twins, source lists and pages, bounds,
source field types, duplicate identities, required related metadata and removals.
The existing Rust CLI validates staged search before immutable cache publication.
Current/previous pointers, full content manifests and source feed/base provenance
are retained. Offline build verifies the original base and cached snapshot. One
store refresh failing does not prevent attempts for the other stores.

**Live limitation:** pinned feed `9cbee660d199411d637f0948be1a88f13adf050a`
(version 1.0.10, base `f6fe484e54b5ae584b7736c00901d618db44de9f`) contains an
embedded CR/LF inside URL and browserUrl for record `rc5a6ab1425db` in
`releases/CUSA02528.json`. The final strict check rejects it. A candidate that
passed the earlier release/base/search checks was withdrawn before completion;
its pointer is preserved at `cache/storefronts/overdrive/rejected-current-2026-10-26.json`.
The original and failed staging/cache snapshots remain for diagnosis. Default
search uses the prior usable merged catalog: 3,251 unique titles, with no exact
duplicate title/source records. No bad record was probed, silently repaired,
quarantined out of the feed or downloaded. Retry refresh after the upstream feed
is corrected; the new implementation is tested with valid synthetic deltas.

## Download recovery

The shared checked downloader adds a bounded network-only watchdog: 90-second
no-observable-progress read limit, plus a rolling 300-second network window with
75% idle intervals of at least 20 seconds. Available-byte body reads avoid hiding
progress inside a full-buffer read. The complete bounded initial screening window
is still accumulated so split executable/HTML signatures cannot bypass rejection.
Socket timeout remains bounded; disk writes, hash work, callbacks and Retry-After
sleep do not count toward network-idle time.

Watchdog failures share the existing retry budget, original URL, durable sidecars,
entity/digest and range checks. A body stall was tested resuming at the actual
written offset with matching ETag/If-Range. No automatic URL/source replacement,
partial deletion, overwrite, upload resubmission or console operation is added.

## Verification

98 Python tests passed, including synthetic full publication and malformed/partial
feed rejection, release/base mismatch, legacy line endings, concurrent edits,
pointer tampering, independent store refreshes, idle/burst recovery, body resume,
split MZ/HTML responses and retry exhaustion. Existing Rust CLI staged search was
executed; Rust source is unchanged and its earlier 64-test result is historical.
PowerShell wrappers and deployment/snapshot hashes were checked. No real game
content was downloaded and no console/helper/firmware state changed.
