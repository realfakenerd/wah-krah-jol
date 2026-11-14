# Converting game data

The converter turns a Skyrim Special Edition `Data` folder into the asset pack the engine streams:
a SQLite world database (`skyrim_world.db`), glTF 2.0 meshes, KTX2 textures and Luau scripts. The
launcher runs it for you; this guide covers the command line and the options behind the
launcher's settings.

- [Run the converter](#run-the-converter)
- [Convert a Mod Organizer 2 profile](#convert-a-mod-organizer-2-profile)
- [Repair a failed conversion](#repair-a-failed-conversion)
- [GPU texture encoding](#gpu-texture-encoding)
- [Archive extraction](#archive-extraction)
- [Asset compatibility notes](#asset-compatibility-notes)

For the format-level design, see the [converter specs](../specs/converters/) and
[`pipeline.md`](../specs/converters/pipeline.md).

## Run the converter

```bash
cargo run --release -p converter --bin converter -- "<Skyrim Data>" "<output directory>"
```

The output directory defaults to `modern_assets`, which is also where the engine looks by default.
Conversion continues past inaccessible source subfolders and failed loose inputs, recording skipped
inputs in the manifest and leaving the pack incomplete. `--fail-fast` stops at the first such error.
If an inaccessible source subfolder would replace a complete pack with an incomplete one, the run
stops before any conversion work. It names the unreadable paths and leaves the previous pack as it
is. Fix the permissions and run again, or convert to a different output folder.
An unusable previous manifest triggers a rebuild without reusing that pack's assets; successful
publication preserves the original pack in a sibling backup directory named in the run's notices.
Check an existing pack with:

```bash
cargo run --release -p converter --bin converter -- check "<output directory>" --full
```

Run `cargo run -p converter --bin converter -- --help` for every option. Build time, conversion
time and disk space are listed in [`requirements.md`](../specs/meta/requirements.md).

## Convert a Mod Organizer 2 profile

In the launcher, choose **Skyrim Data** to convert the physical Data folder, or **MO2 profile** to
overlay a Mod Organizer 2 profile. Open **MO2 settings**, drop the instance folder containing
`ModOrganizer.ini`, and select a detected profile from the dropdown (MO2's active profile is
selected by default, falling back to the first detected profile). The Skyrim Data folder is still
required as the base. Close settings before dropping the output folder. Source settings apply to
Start and Resume and are selected per launcher session.

On the command line, pass `--mo2-instance "<MO2 instance>"` and, optionally,
`--mo2-profile "<profile>"`.

MO2 sources are read in place; native SKSE plugins are not supported. Instances must declare a
supported Skyrim `gameName`. Configured directories may resolve through junctions, but profile,
enabled-mod, and asset reparse points are rejected.

## Repair a failed conversion

Repair a failed conversion without rebuilding the world:

```bash
cargo run -p converter --bin converter -- repair-failed "<Skyrim Data>" "<output directory>" --mo2-instance "<MO2 instance>" --mo2-profile "<profile>"
```

This reads the original manifest and stages failed inputs, assets from failed archives, and necessary texture and skeleton dependencies in a unique `<output>/.repair-<pid>-<timestamp>` directory. Loose MO2 files are read in place. Repair stages the pack for validation, linking unchanged assets where supported, re-finalizes `skyrim_world.db` to update mesh bounds, and re-runs integration; it does not rebuild the world from plugins. Allow disk space for repaired assets, rewritten metadata, and copies where hard links are unavailable. Omit the MO2 options for physical Data.

The default is a preview: inspect `repair-report.json` and the staged `assets/` folder; the published pack is unchanged except when recovering an earlier interrupted publication. Repeat with `--apply` to validate existing artifact hashes and publish the repairs. Conversion and repair share exclusive output locking and cannot run while the engine holds its runtime read lock. Publication writes a rollback journal before replacing files and updates the manifest last; the next conversion or repair recovers an interrupted repair. Publication is recoverable, not atomic for readers that do not acquire the lock.

The configuration must match the original conversion, and unresolved failures or failed integration prevent publication. Supply the original encoding settings when needed: `--texture-encoder`, `--texture-fallback-quality`, `--texture-uastc-level`, and `--texture-zstd-level` (and the corresponding GPU options). GPU repair uses the same GPU encoding worker as conversion, including configured quality and CPU fallback. Changes to MO2 window settings or disabled mod entries do not invalidate the whole conversion cache; winning asset and plugin changes are tracked separately.

## GPU texture encoding

The converter copies textures whose DDS blocks a GPU can sample directly (DXT1–DXT5, BC1–BC7) into KTX2 unchanged. Ordinary source textures that need encoding use the CPU by default: uncompressed 8-bit RGB(A) (24-bit, X8R8G8B8, A8R8G8B8, A8B8G8R8, DXGI BGRA/BGRX) is block-compressed to native BC7, and the remaining textures are encoded to UASTC (Basis Universal). With `--texture-encoder gpu` a wgpu compute shader encodes every texture it takes, uncompressed RGB(A) included, to UASTC instead, writing KTX2 with the same Zstandard supercompression, so the engine needs no changes:

```bash
cargo run --release -p converter --bin converter -- "<Skyrim Data>" "<output directory>" --texture-encoder gpu
```

Measured by the encoder's author on a full Skyrim SE conversion (Ryzen 5 5600G, RTX 5090, NVMe), from scratch:

| | CPU encoder | GPU encoder |
| --- | --- | --- |
| Whole conversion | 28 min 08 s | 8 min 55 s |
| Texture stage (10,236 encoded, 22,709 copied) | 20 min 37 s | 1 min 29 s |

The CPU timings were measured before the CPU path wrote native BC for uncompressed textures (as of #119); they describe UASTC encoding on the CPU.

On the author's 20 sample textures, quality was about 0.3–0.5 dB PSNR below the CPU encoder's UASTC level 2, both after BC7 transcoding (desktop) and as ASTC (mobile). The comparison tool is not part of this repository, so treat these figures as indicative.

Those samples do not demonstrate mobile/browser pack loading. Native BC still bypasses GPU
encoding, and CPU fallback can emit BC7. See the [desktop texture contract and proposed
portable-output deferral](../adr/0014-phase1-desktop-texture-contract.md), pending approval in
[#274](https://github.com/Mudcrab-Team/mudcrab/issues/274).

- `--gpu-quality N` — endpoint refinement passes, 0–8 (default 2).
- `--gpu-batch-mb N` — source megabytes packed into one GPU dispatch (default 256).

Textures the GPU path cannot take (volume textures, formats it cannot decode) use the CPU encoder, as does the whole run when no hardware GPU is available. If the GPU fails mid-run (a driver error or a lost device), everything it had not finished is encoded on the CPU instead. Encoded textures are cached separately per encoder, so switching it reconverts only them; copied textures and meshes are reused.

Generated terrain LOD atlases use the GPU by default in new CLI runs and
`PipelineConfig::new`. Their encoder is independent of ordinary source textures,
which still default to CPU. To use the GPU for both:

```bash
cargo run --release -p converter --bin converter -- "<Skyrim Data>" "<output directory>" --texture-encoder gpu --cpu-jobs 8 --io-jobs 4
```

Use `--lod-encoder cpu` to select CPU terrain encoding. Existing serialized
configs without `lod_texture_encoder` retain the legacy CPU setting.

`--gpu-quality` and `--gpu-batch-mb` apply to both GPU encoders. Terrain batches
are also bounded by `--cpu-jobs`, and their GPU slots cap the batch budget at
64 MiB. GPU terrain encoding preserves the three authored atlas mips, tile
gutters and sRGB transfer function. Switching encoder recipes or changing GPU
quality rebuilds affected LOD atlases; batch-size changes preserve their cache
proof. A GPU failure falls back to CPU encoding and is recorded in the report.
Fallback chunks have no GPU reuse proof, so a later GPU run retries them.
Explicit CPU selection produces the CPU recipe and its reuse proof instead.

## Archive extraction

Archive extraction uses `--cpu-jobs` for decoding and `--io-jobs` for writers.
By default it selects runtime inputs and English string banks;
`--extract-all-archive-files` keeps every archive entry. The default
`--ingestion-sync archive` seals flushed cache batches before creating derived
files. `per-file` additionally flushes unique derived payloads; `none` skips
fresh extraction flushes and requires verified reuse. Resuming validates those batches and
reconstructs missing or damaged files; archive ordering still determines
which override wins. Containing directories are also flushed on Unix;
portable directory flushing remains a Windows limitation.

## Asset compatibility notes

Compatibility changes also apply to physical Data-folder conversion: BodySlide/Outfit Studio resources under `CalienteTools` are excluded as authoring data, the manifest records `excluded_inputs`, and unsupported NIF versions are rejected explicitly. Particle-size parsing and dynamic-shape recovery restore geometry that was previously omitted.

Dynamic head, hair, and eye meshes now produce geometry. The [PR #154 review](https://github.com/Mudcrab-Team/mudcrab/pull/154#issuecomment-5987129687) reports total GLB size increasing from 209 MB to 451 MB; these are reviewer-provided measurements, not a benchmark reproduced here. The script snapshot delta came from instruction dispatch chunking, not the Mudcrab header rename, which was already in `main`.
