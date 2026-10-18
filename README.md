# PS5 Companion

PS5 Companion is a Rust-first PC companion CLI for a planned PS5 transfer workflow. The initial scaffold only identifies the application and its planned workflow; transfers and supporting operations are not implemented.

## Build and run

Install Rust 1.75 or newer, then run:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run
```

The executable currently prints the application name and the planned `download -> extract -> verify -> upload` sequence.

## Planned architecture

- The Rust CLI will own job manifests and orchestration for download, extraction, verification, and upload.
- Extraction will initially delegate to an external 7-Zip executable.
- `PS5Upload` will be a local engine adapter for upload, while `ShadowMount` will own the related mount role.
- SQLite may persist future job state; it is not implemented.
- A GUI can be added later as an optional Tauri shell around the CLI.

Cleanup will be limited to paths owned by a job and will preserve save files. Jobs will preflight available disk space. Repository configuration must not contain personal IP addresses, tokens, game URLs, or catalog assets.
