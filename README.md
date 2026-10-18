# PS5 Companion

PS5 Companion is a local Rust CLI for preparing a PS5 transfer job on a PC. The current milestone can inspect an archive and extract it into a staging directory with an auditable manifest. It does not connect to a PS5.

## What works today

The command line interface provides:

```text
ps5-companion inspect <archive> [--seven-zip <path>]
ps5-companion extract <archive> --work-dir <directory> [--seven-zip <path>]
ps5-companion help
ps5-companion version
```

`inspect` validates that the archive can be read with 7-Zip and reports the archive size, estimated extracted size, archive file count, available space on the archive volume, and whether the available space is sufficient. `extract` performs the same preflight, extracts into a new job staging directory under the requested work directory, scans the staged result for recognizable save or game artifacts, and writes a verified manifest. The command prints the manifest path and the detected artifact summary when it succeeds.

The 7-Zip executable is resolved from `PATH`, then from the standard Windows Program Files locations. Use `--seven-zip` when 7-Zip is installed somewhere else or when a specific executable should be used.

Examples (use paths appropriate for your machine):

```text
ps5-companion inspect path/to/archive.7z
ps5-companion inspect path/to/archive.7z --seven-zip path/to/7z.exe
ps5-companion extract path/to/archive.7z --work-dir staging/ps5-job
```

The staging directory is owned by the job. The manifest records the archive, extraction destination, and SHA-256 checked extracted files, plus the detected artifact title when one is identified, so the result can be reviewed before a future transfer step. Extraction does not remove the source archive or any other files automatically. Keep enough free disk space for the estimate plus normal filesystem overhead; a failed preflight stops before extraction starts.

Encrypted archives are rejected in this milestone, and parsed 7-Zip listing metadata is bounded to 32 MiB by default. If extraction fails, the job preserves its owned staging directory and failed manifest for review. The current tests use small synthetic fixtures; no PS5 hardware or production archive has been validated.

## What is planned

Download, PS5 upload, catalog browsing, uninstall, and live PS5 actions are not implemented. The next milestone can add an explicit transfer adapter that consumes a reviewed manifest, with job state and cleanup rules that preserve save files. No network address, token, game URL, or catalog asset is required by this local milestone.

## Build and test

Install Rust 1.75 or newer, then run:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The extraction backend delegates archive operations to an external 7-Zip executable; it must be installed separately for `inspect` and `extract`.
