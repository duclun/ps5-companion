# Sourcing workflow

The working download frontend is included under [workspace-tools](../workspace-tools/README.md).
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

The command prints matching title IDs, titles, platforms, and catalog size values when present. A search does not contact a console and does not download a game.

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

## Offline catalog use

For an offline checkout, point each sourcing command at its local `app/data` directory:

```text
ps5-companion search "Outer Wilds" --catalog-dir path/to/overdrive/app/data
ps5-companion sources PPSA08102 --catalog-dir path/to/overdrive/app/data
ps5-companion plan-source PPSA08102 --release release-01 --out plans/outer-wilds.json --catalog-dir path/to/overdrive/app/data
```

The default source is the upstream `MyLuxy/overdrive` catalog at `main`. Use `--catalog-ref` with a commit or tag to pin the fetched index. `--catalog-repo owner/repo` selects another repository with the same catalog layout. `--catalog-dir` and `--catalog-repo` are alternatives; catalog reference options are useful when fetching from a repository.

Catalog existence is a statement about metadata availability. It does not establish that a working link is present, that a release is compatible with a particular firmware, or that the person using the tool owns the title. Source plans preserve provenance so those facts can be checked before a later workflow acts on them.
