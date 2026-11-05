# Folder access verification implementation — 2026-11-05

The initial investigation was committed and pushed as 4e7395d before implementation.

## Behavior
- Folder verification uses PS5Upload's read-only POST /api/ps5/fs/stat contract from pinned 10.4.0 source. No version-string inference or automatic chmod.
- Checks the artifact root and every traversed file/directory, even when the engine reports an already completed/resumed job. Runs before and after no-overwrite publication.
- Requires numeric POSIX st_mode with matching kind/type, read/execute for all classes, and no special bits. Compares stat file size with listing size. Accepts fixed exFAT 0777 and read-only 0555.
- Missing capability, malformed metadata, inaccessible entries and changed kind/size fail without publishing. Staging and accepted engine job are retained; no resubmission or permission repair occurs.
- Post-publication failures explicitly explain that the destination may already exist.
- Receipts separately record access_verification=posix_access_verified for successful folders and not_applicable_image for images. Older receipts deserialize with no access claim. complete_size_verified retains its original meaning.
- Images retain size-only behavior; PKG installation is outside this path.

## Validation
- Full cargo test --locked --offline passed: 69 test functions at that point, including 26 upload tests. Three optional 7-Zip integration functions returned early because 7z was unavailable; no real extraction validation is claimed.
- An additional image regression was then added: targeted upload suite passed all 27 tests. Covers completed/resumed inaccessible files, restrictive directories, missing/invalid/negative modes, special bits, symlink kind/type, size changes, unsupported endpoint, post-publication failure, old receipts, fixed/executable read-only modes, and image isolation.
- cargo fmt --all -- --check and git diff --check passed.
- Strict Clippy found existing unrelated manual_filter in src/sources.rs:614 and field_reassign_with_default in src/main.rs:938. With only those two lints exempted, all-target/all-feature checks passed. No unrelated code changed.
- Release CLI built successfully at target/release/ps5-companion.exe, the existing pipeline's default executable. Used existing workspace Cargo/Rustup caches and installed GNU toolchain, offline. Process-local RUSTFLAGS=-C link-arg=-Wl,--stack,8388608 accommodated the installed GCC linker; repository toolchain configuration was not changed.
- A read-only live capability check could not connect to 127.0.0.1:19113 (connection refused). Active helper capability and real console upload behavior remain untested. No helper was launched/restarted and no console files were changed.

## Limits
Separate list/stat RPCs are not an atomic snapshot and cannot prevent filesystem races or establish ACL-based effective access. No remote hashes or launch success are claimed. Each folder verification adds one metadata RPC per file/directory. Unsupported helpers will retain the completed upload in staging and require inspection rather than bypassing checks. Automatic safe permission repair and helper-side race-resistant publication remain separate work.
