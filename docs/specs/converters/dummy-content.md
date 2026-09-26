# Synthetic Fixtures (`dummy-content`)

`dummy-content` generates deterministic, procedurally built Skyrim-format files so contributors
without a local game installation can develop, test, and demo the converter without touching
copyrighted assets. No game data is read, copied, or required.

Tracks [`issue #2`](https://github.com/realfakenerd/OpenSkyrim/issues/2).

## Quick start

```bash
# Generate a synthetic Data directory
cargo run -p dummy-content -- gen Data

# Convert it with the real pipeline
cargo run -p converter --bin converter -- Data modern_assets
```

The second command produces the same layout the launcher/engine expect from a full conversion:
`modern_assets/` with KTX2 textures, Luau scripts, a GLB mesh, `skyrim_world.db`,
`cell_cache.rkyv`, `vfs/`, the ingestion cache and `conversion-manifest.json`
(`complete: true`). The engine can then inspect the generated world:

```bash
cargo run -p engine --bin world-inspect -- modern_assets 1 0 0 --radius 1
```

## CLI

```text
dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]
                  [--with-interior | --with-lights]
```

- `--seed <n>` — seed for all generated texture content (SplitMix64). The default is stable.
- `--formats dds,pex,nif,bsa,ba2,esm` — restrict output. The default generates everything.
- `--force` — allow writing into a non-empty directory. Existing generated files are replaced
  atomically; unrelated files are left untouched. Generation refuses to follow symlinked path
  components.
- `--with-interior` — replace `Skyrim.esm` with the interior preset (needs `esm`): one exterior
  cell of the generated worldspace, its auto-load door into one interior cell, and the return
  door. The plugin is written through the same atomic, symlink-refusing writer as every other
  generated file.
- `--with-lights` — replace `Skyrim.esm` with the light preset (needs `esm`): one exterior cell
  of the generated worldspace, one `LIGH` base record, and the single reference that places it,
  carrying the light's own `XRDS` radius. Written through the same writer as every other
  generated file.

The two presets each replace `Skyrim.esm`, so they are alternatives: passing both is an error.
The `esm` format has to be included for either.

## Generated tree

| Path | Content |
| :--- | :--- |
| `scripts/generated.pex`, `scripts/second.pex` | Minimal Papyrus `3.2` scripts (magic `0xFA57C0DE`, big-endian header). |
| `textures/generated_color.dds` | BC1 color texture, 64×64, 7 mips. |
| `textures/generated_normal.dds` | BC5 normal texture, 64×64, 7 mips. |
| `textures/generated_color_x8.dds` | Uncompressed `X8R8G8B8` texture, 32×32, 6 mips. |
| `textures/generated_cube.dds` | BC1 cube map, 32×32, 6 mips, six faces. |
| `textures/generated_volume.dds` | BC1 volume texture, 16×16×16, 5 mips. |
| `meshes/generated.nif` | Skyrim SE `20.2.0.7` static quad with a lighting shader and texture set. |
| `Skyrim - Misc.bsa` | SSE `v105` BSA (24-byte folder records) with zlib payloads. |
| `Skyrim - Meshes.bsa` | SSE `v105` BSA containing the generated NIF. |
| `Skyrim - Textures.ba2` | Version 1 `GNRL` BA2 with zlib payloads. |
| `Skyrim.esm` | Worldspace with a 3×3 exterior cell grid, flat LAND terrain, one static and one placement reference per cell. With `--with-interior`, one exterior cell, an interior cell and a reciprocal `DOOR`/`XTEL` pair instead. With `--with-lights`, one exterior cell, one `LIGH` base record and the one `REFR` that places it with an `XRDS` radius override. |

## Library API

The crate can be consumed directly (it is used by converter tests as a dev-dependency):

```rust
use dummy_content::{Entry, bsa};

let archive = bsa::v105(
    &[Entry::new("scripts/hello.pex", b"PEX")],
    bsa::Compression::Zlib,
)?;
# Ok::<(), color_eyre::Report>(())
```

Supported writers:

- `dds`: `X8R8G8B8`, BC1, BC5, BC7; mip chains, cube maps and volume textures with strict
  validation (mip bounds, block alignment, non-zero dimensions).
- `pex`: minimal Skyrim `3.2` script; validates the object name.
- `bsa`: `v105` (default) and `v104`, compression `None`, `Zlib` or `Lz4` (`Lz4` is rejected for
  `v104`). Entries are grouped by folder in first-seen order.
- `ba2`: version 1 `GNRL` (`None`/`Zlib`) and version 1 `DX10` (one chunk per texture).
- `nif`: Skyrim SE `20.2.0.7` static shapes (`BSFadeNode` + `BSTriShape` +
  `BSLightingShaderProperty` + `BSShaderTextureSet`) with validated geometry.
- `esm`: a minimal plugin (`TES4`, `WRLD`, `CELL`, `LAND`, `STAT`, `REFR`, `TXST`, `LTEX`),
  optionally with an interior cell and a reciprocal `DOOR`/`XTEL` load door pair
  (`esm::PRESET_INTERIOR`, `esm::plugin_with_interior`), or with a `LIGH` base record whose
  `DATA` is the 48-byte layout `Skyrim.esm` uses, an `FNAM` fade and one reference carrying an
  `XRDS` radius override (`esm::PRESET_LIGHT`, `esm::plugin_with_lights`). Exports into
  `skyrim_world.db` (schema 4) and `cell_cache.rkyv`.
- `layout`: the `Data/` tree above, with atomic publication and symlink refusal. `layout::generate`
  writes the default tree and `layout::write_plugin` publishes a caller-built `Skyrim.esm` —
  the interior preset included — through the same writer and the same constants
  (`GENERATED_AUTHOR`, `GENERATED_WORLDSPACE`, `GENERATED_MODEL_PATH`, `GENERATED_DIFFUSE_PATH`,
  `GENERATED_NORMAL_PATH`).

Output is byte-for-byte deterministic per seed, which makes fixtures safe to use in golden tests.

## Intentional deviations

Some writers emit layouts that the current converter accepts rather than what a retail game
client would consume; the [ADRs](../../adr/README.md) record the reasoning:

- BSA/BA2 name hashes are zero: the converter resolves entries by table order.
- Cube maps use the legacy `caps2` six-layer layout because the converter rejects spec-standard
  DX10 cube maps.
- `X8R8G8B8` fixtures are 2D only; cube/volume fixtures use block-compressed formats.
- The generated worldspace is intentionally minimal: flat terrain (no `VNML`/`VCLR`/`VTXT`),
  a single static and one reference per cell.
- Door references export without a model. The `--with-interior` preset writes each `DOOR` base
  record with the `MODL` a retail plugin carries, but the exporter fills `statics` from `STAT`,
  `MSTT` and `FURN` only, so a reference that places a door has no model path in
  `skyrim_world.db` (`world-inspect` counts it under `references_without_model`) and the door's
  mesh never reaches the GLB pipeline. Until the exporter handles `DOOR`, the fixture's door pair
  is a link and cell fixture, not a visible one;
  `crates/converter/tests/fixture_interior_pipeline.rs` asserts the count of two so a change to
  either side shows up in a test run.
- `XTEL` destination FormIDs are not load-order remapped: the converter rewrites only the
  subrecords `is_form_id_subrecord` recognises as 4-byte FormIDs, and `XTEL` is not one of them.
  The destination ids are therefore correct only while the fixture is the single plugin at
  load-order index 0, which is how `dummy-content gen` writes it. Nothing consumes `XTEL` yet.
- A light reference's `XRDS` is a single little-endian `f32` rather than a FormID, so the
  load-order remap leaves it alone and a reader gets the value as written. The base record's
  `DATA` puts the radius at bytes 4..8 (`u32`), the colour at 8..11, the flags at 12..16 (`u32`)
  and the falloff exponent at 16..20 (`f32`); `FNAM` is the fade (`f32`).

## Validating with a local game install

Real-asset checks stay opt-in and never run in CI with proprietary data. Point them at an
**unmodded** `Data` directory (for example the Steam install):

```bash
export OPENSKYRIM_SKYRIM_DATA="$HOME/.local/share/Steam/steamapps/common/Skyrim Special Edition/Data"
export OPENSKYRIM_NIF_FIXTURE="/path/to/a/static.nif"
cargo test -p converter -- --ignored
```

Mod-manager "Stock Game" directories are not suitable: their loose files are often modified.

## Development

```bash
cargo test -p dummy-content                                        # unit + integration tests
cargo clippy -p dummy-content --all-targets --all-features -- -D warnings
cargo bench -p dummy-content                                       # criterion benches
cargo test --release -p dummy-content -- --ignored performance     # release budgets
```

Security sweeps (truncation and mutation) for generated archives, DDS files and PEX scripts live
in the converter unit tests; the generated pipeline is covered end to end by
`crates/converter/tests/fixture_round_trip.rs`, and the `--with-interior` tree by
`crates/converter/tests/fixture_interior_pipeline.rs` (which asserts the exported world's
`references_without_model` count) together with the parser-level checks in
`crates/converter/tests/fixture_doors.rs`. The `--with-lights` plugin's `LIGH` `DATA`/`FNAM`
bytes and its reference's `XRDS` are read back by `crates/converter/tests/fixture_lights.rs`.
