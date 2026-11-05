# Resume permission fix investigation — 2026-11-05

## Recommendation
Investigate a targeted permission-validation step for owned staged folder uploads. Do not replace our resume mechanism or copy Orbit's mode rules wholesale. No evidence from this source review proves our existing transfers suffer Orbit's reported bug.

## What Orbit fixed
Orbit 1.8.5 fixes access permissions on resumed folder transfers. A client may skip files completed before interruption; fixing only newly received files leaves earlier files with private permissions. Its receiver now checks publication access, checks chmod failures, and validates the owned bundle tree before publication, including skipped and crash-recovered files. Its traversal includes bounds and filesystem identity checks. This is a permission/publication fix, not faster downloads or a change to byte-offset resume.

Evidence: [Orbit 1.8.5](https://github.com/saawant12/orbit-store-ps5/releases/tag/v1.8.5), verified official source bundle retained under reports/overdrive/evidence/2026-11-05; backend/receive.c and its saved diff. The bundle SHA256 was 8b7cfeb7581bc2865057c17c58a661c3f92caf23e59bd0b0d12cb966358bad6e, matching the official checksum file. Review was targeted, not a full audit.

## Our different path
The companion submits explicit files to PS5Upload /api/transfer/file-list, saves the accepted engine job ID, verifies the staged tree, moves it to /data/homebrew, and verifies again. src/upload.rs verify_remote_tree compares paths, kinds, counts and sizes. RemoteEntry does not retain POSIX modes. A complete_size_verified receipt therefore does not establish permissions or launch success.

PS5Upload 10.4.0 source is pinned to b82db4e6d87d678b64ca786c15a7d736d994de99. Its exact AVA1 submodule is 223b6948857fa3263aed919d0074d849ec94681d. The file-list route uses AVA1. AVA1 c/ava1_internal.h lines 13–18 explicitly forces console files and directories to 0777 regardless of Windows-supplied modes; the source comments explain the console loader requirement. Consequently, Windows supplying 0644 does not itself establish an executable-permission bug.

However, c/ava1_apply.c lines 1368, 1426, 1783 and 2956 discard fchmod return values. Normal commits attempt permissions but do not prove success. c/ava1_recv.c resume_check and reconcile check durable data and recovered files without permission verification; completed files are skipped. c/ava1_apply.c finish and finish_landed publish without a whole-tree access check. Thus an incorrect mode left by an earlier failure could survive resume and our size verification. This is a concrete verification gap, not a demonstrated failure on the user's console.

Official pinned source:
- https://github.com/phantomptr/ps5upload/tree/b82db4e6d87d678b64ca786c15a7d736d994de99
- https://github.com/phantomptr/ava1/blob/223b6948857fa3263aed919d0074d849ec94681d/c/ava1_internal.h
- https://github.com/phantomptr/ava1/blob/223b6948857fa3263aed919d0074d849ec94681d/c/ava1_apply.c
- https://github.com/phantomptr/ava1/blob/223b6948857fa3263aed919d0074d849ec94681d/c/ava1_recv.c

Pinned evidence is saved in reports/overdrive/evidence/2026-11-05-resume-investigation. The installed desktop release does not prove the running console helper matches this source; helper version/capability still requires live confirmation.

## Proposed work, if approved
1. Confirm the active helper's version and whether its read-only listing/stat API exposes meaningful modes on the destination filesystem. Missing mode support must be reported as unverified, not passed.
2. For owned loose-folder staging only, validate required file access and directory traversal before publication, including files skipped by resume. Respect PS5Upload's console mode contract and filesystem semantics; do not substitute Orbit's extension-based rules without compatibility evidence.
3. If repair is needed, use a bounded, ownership-scoped helper operation and verify the result. Never recursively chmod existing games or unrelated paths. A helper-side checked fchmod and resumed-tree validation is stronger than client-only checks.
4. Preserve existing accepted-job reconciliation and complete_size_verified semantics. Record access verification separately. Images and PKGs require separate handling and are not automatically covered by loose-folder checks.

Expected benefit: catches a transfer that has correct bytes/sizes but inaccessible game files before publication. Effort: small-to-medium for API capability discovery and reporting; medium for safe repair and integration because helper compatibility, filesystem semantics and race-resistant traversal matter.

Meaningful tests for a later implementation: interrupted transfer with a previously completed restrictive-mode file; injected chmod failure; crash after rename before journal completion; missing mode capability; exFAT fixed modes; symlink/hard-link and escaped-path rejection; correct size but inaccessible file; unchanged normal upload; image/PKG paths unaffected. No such tests were executed in this investigation.

## Scope and limits
Static source inspection only. No games launched, uploads submitted, console permissions changed, helper restarted, firmware updated, builds run, or application code edited. Runtime helper behavior and an actual permission failure have not been reproduced.
