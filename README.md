# PS5 Companion

PS5 Companion is a Rust CLI for preparing a local extraction and transferring one reviewed game artifact through the paired PS5Upload HTTP engine. The transfer path is explicit: local verification, PS5 staging, remote size verification, and a no-overwrite publish into `/data/homebrew`.

## What works today

The command line interface provides:

```text
ps5-companion download --catalog <dir> --title-id <PPSAxxxxx> --out <dir>
    [--direct-url <url>] [--list]
ps5-companion inspect <archive> [--seven-zip <path>]
ps5-companion extract <archive> --work-dir <directory> [--seven-zip <path>]
ps5-companion plan-upload <manifest>
ps5-companion upload <manifest> --addr <ps5-ip> [--engine <loopback-url>]
    [--timeout-seconds <1..86400>]
ps5-companion help
ps5-companion version
```

`download` fetches a game archive before the local pipeline starts. Sourcing is
adopted from the Overdrive game store: it reads a user-supplied title catalog
(`--catalog <dir>` holding `<TITLE_ID>.json` release lists in Overdrive's
format), groups "part N" releases into archive sets the way Overdrive's
release-sets do (gapped or ambiguously duplicated sets are marked unavailable),
ranks entries the same way (full game before backport/DLC/patch, preferred
hosts first, direct links before page scrapes before browser captures),
resolves the winner to a direct file URL, and streams it into `--out`. A set
downloads its parts sequentially in volume order, each resolved and
size-checked on its own; parts already on disk at the expected size are
skipped. `--list` prints the ranked release table without downloading.
`--direct-url` skips the catalog for a known file URL. Hosts that need a human
browser capture fail with the capture instruction instead of a dead end. The
catalog stays outside the repository: game URLs must not be committed.

`inspect` validates an archive with 7-Zip and reports its size, estimated extracted size, file count, and available local space. `extract` performs the same preflight, extracts into a new owned job directory, validates the extracted tree, identifies one supported game image or game folder, and writes a complete manifest. It does not remove the source archive or scan for saves.

`plan-upload` reads and re-verifies a complete local manifest without contacting the network or changing files. It prints the artifact byte count, file count, PS5 staging namespace, exact final destination, and each planned destination. It rejects changed files, unsafe paths, ambiguous artifacts, and incomplete manifests before an upload can be submitted.

`upload` is the explicit network command. It requires `--addr <ps5-ip>`, uses the paired PS5Upload engine at `http://127.0.0.1:19113` by default, and accepts an optional loopback HTTP engine URL. The upload core refuses non-loopback engines, credentials, proxy redirects, and existing PS5 destinations. It checks the PS5 engine, free space, and destination occupancy before the POST. Files are sent to `/data/downloads/ps5-companion/<job-id>`, verified there by remote kind and size, then published with `overwrite: false` into the exact `/data/homebrew` target. The command prints the engine job ID as soon as the enqueue response is received and polls until the engine succeeds or fails.

The timeout is bounded to 1 through 86,400 seconds and defaults to one hour. If polling expires, the known engine job ID is reported and the running job is left alone. A submission timeout can leave the job ID unknown: inspect PS5Upload before taking further action. An immutable `upload-attempt.json` reserves each local job before submission and blocks a second submission, including after an ambiguous response. An accepted engine job ID is saved to `upload-submission.json`. Automatic retry and resume are not implemented; the CLI does not cancel the engine job. Completion means the files are ready for ShadowMount after remote size verification. It does not claim that the game launched or that PS5 hardware was validated.

On successful completion, the job directory receives an immutable `transfer-receipt.json` containing the engine job ID, PS5 address, final destinations, sizes, SHA-256 values from the local manifest, and completion state. The receipt is an owned allowlist for a future cleanup feature; cleanup is not implemented yet.

The 7-Zip executable is resolved from `PATH`, then from the standard Windows Program Files locations. Use `--seven-zip` when 7-Zip is installed somewhere else or when a specific executable should be used.

Examples (use paths and addresses appropriate for your machine):

```text
ps5-companion inspect path/to/archive.7z
ps5-companion inspect path/to/archive.7z --seven-zip path/to/7z.exe
ps5-companion extract path/to/archive.7z --work-dir staging/ps5-job
ps5-companion plan-upload staging/ps5-job/.ps5-companion/jobs/<job-id>/manifest.json
ps5-companion upload staging/ps5-job/.ps5-companion/jobs/<job-id>/manifest.json --addr 192.0.2.10
ps5-companion upload path/to/manifest.json --addr 192.0.2.10 --timeout-seconds 7200
```

The staging directory is owned by the job. The manifest records the archive, extraction destination, and SHA-256 checked extracted files, plus the detected artifact title when one is identified, so the result can be reviewed before a future transfer step. Extraction does not remove the source archive or any other files automatically. Keep enough free disk space for the estimate plus normal filesystem overhead; a failed preflight stops before extraction starts.

Encrypted archives are rejected, parsed 7-Zip listing metadata is bounded to 32 MiB by default, and extraction failures preserve the owned staging directory and failed manifest for review. The transfer core does not hash files on the PS5; it verifies remote file kinds and sizes and relies on the locally verified manifest for SHA-256 values. Development and tests use synthetic fixtures and mock loopback engines. No live PS5 transfer or production game archive has been validated.

## Planned work

Catalog browsing, desktop UI, cleanup using transfer receipts, ShadowMount registration, and launch verification are planned. Split-manifest downloads are not implemented.

## Build and test

Install Rust 1.75 or newer, then run:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The extraction backend delegates archive operations to an external 7-Zip executable; it must be installed separately for `inspect` and `extract`.
