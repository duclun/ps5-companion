# Current PC workflow scripts

This directory preserves the working Python/PowerShell frontend alongside its Rust backend so other coding agents can review the complete implementation. The scripts previously lived only at the user's workspace root, outside this Git checkout.

## Deployment layout

These are workspace tools, not new Rust subcommands. Deploy the contents of this directory into an existing workspace with this layout:

```text
<workspace>/
  ps5-search.ps1
  ps5-download.ps1
  ps5-homebrew.ps1
  scripts/                    # this directory's scripts/
  repos/ps5-companion/         # this Git checkout; build target/release
  repos/overdrive/app/data/    # separate Overdrive catalog checkout
  downloads/                  # runtime files, not committed
  cache/homebrew/             # runtime metadata, not committed
```

Review existing workspace files before copying; this snapshot does not install itself. Running the scripts directly from `workspace-tools/` without deploying the layout above will not locate the backend/catalog. Portable packaging and a single Rust-native download command remain separate work.

## Current functionality

- Primary and alternate-edition game catalog search, explicit regional artifact IDs, and download handoff through the PowerShell wrappers.
- Offline merged Overdrive/BlackBox/Orbit search, independent pinned storefront snapshots, source provenance, conservative duplicate handling and explicit catalog refresh through `ps5-catalog.ps1`.
- Provider resolution or final-URL browser handoff; installer/HTML rejection; resumable, bounded download retries with entity/range checks.
- Rust archive inspection, expected-title selection, hashed extraction manifests, and staged PS5Upload transfers with no-overwrite publication and remote-size verification.
- Separate ProsperoStore homebrew search/info/download, expected catalog SHA-256 verification, and explicit `--through-upload` (download-only by default).

See [scripts/README.md](scripts/README.md) for commands, boundaries, and limitations. The homebrew adapter does not verify the catalog's signed manifest; expected-digest checks are against the catalog assertion. Transfer completion does not establish game launch success.

## Verification and provenance

On October 24, 2026, the deployed source passed 75 Python tests and the backend passed 64 Rust tests. Synthetic ZIP integration used real extraction/planning with mocked PS5 submission. Live homebrew search/info were checked, without app downloads or console changes. CI results for this commit are not yet known.

`snapshot-sha256.json` binds these files to the deployed workspace copies at commit preparation. It excludes downloads, caches, logs, credentials, transient resolved download links, and historical one-off recovery scripts. Future edits should update the deployed copy and repository snapshot together until packaging is consolidated.
