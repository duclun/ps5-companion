# Sourcing workflow

The working download frontend is included under [workspace-tools](../workspace-tools/README.md).
Its deployed wrappers now build an offline combined index from Overdrive and cached
BlackBox/Orbit catalogs. Use `ps5-catalog refresh all` to refresh the latter two;
see the storefront setup and provenance details in `workspace-tools/scripts/README.md`.
Direct Rust search still reads the catalog directory explicitly selected by the caller;
it does not fetch BlackBox/Orbit automatically. Imported source plans preserve
storefront provenance and supplied firmware bounds, which are catalog claims rather
than compatibility tests.
Its PowerShell wrappers search the local catalog and run the Python download,
inspection, extraction, validation, and explicit PS5Upload pipeline. Homebrew has
a separate `ps5-homebrew` entry point with catalog-provided checksum verification.
Those scripts require the documented workspace layout. The Rust commands below
remain metadata/source-plan commands; they do not themselves download artifacts.

The sourcing commands make the Overdrive catalog available to an agent without requiring a separate checkout or external conversation history. They read catalog metadata, show the source records attached to a title, and save a reviewed source selection as an immutable JSON plan.

## Look up a title

Search accepts one query argument. Quote a multi-word title so the parser receives it as one query:

```text
ps5-companion search "Outer Wilds"
```

The command searches both primary `games` and alternate `twins` without executing JavaScript. It prints matching title IDs, titles, platforms, region, alternate-edition status, work relationship and catalog size when present. Editions remain separate choices with their own release lists. Duplicate IDs across arrays and contradictory platforms are rejected. A search does not contact a console and does not download a game. The PowerShell game download wrapper still supports PS5/PPSA only; PS4 results are searchable but do not gain PS4 installation support.

## Inspect sources

Use the title ID from the search result:

```text
ps5-companion sources PPSA08102
```

Each source line includes its selectable release-source ID, provider, version, format, size, and access label. The output also shows source, browser, or manifest URLs when the catalog provides them. A browser-required or unavailable label is metadata for the next workflow step; the command does not automate a browser, solve a CAPTCHA, or claim that a link works.

## Save a source plan

Choose a release-source ID from `sources` and write a plan to a new path:

```text
ps5-companion plan-source PPSA08102 --release release-01 --out plans/outer-wilds.json
```

The command validates the title and release against the catalog, records the selected source and catalog provenance, and creates the output with immutable create-new semantics. If the destination already exists, choose another path. The plan is an input for a future resolver and transport workflow; it does not download files.

Source plan schema 2 keeps `title_id` as the catalog listing ID and adds `artifact_title_id` for the actual release edition. Each release also exposes `title_id`, region and platform. A missing release title ID falls back to its listing ID; an invalid ID or cross-platform mapping is rejected. Selecting a release ID explicitly can select another region's artifact without changing which catalog file supplied it.

The Python frontend restricts automatic source fallback to one artifact ID. It displays different IDs before network activity and, in interactive mode, offers an explicit edition choice. For unattended use, select an advertised alternate with `ps5-download.ps1 -TitleId PPSA00001 -ArtifactTitleId PPSA00002` (illustrative IDs). This does not search a different listing or combine its sources. Download receipts and pipeline state retain both IDs; extraction, local metadata checks and upload planning require the chosen artifact ID. Old receipts without the new identity fields retain their existing local artifact checks. File hashes, image headers and remote no-overwrite checks remain required.

## Offline catalog use

For an offline checkout, point each sourcing command at its local `app/data` directory:

```text
ps5-companion search "Outer Wilds" --catalog-dir path/to/overdrive/app/data
ps5-companion sources PPSA08102 --catalog-dir path/to/overdrive/app/data
ps5-companion plan-source PPSA08102 --release release-01 --out plans/outer-wilds.json --catalog-dir path/to/overdrive/app/data
```

The default source is the upstream `MyLuxy/overdrive` catalog at `main`. Use `--catalog-ref` with a commit or tag to pin the fetched index. `--catalog-repo owner/repo` selects another repository with the same catalog layout. `--catalog-dir` and `--catalog-repo` are alternatives; catalog reference options are useful when fetching from a repository.

Catalog existence is a statement about metadata availability. It does not establish that a working link is present, that a release is compatible with a particular firmware, or that the person using the tool owns the title. Source plans preserve provenance so those facts can be checked before a later workflow acts on them.

Package sources are opt-in through the deployed `ps5-search -PackageOnly` / `ps5-download -PackageOnly` branch. It downloads and locally inspects one explicitly selected base package; it never uploads or installs. See [PKG-INSTALL.md](PKG-INSTALL.md) for scope, source evidence and remaining steps.

Overdrive feed updates are available through `ps5-catalog refresh overdrive` (or `refresh all`). They are published into cache only after release/base, full metadata, and existing CLI search checks pass. The checkout remains unchanged. Offline search pins a verified delta snapshot when present; later base/cache mutation stops visibly.
