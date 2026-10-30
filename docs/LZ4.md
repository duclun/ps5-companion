# LZ4 content labels and next transfer stage

Implemented October 30, 2026. Catalog adapter 4 preserves contentFormat=LZ4 separately from outer ZIP/RAR/7z and artifact format. Search shows content LZ4 when a title has such an option (not a claim that every option is packed). Source details retain the marker and AMPR requirement. Exact semantic dedup includes contentFormat. Unsupported/contradictory content metadata is rejected. All 49 current Orbit LZ4 source markers survive normalization.

The regular Python download/through-upload workflow excludes these sources before provider access and names the runtime requirement. Even a direct download_group call cannot bypass that check. Ordinary variants remain eligible under existing rules. Rust source plans are metadata-only; retaining a source plan does not authorize or implement download/upload.

## What happens to a downloaded packed game?

The supported path we will verify is:
1. Inspect and safely extract only the outer archive using existing bounds/path/hash checks. Retain the source archive on failure.
2. Identify the actual pack dialect and selected title. For AMPR packs, inspect the game metadata/eboot, pack/index tables, referenced volumes and the supplied fakelib runtime. A catalog LZ4 label alone is insufficient.
3. Either preserve the packed assets with a matching AMPR runtime and compatible ShadowMount fakelib configuration, then stage the complete game folder through PS5Upload; or use a format-aware unpacker to restore loose assets to a separate verified folder and pass that folder through the existing pipeline. Unpacking needs additional disk space and full logical-file verification.
4. Check runtime provenance/version, original game firmware requirements and index/file consistency before enabling a transfer. Never rename a pack as exfat, recursively feed pack assets into generic lz4.exe, or silently replace console runtimes/settings. Runtime/library configuration changes need a separate concrete reviewed task.
5. Registration and user launch testing remain distinct from remote byte/size verification. Test one title before enabling a class of packs; a successful launch does not establish save/load stability.

No LZ4 game, runtime or converter was downloaded/installed by this label change. No console changed. No end-to-end packed-game support is claimed yet. Until the format/runtime gate is implemented and validated, select the ordinary folder/image alternative; the tool will not fill the disk with automatic LZ4 downloads.

## Source evidence and caveats

- Orbit pinned docs distinguish outer compression from packed game content: https://github.com/saawant12/orbit-store-ps5/blob/9a2c108eee1cd9934922e9a2c7a426c1a5b70c1d/guides/install-games.md#lz4-game-files
- Dump Forge documents AMPR LZ4 asset volumes, index and runtime, and format-aware unpacking: https://github.com/quer3q/ps5-dump-forge/blob/main/docs/LZ4.md
- ShadowMountPlus documents game/global fakelib precedence and emulator update replacement: https://github.com/drakmor/ShadowMountPlus#readme

These are upstream technical claims, not tests of this user's firmware 11.20 or installed payload version. Some upstream AMPR test builds document save crashes. Index/pack compatibility and preservation of the correct game runtime must be checked per artifact; simply updating to a newer runtime is not proof.

Verification: 101 Python tests and 65 Rust tests passed, including same-URL variants with different content markers, legacy metadata, invalid packaging, metadata-only plans and no-network default exclusion. Build uses the existing cached GNU Rust toolchain/bundled ld.lld; the system MSVC linker is unavailable.
