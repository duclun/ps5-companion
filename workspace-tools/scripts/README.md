# Search and download wrappers

## Multiple storefront catalogs

`ps5-search` and `ps5-download` now combine the local Overdrive catalog with
cached BlackBox and Orbit snapshots. The original Overdrive checkout is never
modified by this adapter. Search/build are offline; refreshing catalogs is explicit:

```powershell
.\ps5-catalog refresh all
.\ps5-catalog refresh blackbox
.\ps5-catalog refresh orbit
.\ps5-search "outer wild"
.\ps5-download PPSA08102 -List
```

Search groups by exact title ID and preserves PS4/PS5 editions separately. It shows
catalog origins. Source options show their storefront, actual artifact ID, region
and explicitly supplied firmware bounds. Unknown firmware stays unknown; a catalog
label is not a compatibility test. Exact semantic source duplicates merge while
retaining every source origin. Distinct formats/versions/IDs remain separate.
Storefront commits, original release IDs, checksums and timestamps remain in cache
and download receipts. Rust source plans retain imported provenance as well.

Metadata refresh resolves official GitHub main to an immutable commit, fetches
bounded metadata without redirects and publishes each storefront independently.
Immutable snapshots, hashed pointers and previous pointers live under
`cache/storefronts`; failed refreshes retain the last usable snapshot. Rejected
records are recorded in the snapshot. Combined snapshots use content-addressed
directories and recheck Overdrive inputs to refuse a concurrent mixed snapshot.
Concurrent refreshes of the same store are refused. No game links are probed during
refresh or search. The base-game adapter excludes Orbit add-ons. PKGs and known
password-protected archives are excluded from the current pipeline's candidates.

BlackBox uses public schema-1 JSON catalogs. Orbit uses its official v8 authenticated
AES-256-GCM envelope. Its released GPL source documents the distributed catalog key
as scraping resistance, not account authentication. Our reader parses the published
header and authenticates the entire envelope; it never executes upstream tools.
No user account secrets are required or stored. A shared-key catalog checksum/tag
does not independently authenticate the game publisher.

For a fresh workspace, install the Orbit refresh dependency in an isolated environment:

```powershell
python.exe -m venv .tools/catalog-python
.tools/catalog-python/Scripts/python.exe -m pip install -r scripts/requirements-catalog.txt
```

Obtain `backend/catalog_key.h` from the Store source archive inside the official
Orbit release source bundle (v1.5.0 was inspected). Verify the source archive against
that bundle's `SOURCE-SHA256SUMS`; copy only the header to
`cache/storefronts/orbit/catalog_key.h`. The inspected header's SHA-256 is
`1eada22bfa7ee24369d68a4ccda7eaa960ceabe1d42efe48d8dad317e72d4e5a`.
The wrapper prefers the catalog virtual environment. A key/envelope generation
change fails closed and requires inspecting the corresponding new official source.
Catalog data, the public header, source archives and virtual environment are runtime
files, not committed. `--orbit-key-header` is also available on the Python refresh command.

Our download/extraction/PS5Upload stages remain in charge. This integration installs
neither storefront app nor its receiver. Source-specific expected SHA-256 values,
when present, are now enforced by the checked downloader. Existing installer/HTML,
range/entity, title, archive traversal, staged upload and no-overwrite guards remain.
These are metadata/adapter tests, not verified downloads from every host.

## Game editions and regions

Search includes primary and alternate catalog editions, each with its own ID,
platform, region (when provided), and work relationship. The game download wrapper
offers PS5/PPSA results only; searchable PS4 entries do not imply PS4 upload support.

Source lists display the actual artifact ID separately from the listing ID.
Automatic fallback stays within the selected artifact ID. Interactive mode asks
which edition to use when IDs differ; unattended callers must explicitly select an
advertised alternate with `ps5-download.ps1 -TitleId <listing> -ArtifactTitleId <artifact>`.
No source sets are merged across editions. Receipts preserve both IDs, while
extraction and upload validation require the selected artifact ID. Missing source
IDs fall back to the listing; malformed or cross-platform identities stop before
network activity. Catalog presence still does not establish availability or compatibility.

## Homebrew catalog CLI

`ps5-homebrew.ps1` is a separate Python-backed entry point using ProsperoStore's
public schema-3 API. It does not mix homebrew identities with the game catalog.

```powershell
.\ps5-homebrew search "player"
.\ps5-homebrew search --category emulators
.\ps5-homebrew info PPSA01153
.\ps5-homebrew download PPSA01153
# Explicitly add validated extraction and staged PS5 upload (never launches):
.\ps5-homebrew download PPSA01153 --through-upload
# Read cached metadata without network requests:
.\ps5-homebrew search --offline
```

Search/info fetch metadata only. Download stops after verifying the artifact by
default. Select the exact catalog app ID: shared-title variants keep their owner
suffix instead of being collapsed into the same entry. Automatic upload of entries
declaring title conflicts is refused, and the existing uploader never overwrites
an installed destination.

The adapter requires available status, matching index/detail identity and version,
a supported ZIP/image format, a positive byte count, an expected SHA-256, and a
pinned GitHub release asset in the declared source repository. Existing downloader
installer/HTML rejection and bounded retries still apply. A checksum supplied by
the catalog establishes integrity against that catalog's assertion; this adapter
does not verify the API's signed manifest or independently authenticate developer
ownership. It never executes downloaded code on the PC.

Cache: `cache/homebrew/`; downloads and receipts:
`downloads/homebrew/<catalog-app-id>/<source-key>/`. Normal operations refresh
metadata and fail on network errors; stale cache use is explicit with `--offline`
and is limited to search/info. Metadata responses are limited to 4 MiB and do not
follow redirects. `--cache`, `--out`, `--addr`, and `--engine` override defaults.
The current upload pipeline supports PPSA title IDs; reserved/coming-soon listings
are searchable but cannot be downloaded. No PKG installation or helper/firmware
update is performed by this command.

## Automatic download recovery and artifact selection

Downloads now retry transient connection failures and temporary HTTP errors up
to three times after the initial attempt. Backoff has jitter and a total wait
budget of five minutes; server Retry-After delays are honored within that budget.
The existing socket read timeout bounds a read with no incoming data. A retry
never changes source URLs, runs browser verification, or retries a PS5 upload.
Attempts and retained byte offsets are recorded beside the download in
`<filename>.retry.json`.

Resume retains exact range/size checks. When available, strong ETag or
Last-Modified validators are saved in `.partial.entity.json` and sent with
If-Range; changed/missing validators stop the transfer. Automatic resume after a
partial failure requires a saved entity validator or an expected SHA-256.
Legacy partials remain usable for explicit manual resume under the original
URL/size sidecar checks, but those checks alone do not authenticate a publisher.
Bad ranges, HTML/installers, digest mismatches and disk failures do not trigger
transport retries. The Python API accepts `max_retries=0` to disable recovery and
a `cancelled` callback for cancellation between reads or during backoff.

The archive pipeline passes `extract --title-id <PPSA-id>` to the Rust CLI.
Extraction and upload planning share a candidate selector: optional sibling
patch/update/backport folders are excluded, title metadata is matched, and
unresolved competing game candidates stop the pipeline. A lone full game inside
a backport-named wrapper remains supported. The manifest pins `selected_artifact`;
all extracted files stay on disk and remain hash-verified, while only the selected
artifact enters the upload plan. Nested game candidates inside a selected game
remain ambiguous and are refused. No files are automatically deleted.

Rebuild both debug and release CLI binaries after source changes: wrappers prefer
the existing release binary. Offline tests use synthetic files and mock PS5
transfers; no new game launch is established by passing them.

The shared downloader rejects Windows executable/installer filenames, redirect
targets, response filenames and executable MIME types. It also inspects the
initial bytes for Windows executable and MSI/OLE signatures before writing file
data, even if the payload is named `.exfat`. This is a targeted installer guard,
not a malware scan or proof that a game download is authentic.

From the workspace root, run `./ps5-search "outer wild"`. Select a match if
needed and answer `y` to download. Search uses the existing catalog CLI;
downloads use `ps5-download.ps1` and `scripts/ps5_download.py`, independently
of the unfinished Rust download command.

The PowerShell wrappers now default to stages 1–5: download, inspect, extract,
validate the title ID and hashed manifest, then perform the existing staged
PS5Upload transfer and remote size verification. PS5Upload must be running on
the PC and console. Defaults: PS5 `192.168.1.214`, local engine
`http://127.0.0.1:19113`; override with `ps5-download <TITLE-ID> -Addr ... -Engine ...`.
Use `-DownloadOnly` to stop after the file transfer. Nothing launches the game.

The pipeline prefers an existing release build of `ps5-companion` for large-file
hashing, falling back to the debug build only when no release binary exists.

Archive handling delegates to the existing CLI and 7-Zip, including its path,
free-space, and extraction checks. Unsupported/encrypted archives stop. Standalone
images skip archive extraction and must pass the CLI's image-header validation;
unrecognized headers stop. Title matching for images uses the image filename and
does not authenticate the contents. Known standalone backports, patches, updates,
and DLC are excluded; detection of a game-shaped artifact does not prove that it
is a complete or playable commercial game. Launch testing remains manual.

Pipeline state and manifests are retained under `downloads/<TITLE-ID>/.pipeline/`.
An upload attempt is never automatically submitted twice when it may already
have reached PS5Upload. Inspect the transfer receipt before resolving a failed
or ambiguous attempt. Existing PS5 destinations are never overwritten.

`./ps5-download PPSA08102` downloads by title ID with all paths defaulted.
`./ps5-download PPSA08102 -List` lists candidate sources without downloading.
Files are saved under `downloads/<TITLE-ID>/<source-set-key>/`, with resumable
partials and SHA-256 receipts. Each fallback source gets its own directory.

Source selection incorporates the relevant Overdrive v1.0.7 changes: unavailable
catalog entries are excluded, direct sources precede browser pages, and flaky
sources rank below peers with the same access method. This is selection policy,
not a claim that any host is currently working. MediaFire catalog sources are
disabled by default after widespread removals. To deliberately retry them, pass
`./ps5-download PPSA08102 -IncludeMediaFire` (or `--include-mediafire` to Python).
MediaFire resolution remains supported for that explicit opt-in.

Every volume in a source set is now resolved and probed before the first volume
downloads. A dead later part or duplicate destination filename stops that set.
The complete set's remaining server-reported download bytes must fit, with a
64 MiB reserve. Only matching partial sidecars count toward retained bytes.
This does not predict decompressed size: the CLI inspects archives and enforces
extraction space separately after download. Existing files are kept on failure;
no automatic cleanup was added.
Existing completed files are reused only after checking their receipt and hash.

Direct HTTPS sources and ordinary MediaFire/VikingFile download-link pages are
supported. A browser challenge or unsupported host is reported; another eligible
source is tried. Catalog entries marked unavailable, DLC-only entries, and
incomplete numbered archive sets are excluded. Sequential multipart downloading
requires a contiguous set starting at part 1; the catalog must include all parts.
The Python entrypoint alone remains download-only unless `--through-upload` is
passed; the PowerShell wrappers enable that pipeline by default.

When automatic sources fail, the PowerShell wrappers offer browser handoff:
choose a source, optionally open its page, complete verification yourself, and
paste the final file URL copied from the browser download. Empty URL skips the
source; `q` at the source menu cancels. The URL is probed before transfer, and
the receipt retains the original catalog source. Cookie-bound downloads may
still need to finish in the browser. HTTP 404 sources are not offered for handoff.
Use `ps5-download <TITLE-ID> -NoBrowserPrompt` for unattended attempts. The Python
entrypoint enables prompts only when explicitly passed `--interactive`.

Offline checks: `python -m unittest discover -s scripts -p "test_*transfer.py"`
and `python -m unittest discover -s scripts -p "test_ps5_download.py"`.

# Bounded Overdrive transfer utility

`pragmata_transfer.py` inspects the public Overdrive v1.0.5 release catalog for
Pragmata (`PPSA02530`) and Ace Combat 8 (`PPSA24913`). Listing is the default
operation. It writes only the small catalog JSON to `downloads/<title>/` when a
catalog is refreshed; it does not extract archives, connect to a PS5, or call a
PS5 upload API.

The source classification mirrors the audited Overdrive resolver in
`audit/overdrive-source-v1.0.4-2211b28c/.../app/state/download-sources.js`.
MediaFire `/file` pages, VikingFile `/f` pages, DataNodes, and other browser
sources are reported as browser-required. CAPTCHAs and provider controls are
never automated. A download is allowed only for an Overdrive record classified
as `direct`, or after the user supplies a final file URL with `--direct-url`.
HTTPS is required for catalog and file URLs, and redirects to HTTP are refused.

From the repository root, refresh and list Pragmata metadata:

```powershell
python scripts/pragmata_transfer.py --title pragmata --refresh
```

Select the known `ffpfsc` record without downloading it:

```powershell
python scripts/pragmata_transfer.py --title pragmata --id rf0249707bcbd
```

Probe a selected provider page. The probe performs a bounded GET of at most 64
KiB, reports HTML pages, and never follows links found in that page:

```powershell
python scripts/pragmata_transfer.py --title pragmata --id rf0249707bcbd --probe
```

If a browser session has already produced a final file URL, pass that temporary
URL explicitly. Keep it out of source control and do not use a provider landing
page in this argument. The size below is the catalog size for the selected
Pragmata record; replace it when selecting a different record.

```powershell
python scripts/pragmata_transfer.py `
  --title pragmata --id rf0249707bcbd `
  --direct-url "https://<final-file-url-from-your-browser>" `
  --expected-size 38503055360 --probe
```

Add `--download` only when the selected final URL has been reviewed:

```powershell
python scripts/pragmata_transfer.py `
  --title pragmata --id rf0249707bcbd `
  --direct-url "https://<final-file-url-from-your-browser>" `
  --expected-size 38503055360 --download `
  --output downloads/pragmata/PPSA02530.ffpfsc
```

Downloads stream to `<output>.partial` and retain a sidecar binding the partial
to its URL, expected size, and optional SHA-256. Resume requires HTTP `206` and
an exact `Content-Range` start; a server that ignores the range is refused.
Existing destination files are never overwritten. A supplied SHA-256 detects
corruption against that supplied value; it does not prove the source is
authentic.

Inspect Ace Combat 8 and refresh its catalog separately:

```powershell
python scripts/pragmata_transfer.py --title ace-combat-8 --refresh
```

The current Ace records are labeled `12.xx` base parts, so the default firmware
gate for the recorded PS5 firmware (`11.20`) refuses a download. An `11.xx`
label is marked `uncertain` because its minor floor is not stated; a label
without any firmware floor is reported as `unknown`. Only an `allowed` or
`unknown` record can proceed to an explicitly reviewed direct URL.

Run the offline guard tests with:

```powershell
python -m unittest scripts/test_pragmata_transfer.py
```


## PKG download and inspection (steps 1–3)

Package support is an explicit **download/local-inspection-only** branch. It never
submits a console transfer or installation. Image/folder defaults are unchanged.

```powershell
.\ps5-search -PackageOnly "title name"
# Or list then explicitly select one source from that immutable catalog snapshot:
.\ps5-download PPSA00001 -PackageOnly -List
.\ps5-download PPSA00001 -PackageOnly -SourceNumber 1
# PS4 CUSA identifiers are accepted only in package mode.
python.exe scripts/ps5_pkg.py downloads/path/game.pkg --title-id PPSA00001
```

Python equivalents: `ps5_download.py TITLE --package-only --list`, then
`--package-only --source-number N` (or `--interactive`). `--package-only` and
`--through-upload` conflict before any network or output creation. PowerShell's
`-PackageOnly` suppresses its normal automatic upload handoff.

Only one uncompressed base PKG/FPKG is eligible. Archived, multipart/split,
password-protected, update and DLC packages remain excluded. Choose the exact
source/version explicitly; no automatic switch between PKG sources occurs.
The list shows platform through the artifact ID, format, version, edition,
region, storefront provenance and known firmware bounds. Missing firmware means
unknown, not compatible. Listing a source does not probe its availability.

Downloads reuse the checked resolver/transport: executable/HTML rejection,
local space checks, bounded retries, entity-safe resume, server byte count,
SHA-256 recording, and expected catalog digest enforcement when supplied.
After completion a bounded read-only parser checks CNT/FIH signatures, supported
FIH version/embedded offset, content-ID/title/platform agreement, base content
type/flags, entry counts/table bounds and entry file ranges. Generic `.ffpkg`
mountable images are not treated as installable packages by extension.

`package-receipt.json` records `package_locally_validated`, the source and digest,
inspection limits and `installation_submitted: false`. Invalid packages remain
on disk and stop the workflow; validation failure does not try another source.
These checks do **not** validate Sony signatures, decrypt/verify all payload
contents, establish a complete game, or prove firmware compatibility/playability.
The advertised server length is enforced; no unverified package-wide size field
is inferred. A locally recorded digest establishes integrity, not authenticity.

Installation API findings and remaining steps: [PKG-INSTALL.md](../../docs/PKG-INSTALL.md)
in the repository snapshot. Console preflight, install submission, durable job
tracking and installed-title verification are still unimplemented.

## Overdrive delta updates and stalled-download recovery

Commands: `ps5-catalog refresh overdrive` refreshes the feed; `refresh all`
independently attempts all three stores. `ps5-catalog build` stays offline.

Overdrive feeds materialize under `cache/storefronts/overdrive`, without changing
the dirty checkout. The immutable feed must match its official release tag/base
and static version declaration. Local full metadata must match that release's
Git blobs; only exact LF/CRLF checkout conversion is tolerated. Game/source/page
schemas, related source-list completeness, bounds, duplicate IDs, safe paths,
removals and staging search are checked before publication. Failed checks retain
the current usable snapshot and prior pointer. Failed staging remains for diagnosis.
Applied feed/base commits appear in source provenance. No JavaScript is evaluated.

Publication requires the existing release CLI. If the feed base changes, a separate
guarded whole-base refresh is required; this operation does not pull the repository.
Offline build verifies both cached output and original base hashes. Changed files
cause a visible stop rather than silently hiding imported metadata. Catalog refresh
does not probe links, download games, install dependencies or contact the PS5.

The shared downloader now uses available-byte reads (`read1` where supported).
A network-only watchdog detects a read without observable progress for 90 seconds,
or a 300-second network window with at least 75% idle waits of 20 seconds or more.
Socket timeout is capped at the smaller configured/socket watchdog limit. Disk,
hash, callbacks and HTTP Retry-After sleep are excluded from the network window.
Thresholds are keyword arguments on `download_direct(stall_timeout=90,
idle_window=300)`, not additional PowerShell flags.

Watchdog interruptions use the existing three-retry budget, same URL, sidecar and
entity/digest guards. Existing written offsets resume only when those checks permit.
No automatic URL refresh, source switch, partial deletion, overwrite or upload
resubmission is added. Previous store pointers survive unchanged refresh runs.
