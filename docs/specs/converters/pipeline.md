# Asset & Data Modernization Pipeline Strategy

This document outlines the conversion pipeline to ingest legacy Skyrim formats (`.bsa`, `.dds`, `.nif`, `.hkx`, `.esm`) and output modern asset standards suitable for modern web & native runtimes (Bevy Engine, glTF 2.0, KTX2, WebGPU).

The converter uses the in-house reader by default. `--record-reader legacy`
selects the hand-written reader; `--record-reader inhouse` selects the default
explicitly. Both routes use the same plugin ordering, runtime tables and terrain
cache writer. The in-house route scans memory-mapped records and decodes authored
schema entries before building canonical projection inputs; it does not run the
legacy record decoder for supported records. Unknown record types remain opaque.
Malformed fields and records are omitted with bounded per-plugin diagnostics;
valid neighboring records continue through publication. Semantic terrain and
movement failures are caught per record before a candidate replaces an earlier
usable winner. Legacy decoding recovers at checked record boundaries or the end
of the current bounded group, and never searches corrupt payload bytes for a
replacement header. Malformed optional native links are cleared or their
occurrences omitted; malformed required links omit only their record candidate.
Player lists, MO2 profiles and automatic discovery omit unusable TES4/MAST headers and their dependency closures while usable neighbors continue; a nonempty selection reduced to no usable plugins fails clearly. Selection recovery also omits dependency cycles, remaining dependency inversions and slot overflow with their dependents, as described below. Unusable selection authority remains a conversion error.
Real deletions still apply. LAND with rejected authored heights
or incomplete framing is skipped; genuinely heightless texture/color records
retain the existing default-height cache behavior.

Both readers retain their terminal diagnostics and return bounded per-plugin
summaries by severity. `PipelineReport.warnings` contains data-loss summaries:
malformed records/frames, unusable candidates, cleared optional links, truncated
light IDs, and in-house decoder/adapter issues such as omitted fields or missing
localized banks or referenced malformed entries. `PipelineReport.notices` contains
successful compatibility remaps: master indices past a plugin's list treated as
the plugin's own, and the in-house reader's supplier-self compatibility lines.
Absent localized keys in readable banks also produce notices. Counts and first
examples remain bounded per plugin and kind.

The existing completeness rule requires zero skipped asset inputs and no
warnings. Usable neighboring records still publish, but data-loss warnings mark
both the report and conversion manifest incomplete and make the CLI exit with
code 1. Compatibility notices remain visible without affecting completeness or
the exit code; a mixed run retains both severities and stays incomplete.
`--report-json`, the CLI and the launcher show the warnings and notices. Reader
recovery does not increment the separate skipped-asset counter. Metadata rebuild
and `--reuse-assets` use the same severity and completeness rules when they
re-export records. Official inputs with only compatibility remaps remain complete.

`record-reader.json` stamps the selected frontend. Legacy `projection_version=3`
preserves WRLD RNAM payloads and TNAM strings and restores record-aware VMAD
ownership compatibility. Version 2 added corrected faction links, strict native
link ownership and local recovery to version 1's corrected TX06/TX07 and ARMO
world-model projections. These metadata contracts are independent of asset
producer 27. In-house adapter version 11 adds projection-aware winner admission
to version 10's corrected schema layouts, GMST winner admission, union localization
and filename namespaces. Required runtime projection checks resolve localized
fields before admission, so an unusable later candidate preserves its usable
predecessor without crossing an admitted deletion.
Version 12 keeps zero-FormID GMST settings distinct and prevents a rejected
identity collision from blocking a later usable definition.
In-house identity includes the
authored schema SHA-256, adapter version, string-table language and encodings, and
`inhouse-reader-diagnostics.json` records bounded decoder and adapter issues.
The manifest's `configuration_hash` keeps the full reader identity for metadata
provenance. Database, terrain cache and LOD artifacts are rebuilt on every
conversion, including resumed staging. Resume commands preserve the reader choice.
Converted assets use separate producer, settings and source proofs, described
below; a full reader identity change alone does not discard those candidates or
the archive extraction manifest.

Localized fields in in-house mode use their declared STRINGS, DLSTRINGS or
ILSTRINGS bank, `Strings/<plugin>_<language>.<bank>`, from the effective archive VFS,
with loose files taking precedence. `--language` (`PipelineConfig.language`, default
`english`) selects the language as Skyrim's `sLanguage` does; it must be letters,
digits and underscores, since it is part of a file name. Resume commands keep it. Every validated occurrence remains in canonical records. Null or
missing IDs yield empty canonical text and retain their original ID, bank,
winning plugin and status in `inhouse_localized_fields`; nullable runtime name
columns remain NULL. Missing nonzero keys in readable tables produce counted
notices; missing or unreadable tables and referenced malformed entries produce
warnings. Both readers apply this severity policy; the legacy reader resolves
subrecords whose schema occurrences unambiguously agree on localized type and bank.
A null ID does not require its declared bank. Explicitly resolved
empty text remains distinguishable from unresolved text. A selected union or definition leaf's explicit
bank overrides its parent's bank; an unspecified leaf inherits the parent bank,
then defaults to STRINGS.
Ordinary plugin text and English string banks use Windows-1252, including bytes
that happen to form valid UTF-8. Banks in another language are read as UTF-8, as
Skyrim SE's non-English banks are (Cyrillic, Polish), falling back to Windows-1252
for text that is not valid UTF-8; the reader identity of a non-English pack records
this as `string_table_encoding`, while an English pack's identity is unchanged.
VMAD strings use their independent UTF-8 format.
Text requiring normalization is encoded as UTF-8 after decoding; already exact
ASCII codes retain their original width and termination. Original source bytes
remain available in provenance. The reader identity records both encoding contracts.
Metadata rebuild uses the selected reader while validating retained assets under
their original recorded reader configuration. Older unstamped packs and the exact
`{"mode":"legacy"}` stamp retain the historical unversioned legacy contract;
they are not identified as a versioned projection. Exact version-1 and version-2
legacy stamps and full hashes remain recognized as their original retained-asset
contracts and are not upgraded to the current projection.
Adapter-12 metadata recognizes the exact adapter-10 and adapter-11 retained-asset contracts
with matching original schema/add-on bytes, reader fields and combined settings
hash. The original reader and asset producer remain recorded across repeated
metadata rebuilds. The string-table language changes database text only, so a pack
read in one language is a valid source for a rebuild in another; its proof is
checked in the language it recorded. These explicit compatibility pairs do not authorize arbitrary
older in-house identities or automatically extend to future adapters.
New full configuration hashes
include the versioned legacy identity, while `configuration_hash_for_schema`
preserves historical legacy hash reproduction. Metadata retention validates each
hash against its recorded reader contract; a versioned source cannot omit its
retained asset-reader provenance and infer it from the current metadata reader.
Provenance preserves that reader, producer schema and combined settings hash
across repeated metadata rebuilds, together with any per-kind asset settings
proofs. Retained models/textures keep their verified bytes and may carry older
TXST role encodings. Explicit retention does not upgrade their producer or grant
normal cache eligibility: a later normal conversion checks the original producer
and current settings, source dependencies and resolved texture encoding.
Composed default bodies and generated default gait clips carry separate
`derived_outputs` proofs. Metadata reuse copies only derived files whose supported
recipe, logical input hashes, output size and final SHA-256 are recorded. It
preserves those original proofs across repeated rebuilds, including runtime packs
without raw VFS inputs. It does not regenerate them from unauthenticated current
Data. Historical derived files without proof, unavailable individual derived
outputs (including read, copy or hash failures) and unknown stray runtime files
are omitted with one counted, bounded notice; verified neighboring assets remain
unchanged. Partial derived copies are removed when possible; cleanup failures
are counted in that same notice without aborting the rebuild. These optional omissions
do not make the metadata rebuild incomplete. Use normal conversion to regenerate
derived outputs. Recorded ordinary assets retain their checksum requirements.
Unknown source producer identities are rejected by the retained-asset route.
Plugin filename keys use Unicode lowercase after native Windows-1252 decoding,
including accented case pairs. The same shared function supplies player and MO2 selection, the merged MO2 Data index, master and identity lookup, and archive attachment keys. Record signatures and GMST EditorIDs retain their
separate ASCII conventions. Filename spelling remains available in source proof;
neither Unicode normalization nor general Unicode case folding is implemented.
Complete load-order database exports replace canonical/runtime rows, identity
maps and derived spatial/LOD state transactionally. Public plugin conversion
replaces plugin metadata in that same export transaction. Subset exports retain
unrelated rows and remove an updated reference's obsolete exterior spatial row.
Caller tables and asset conversion proofs survive complete snapshot replacement.
This route copies no archive extraction cache and clears its manifest entries;
the next normal run extracts those archives once before recording new reusable
source/integrity proofs.

The explicit retained-asset route accepts the exact schema-15/16 producer
configuration variant that records fixed Zstd level 6. Protected schema-15
source provenance hashes default quality 192, UASTC level 2 and script ABI 1
plus this fixed compression level to
`b23881a864cdcfe1609779a9a27fefab2de629b50ec339101d4e0ad592e7e435`.
Texture quality and script ABI must still match the recorded configuration;
every retained asset is copied and verified against its output hash. This
compatibility proof does not alter normal conversion or staging cache identities.

For record-only diagnostics, `inhouse-records <Data> <plugins.txt> <new directory>`
writes the database, cell caches, reader identity and diagnostic report without
running asset stages or publishing a runtime pack. The new directory also keeps
selected English archive STRINGS inputs under `.strings-input` for comparison; it does
not take `--language`.

`--typed-signatures TAG1,TAG2` selects the authored typed envelopes to retain;
`--database-work-dir <new directory>` builds SQLite on a separate local disk and
copies the closed database and diagnostics into the output, retaining both copies.
Output and working directories must be fresh and separate from the inputs.

The explicit diagnostic option `--record-archive-only` retains the same typed
selection, canonical `records` and `formid_map`, original source payloads, physical
override history, localized keys/text/status, terrain layers/source order and both
terrain caches. It defers runtime materialized tables, script indexing and the
exterior spatial index. Script bytes and typed VMAD fields remain in the records.
`record-bundle-scope.json` begins with `INCOMPLETE` and declares completion only
after all retained artifacts and any database copy finish. It lists preserved and
deferred tables and stamps `runtime_ready=false`, as does `record-reader.json`.
This option does not produce a runnable pack or claim whole runtime SQL
equivalence. The default diagnostic export and normal converter still build all
runtime views. No diagnostic mode converts meshes, textures, scripts or LOD.

Fixed link-free scalar arrays use compact decoded storage with a shared element
layout. Serialization still emits the existing tagged `Array`, `Struct` and
`Bytes` values in source order, with unchanged numeric values and canonical field
bytes. Layouts containing links, strings, selectors or deferred fields keep the
ordinary representation. Rust consumers can use `Value::array_element` or
`Value::materialized_array` for either representation; exhaustive enum matches
must handle `Value::CompactArray`. The streaming typed writer avoids materializing
the array. Serialized data and runtime archive layouts do not gain a new variant.

Export buffers are released after their last consumers. Accepted raw records,
physical source history and unresolved localized bytes remain available through
their publication steps. Runtime projections clone only the retained subrecords,
avoiding a redundant full-record clone before omitted fields are filtered.

Set `MUDCRAB_RECORDS_PROFILE=1` before starting the process to emit stage timing
observations on stderr. Each observation names the stage, elapsed wall seconds
and record count; these timings need a quiet machine for meaningful comparisons.
This is the single profiling switch. It covers coarse export stages, reads no
clock when disabled, and ignores observation-write failures. Detailed per-record
SQL/binding instrumentation is not part of production export.

In-house terrain publication selects the latest accepted LAND by plugin priority
and physical record order when several identities share a cell. It retains
normalized original texture IDs, slots and blend weights in
`inhouse_terrain_layers` and `cell_cache_preserved.rkyv`, including every active
layer beyond the runtime material's capacity. The runtime `cell_cache.rkyv`
retains its existing one-base-plus-five-overlay limit per quadrant. It removes
overlays with the smallest sum of authored opacity, breaking ties by removing
the highest slot first, and emits one bounded warning per affected cell with
drop counts for all four quadrants. Retained slots and weights use explicit
zero-ID placeholders for absent, deleted or wrong-kind texture targets;
cleared IDs participate in the same ranking as their source assignments.
Native null assignments are resolved before this replacement, so they do not
displace a nonnull assignment or revive discarded weights. Runtime material
construction accepts placeholders in any slot; a nonzero texture with missing
converted images still fails its
existing lookup checks. Original native fields and padding remain in the source
table independently of these derived caches, including superseded slot
assignments, native NULLs and their weights. The preserved cache is auxiliary;
the engine continues to load only the runtime cache. The cache layout and
runtime selection are unchanged; normal conversion and metadata rebuild
regenerate both caches and the layer table.

Explicit player selection is available through `--plugins-file FILE`. Optional `--skyrim-ccc-file FILE` overrides the game-folder Creation list, and `--skyrim-ini-file FILE` supplies General test-file settings without reading ambient profiles. These options also exist as `PipelineConfig.plugins_file`, `skyrim_ccc_file` and `skyrim_ini_file`. Selection rules and recoverable missing-master behavior are documented in [the database specification](db-schema.md). Automatic discovery remains the default.

MO2 profiles use the same official/Creation activation, Windows-1252 decoding, last-duplicate selection and missing-master recovery. `--mo2-instance` and `--mo2-profile` retain the profile's file overlay and `loadorder.txt` order after implicitly active plugins. An active plugin omitted from `loadorder.txt` appends after ordered plugins, in `plugins.txt` order, with a warning. Missing Data entries and missing/disabled-master closures warn and skip while usable neighbors publish. `Skyrim.ccc` comes from the game folder beside physical Data. When it is missing or unreadable, player and MO2 selection activate the installed subset of `ccBGSSSE001-Fish.esm`, `ccQDRSSE001-SurvivalMode.esl`, `ccBGSSSE037-Curios.esl`, `ccBGSSSE025-AdvDSGS.esm` and `_ResourcePack.esl`, in that order after the five official masters. A readable CCC, including an empty file, remains authoritative. MO2 selection and `--plugins-file` are mutually exclusive. In-house localized fields use the effective winning loose English string banks from the MO2 Data overlay before archive banks; malformed winning banks remain unresolved with counted diagnostics, without reverting to a losing bank. Metadata rebuild via `--reuse-assets` requires authenticated physical Data and recorded plugin order; it rejects MO2 profile selection before source/output work. Use normal conversion for an MO2 profile.

All plugin-name keys share Unicode lowercase normalization, including TES4 master lookup, direct MO2 plugin overlays and plugin-to-archive matching. Player, MO2 and automatic discovery omit unusable headers, missing-master closures, dependency cycles and their transitive dependents. Player and MO2 orders also omit a plugin still placed before one of its masters, with its dependents, instead of moving it. After each path establishes its order, it keeps the first 254 full and 4,096 light plugins and omits overflow plus its dependents without backfilling. Bounded notices reach progress and the conversion report. `skipped_plugins` lists every original filename removed by recovery, once each in pre-recovery selection order, including every file in an attempted ambiguous identity; deliberately unchecked plugins are not recovery exclusions. `load_order` lists surviving original filenames in published database slot order. Surviving paths determine slots and all subsequent conversion work.

Player selection and automatic discovery reject all installed files sharing a normalized filename, with one bounded notice per ambiguous identity. MO2 similarly rejects ambiguous direct plugin identities within the winning overlay layer; a higher layer's unique plugin still replaces the lower layer normally. Other asset collisions retain the resolver's existing validation. Header validation and master recovery use only the remaining files, so dependents of an ambiguous name are also omitted while usable neighbors continue.

One shared matcher defines archive ownership, MO2 admission and priority: the normalized archive stem equals a plugin stem or follows it with a space, `-` or `_`, such as ` - Textures`, `-Textures` or `_Textures`. The longest matching installed stem determines ownership. A companion is extracted only when a plugin with that owner stem survives selection; disabled and recovered-away companions cannot override surviving assets. Archives without an installed direct-Data owner retain standalone/base admission outside MO2, including archive-only conversions. Ignored nested plugins do not claim ownership.

MO2 also checks the longest installed owner, using all root plugin identities in the resolved virtual filesystem before recovery. This inventory includes unchecked and recovered-away plugins whose physical source files live in enabled mods or overwrite rather than Data. A shorter surviving prefix cannot activate a more specific rejected owner's companion. Plugins with different extensions may share a stem; any surviving same-stem plugin keeps its companions active. Active plugins' `_` and `-` companions are admitted in MO2 under the shared matcher. MO2 retains its active-companion requirement and rejects unowned archives. Both paths retain plugin load-order sorting and loose-file priority. Each full conversion reconstructs archive inputs and publishes only current assets, so recovery also removes formerly published rejected companions on a warm conversion.

Metadata rebuilds use the source package's checksum-matched plugin order for archive priority. The longest installed direct-Data owner must appear in that order before an archive supplies current LOD settings or terrain diffuse inputs. Verified source assets remain retained.

Asset repair selects replacement sources with the same installed-owner filters after recovering the selected plugins. It keeps recorded database plugin provenance and existing assets outside the repair targets. It retains non-MO2 standalone admission and MO2 unowned-archive exclusion. Archive sorting uses the same shared matcher and longest matching active stem, keeping the first load-order slot for equal stems.

Creation fallback warnings are typed entries in the report's `selection_warnings`, naming the CCC path, read failure and installed built-in fallback. CLI summaries, progress and the launcher show them as warnings. They describe input-selection policy and do not count as failed asset conversions or make healthy published output incomplete. The `warnings` field describes conversion failures and reader data loss; successful compatibility remaps remain `notices`. Empty `skipped_plugins`, `load_order` and `selection_warnings` fields are omitted; old reports deserialize with empty vectors.

---

## 1. Pipeline Overview Diagram

```
┌───────────────────────────────┐
│     Legacy Skyrim Data        │
│  (.bsa, .dds, .nif, .hkx)     │
└───────────────┬───────────────┘
                │
                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                       Mudcrab Converter Pipeline                         │
│                                                                             │
│   ┌────────────────-┐     ┌────────────────┐     ┌──────────────────────┐   │
│   │ Archive Unpacker│ ──► │ Mesh & Texture │ ──► │ Record / World Data  │   │
│   │ (BSA / BA2)     │     │ Converter      │     │ Serializer           │   │
│   └────────────────-┘     └────────────────┘     └──────────────────────┘   │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Modern Asset Output Formats                             │
│  ┌──────────────────────────┬──────────────────────┬─────────────────────┐  │
│  │   glTF 2.0 / glb         │   KTX2 / Basis       │   JSON / BSON / Ron │  │
│  │   (Meshes, PBR, Skeleton)│   (GPU Textures)     │(Cell & Scene Graphs)│  │
│  └──────────────────────────┴──────────────────────┴─────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Format Conversion Matrix

| Legacy Format                 | Target Modern Format                         | Purpose                                                            | Conversion Tool / Library              |
| :---------------------------- | :------------------------------------------- | :----------------------------------------------------------------- | :------------------------------------- |
| **`.bsa` / `.ba2`**           | Directory Hierarchy / VFS                    | Extracted compressed virtual filesystem                            | Rust (`flate2`, `lz4_flex`)            |
| **`.nif`**                    | **`glTF 2.0` (`.glb`)**                      | Standard 3D geometry, mesh hierarchies, PBR materials, skinning    | `nif-parser` ➔ `gltf` crate            |
| **`.dds` (BC1-BC7)**          | **`KTX2` / native formats or UASTC**         | Native preservation, BC7 encoding or UASTC fallback                  | `ddsfile` / `intel_tex_2` / `basis-universal` |
| **`.hkx` (Havok)**            | **glTF Animations & Rapier 3D Colliders**    | Skeletal animation clips & physics collision meshes                | Havok XML exporter / custom parser     |
| **`.esm` / `.esp`**           | **`SQLite3` / Binary (Bincode / rkyv)**      | High-performance spatial indexing, memory-mapped zero-copy queries | `rusqlite` / `rkyv` / `zerocopy`       |
| **`.pex` / `.psc` (Papyrus)** | **`Luau` Scripts**                           | High-performance, sandboxed scripting engine                       | `mlua` / Papyrus-to-Luau AST Transpiler |

---

## 3. Detailed Conversion Modules

### A. Mesh Converter (`.nif` ➔ `.gltf` / `.glb`)

- **Geometry Conversion:**
  - Extract `BSTriShape` vertex buffers (Positions, Normals, UVs, Tangents).
  - Combine multi-part meshes into single glTF primitives.
- **Material Mapping:**
  - Map Skyrim `BSLightingShaderProperty` texture slots:
    - Diffuse Map ➔ glTF `baseColorTexture`
    - Normal Map ➔ glTF `normalTexture`
    - Specular Map / Environment ➔ glTF `metallicRoughnessTexture`
- **Skinning & Rigging:**
  - Map `NiSkinInstance` & `NiSkinData` bone weights directly to glTF `skins` and `joints`.

### B. Texture Transcoder (`.dds` ➔ `.ktx2`)

- Converts DirectDraw Surface files into KTX2 with native payloads, CPU-encoded BC7 or UASTC fallback.
- Derives sRGB, linear-normal, or linear-data encoding from the texture slot recorded by the
  published GLB/world database; filenames and extensions do not determine color space.
- Preserves source alpha and authored mip levels, validates the published KTX2 metadata/hash, and
  aborts the transactional publication when any required texture fails.
- Asset paths use one lowercase canonical key. Archive overlays follow plugin load order, loose
  files have highest priority, and normalized collisions inside one source layer are fatal.
  The shared matcher attaches an archive to its longest matching active plugin stem, using
  exact matches or a following space, dash or underscore and retaining the first slot for
  equal stems. Archives with the same owner use ASCII-lowercase stem order; unattached archives
  follow attached ones. This deterministic tie policy has not been established as equivalent
  to every mod manager's archive insertion order. BSA header file-type categories are advisory;
  unknown category bits do not relax layout, count, offset or compression validation.
- Runtime transcoding applies to UASTC outputs only. Native BC and newly encoded BC7 require compatible BC GPU support. GPU encoder selection does not make a pack portable. The [proposed desktop texture contract](../../adr/0014-phase1-desktop-texture-contract.md) records the pending mobile/web scope decision and validation work in [#274](https://github.com/Mudcrab-Team/mudcrab/issues/274).

### C. World & Scene Database (`.esm` ➔ `SQLite3` + `rkyv` Zero-Copy Cache)

- **SQLite 3 Database (`skyrim_world.db`):**
  - **Spatial R-Tree Indexing (`rtree` module):** Query cell objects instantly using 3D Bounding Boxes (`X, Y, Z` coordinates) based on player camera position.
  - **Relational FormID Lookup:** Fast `O(1)` index table mapping 32-bit `FormID` ➔ Record payload.
  - **Tables:** `cells` (Grid coords, lighting, worldspace), `references` (Placed glTF model URIs, transforms, flags), `npcs` (Stats, race, dialogue trees).

- **Zero-Copy Hot Storage (`rkyv` / `zerocopy`):**
  - Frequently requested terrain meshes and cell data can be serialized into binary files mapped directly into RAM with `mmap`.
  - Eliminates deserialization CPU overhead during fast player movement across Skyrim's terrain.

### D. Scripting Engine (`.pex` Papyrus Bytecode ➔ `Luau` Scripts)

- **Papyrus Decompiler / Transpiler:**
  - Parse `.pex` binary bytecode or decompiled `.psc` source code into an Abstract Syntax Tree (AST).
  - Translate Papyrus features (Events, States, Properties, Native functions) directly to Luau equivalent tables and functions.
- **Luau Runtime Integration (`mlua` crate):**
  - Luau provides extreme execution speed (near C/Rust speed with JIT or optimized bytecode) and native sandboxing.
  - Expose game engine API bindings (e.g. `Game.getPlayer()`, `Actor.addItem()`, `ObjectReference.enable()`) to Luau via `mlua`.
- **State Preservation & Save Games:**
  - Modern Luau state serialization enables light, fast save-game state snapshots without Papyrus VM thread corruption.

---

## 4. Unified Async Pipeline Orchestration (`tokio`)

The converter pipeline is orchestrated asynchronously using **`tokio`** task concurrency and channels (`tokio::sync::mpsc`):

- **Non-blocking Concurrency:** Heavy I/O decompression and file transformations execute concurrently using `tokio::spawn` and `tokio::task::spawn_blocking` for CPU-bound transcode tasks (`dds` ➔ `ktx2` and `nif` ➔ `glb`).
- **Async Progress Reporting:** Sends `ProgressEvent` updates across a `tokio::sync::mpsc::Sender` to the launcher UI or CLI without thread blocking.
- **Unified Interface:** Exposes `AssetPipeline::run_async(config, progress_tx).await` as the single high-leverage entry point for modernizing game assets, and `run_async_with_cancel(config, progress_tx, cancellation)` for a caller that can stop it (the command-line converter's Ctrl+C handler).

### Reuse proofs

Current manifests record `asset_configuration_hashes` for each source kind,
independently of the full reader identity. Each hash contains only the settings
that affect that kind's bytes:

| Kind | Settings proof | Source proof |
| :--- | :--- | :--- |
| NIF -> GLB | Mesh producer contract and current mesh output contracts | NIF and available skeleton bytes, plus scoped material-repair revision and source-texture presence |
| DDS -> KTX2 | Texture producer, fallback quality, UASTC level, Zstd level and CPU/GPU encoder quality | DDS bytes, resolved `TextureEncoding` from the freshly rebuilt database and accepted GLBs, plus the applicable GPU encoding label |
| PEX -> Luau | Script ABI | PEX bytes; known producer eligibility is checked separately |

`derived_outputs` is an optional map keyed by canonical output path, separate
from single-source `entries`. Each proof records a versioned recipe, a map of
canonical logical input paths to winning raw SHA-256 hashes, any recipe-owned
context digest, and the final output's size and SHA-256. `body-compose-v1`
records that sex's heavy body, hands and feet NIFs plus the shared skeleton NIF.
Current producers emit `body-compose-v2`, adding
`recipes/body-compose-v2/context.json`: the SHA-256 of compact JSON containing
the ordered heavy skin choices and thin preparation inputs. Each heavy entry
is `[slot, skin]`; each thin entry is `[slot, winning_raw_sha256, skin]`, in
body/hands/feet order. A skin is null or `[TXST_FormID, eight_NIF_texture_slots]`.
The outer JSON is `[heavy_entries, thin_entries]`. Thin entries are empty when
any thin part is missing, matching the existing preparation gate. These choices
describe main's existing catalog skin selection and thin morph generation;
physical source paths and unrelated database rows are excluded. Historical v1
proofs remain eligible for package-snapshot retention.
`default-gait-v1` consumes the body NIF,
skeleton NIF, skeleton HKX and the sex-specific gait HKX; it builds its bone tree
from the NIF inputs. `creature-idle-v1` records the explicit cow and chicken body
NIF, skeleton NIF, skeleton HKX and `mt_idle.hkx` inputs. `authored-actor-idle-v2`
records one converted body GLB, the project rig HKX, the source clip HKX and
`recipes/authored-actor-idle-v2/tree/<sha256>.json`, the digest of that body's
complete bone tree. Its output path is derived from those inputs without the body
GLB, so bodies with an identical tree share one clip. `actor-idle-catalog-v1`
records `recipes/actor-idle-catalog-v1/context.json` for
`meshes/actor-idles/catalog.json`; the context digest covers every binding's
inputs, unresolved and excluded bodies, clip issues and discovery issues. See
[animal idle investigation](../../research/animal-idle-471.md). Original asset producer/settings provenance remains on the
manifest. Material repair and texture pruning refresh the derived output hash
to describe the published bytes.

Normal conversion continues to regenerate these optional derived outputs and
records a proof only on success; these records do not introduce derived warm
cache hits. Absent or malformed individual proofs establish no eligibility.
Adding this provenance does not change asset bytes or the existing global cache
identities; future recipe byte changes require their own identity change.

Worker counts, GPU batch size and `--no-lod` do not affect these settings proofs.
A reader/schema edit that leaves the actual asset dependencies and encoding
unchanged therefore permits reuse. A role edit that changes one DDS's resolved
encoding changes that DDS's source proof and rebuilds it. Reuse still requires
the published output to exist with its recorded size and SHA-256.
Staging journal entries carry the same per-kind settings proof and also require
the current producer contract and matching source/output bytes; older journal
entries carrying a full configuration hash do not acquire a new proof implicitly.

A converter change to mesh bytes that does not advance the producer appends a
name to `MESH_OUTPUT_CONTRACTS` (`crates/converter/src/cache.rs`). The current
producer's mesh settings proof includes the list, so a normal conversion and a
resumed staging run rebuild meshes an earlier converter of the same producer
wrote, while textures and scripts stay reusable. Every producer-27 manifest
records per-kind proofs, which arrived with producer 26, so the combined fallback
below never decides current-producer meshes. The list is cleared when the
producer advances. `--reuse-assets` still retains the source
package's mesh bytes and its recorded mesh proof, so a later normal conversion
of that package rebuilds them.

Normal producer eligibility remains independent of explicit metadata retention.
The only cross-producer mesh allowance is producer 24 -> 26; a future producer
does not inherit that allowance, so producer 27 (which changes the GLB collision
data) reuses only its own meshes. Older textures regenerate, and scripts require
both a supported producer and matching ABI settings. A retained producer remains
the producer of its original bytes after any number of metadata rebuilds.

Older manifests without per-kind settings hashes must match an exact known
combined configuration hash for their original producer and a known reader
identity. An opaque historical in-house hash whose schema/adapter identity is
unavailable cannot establish asset settings, so its converted assets may rebuild
once. Archive candidates survive this conservative rejection.

Archive extraction reuse is independent of output producer, reader identity and
encoder settings. The archive source SHA-256 must match, and every recorded
content-addressed blob must exist with the recorded size. With cache verification
enabled (the default), each blob must also match its recorded SHA-256. A failed
source or blob integrity check causes that archive to be extracted again;
unaffected archives remain candidates. `--invalidate-cache` explicitly discards
both converted-asset and archive candidates.

### Failed-asset repair

`converter repair-failed <Data> <output> [--apply]` retries only the published manifest's
failed inputs using the original conversion configuration, including reader,
schema add-ons, encoding and MO2 source/profile options. Without `--apply`, the
repair remains in a separate directory for inspection. Applying a repair holds
the output locks, verifies existing outputs, updates the manifest and database,
and publishes transactionally with recovery of the last good output after an
interrupted swap. Missing winning sources remain failures; known authoring resources and geometryless overlay templates are recorded as excluded.
Repaired outputs retain per-kind asset settings and source/integrity proofs.
The manifest records record-data completeness separately from asset failures.
Only normal conversion or a metadata rebuild can establish that proof from a
fresh record read. Asset repair preserves unresolved record loss and missing
historical proof: corrected assets may publish, but the pack remains incomplete
and the repair CLI warns and exits `1`. Run normal conversion to recheck record
data; verified assets remain eligible for reuse. A healthy record source with
only asset failures can become complete after repair. Preview remains separate
from publication and succeeds when its assets and recorded record proof are valid.
Archive extraction candidates stay independent of converted-asset eligibility;
old failed entries cannot override current repaired or excluded decisions.

### Checking a converted output

`converter check <output directory> [--full]` answers "is this converted folder still good?" without
converting anything. It reads `conversion-manifest.json` in the folder and compares every artifact
it lists with what is on disk:

- **Quick** (the default, seconds): each artifact exists and has the recorded size. Only file
  metadata is read.
- **`--full`**: also re-hashes each artifact (SHA-256, in parallel) against the recorded hash.

It also reports a manifest written by another converter schema, a conversion not marked complete,
recorded input failures, and a missing `skyrim_world.db`. It prints `All good` or the problems (up to
20 lines, then a count), and exits `0` when all is good, `1` when problems were found and `2` when the
manifest is missing or unreadable. The check never writes to the folder, and a manifest path that is
absolute or leaves the folder (`..`) is reported instead of read.

A damaged artifact does not need a full reconversion: running the converter again on the same `Data`
folder and output re-hashes each published artifact before reusing it, so only the missing or
changed files are converted again. The library entry point is `converter::check_output`, with a
progress callback for front ends. `converter::check_output_with_cancel` takes the same arguments and
an `&AtomicBool` stop flag: once the flag is set no new artifact is started, the call returns after
the reads already in flight, and the result is an `Err` holding `converter::CheckCancelled` (test it
with `error.is::<CheckCancelled>()`), never a partial report.

---

## 5. Runtime Pack vs Build Workspace

Staging is a build workspace; the published output is a runtime pack. They
are not the same directory.

- Extraction writes originals to `staging/vfs/`. Archive-ingestion blobs
  persist to `<output>.assets-cache/.ingestion-cache/` outside the pack
  (overridable with `PipelineConfig::cache_dir`). Neither ships.
- Conversion writes runtime artifacts (`textures/`, `meshes/`, `scripts/`,
  `skyrim_world.db`, `cell_cache.rkyv`, `integration-report.json`,
  `conversion-manifest.json`) inside staging.
- Publication copies only `report.artifacts` plus the manifest into the
  output directory, then removes non-resumed staging. Hard links are
  preferred with a copy fallback; size audits must deduplicate inodes.
- Resume staging keeps the same layout; retained GLB invalidation still
  excludes `vfs/`.

## 6. Asset Layout & Data Integrity Invariants

1. **VFS Path Normalization (`strip_leading_kind`):**
   - BSA archives and loose mod files use mixed-case conventions (`Textures/`, `Meshes/`, `Scripts/`).
   - The asset pipeline normalizes the leading folder component case-insensitively to ensure flat target mappings (`textures/`, `meshes/`, `scripts/`) without double-nested subfolders (e.g. preventing `textures/Textures/...`).
2. **ESM FormID Remapping Isolation (`is_form_id_subrecord`):**
   - Subrecord remapping during multi-plugin merges is record-type and payload-length aware (`len == 4`).
   - Prevents unintended integer remapping on text strings (`TES4` `CNAM`/`SNAM`), physics parameters (`TREE` `CNAM`), and RGBA color structs (`CLFM`/`AACT`).
3. **Strict Little-Endian ESM Binary Parsing:**
   - All Bethesda ESM multi-byte numeric primitives (integers, floats, FormIDs, and subrecord payloads such as `ACHR` `PDTO`) are parsed as little-endian bytes (`from_le_bytes`).
4. **One File per Extracted Entry (`link_or_copy`):**
   - In the staging workspace, every extracted archive entry is stored twice, as `vfs/<path>` and as the content-addressed blob in `.ingestion-cache/sha256/<xx>/<hash>`, and on a fresh install and on a cache hit alike those two names are a hard link on one file rather than two copies (a cross-volume or linkless filesystem falls back to a copy). Neither ships in the published runtime pack (§5).
   - Every writer into `vfs/` or the cache replaces the path with a new file (renamed over it, or unlink, then copy) instead of writing through it, so a write under one name never changes the other.
   - The exception is a loose-asset override: `overlay_loose_assets` replaces the `vfs` entry with the loose file's own bytes, so that path is no longer a link to the archive entry's blob.
   - Resume and re-conversion writers follow the same replace-not-write rule: pack publication links staging files into the output, so a resumed run must unlink before rewriting any staged artifact it shares with a previous pack.

---

## 7. Command-Line Progress, Interruption and Resuming

`converter <Data> [output]` writes progress to **stderr** and the final summary to stdout, so a
pipeline can keep the outcome while the status goes to the terminal.

`--no-lod` skips generated terrain LOD in normal conversion and `--reuse-assets`.
Full-detail terrain, meshes, textures, scripts and world metadata are still produced.
The resulting database has empty LOD tables and no LOD manifest or payload directory;
an existing package's chunks are not carried forward. Omit the flag on a later run to
build LOD. Toggling it does not change ordinary asset byte identity, so verified current
mesh/texture outputs remain reusable. Resume rebuilds generated database/LOD outputs
before publishing, including when this flag changes.

Conversion owns an exclusive sibling `<output>.lock` for the entire run, including
reads of the previous package. Engine and inspector readers hold shared locks. An
engine using that output prevents conversion from starting, and conversion prevents
new readers from starting. Convert into a separate output directory to keep another
package running. Lock acquisition fails immediately rather than waiting.

Readers open an existing lock file read-only. A read-only installation therefore needs
its owner to provision a readable sibling lock before making the parent read-only.
If that file is missing and cannot be created, startup fails; reading without a lock
would permit replacement of a package while it is in use.

Publication recovery requires the destination's sibling publication record and verified
manifest/generated-file seals. An unowned `<output>.backup-*` directory stops recovery
and remains untouched. Preserve the backup and record for inspection; do not fabricate
a record, delete the backup, or rename it over an existing output. A conversion to a
different new output directory can proceed independently while the old transaction is
investigated. This applies even when the leftover backup predates this recovery protocol.

Every conversion exports remapped `XESP` enable-parent FormIDs, reference `header_flags`,
`enable_parent_id` and `enable_parent_flags`. These database changes also apply to
packages built with `--no-lod`; they are not limited to distant rendering.

### Progress events

`ProgressEvent` (`crates/converter/src/progress.rs`) carries what a status line or a GUI needs:

| Field | Meaning |
| :--- | :--- |
| `stage` | `Discovering`, `Extracting`, `Database`, `Meshes`, `Textures`, `Scripts`, `LodChunks`, `Validating`, `Publishing`, `Complete` |
| `completed`, `total` | Items done and expected for the stage; `fraction()` is `completed / total` |
| `current_file` | The asset or archive in flight |
| `bytes_completed`, `bytes_total` | Bytes done and expected, where the stage knows them cheaply: an archive's file table, or the source sizes a conversion batch sums before it starts |
| `stage_fraction` | The stage's completion when the counts understate it (extraction counts archives but works file by file) |
| `overall` | Whole-run completion in `0.0..=1.0`, from the per-stage weights in `STAGE_WEIGHTS` |

The weights are the measured shares of a fresh Skyrim SE conversion (242,969 assets in 5 h 7 m:
textures about 4 h 30 m of it, extraction 24.2 GB written, meshes about 3 minutes, validation
re-reading every artifact), which is what makes `overall` a time-based bar rather than a
stage-count one. A reconversion that reuses most outputs moves through the early stages faster
than the weights assume, so the bar runs ahead of the wall clock; that is expected.

LOD has no calibrated share of total run time. During that stage the launcher bar
shows completed/total worldspaces and is labeled `LOD <percent>%`; after the stage it
returns to the overall estimate. The underlying overall estimate remains monotonic.
No LOD ETA is shown. A long first world can keep stage completion at zero while work
continues; completed-world counts do not describe work inside that world.

Extraction reports every 512 files (its file table gives both the count and the bytes), each
conversion batch reports every asset with its source size, and validation reports per artifact.

### The status line

On a terminal, the converter keeps one line on screen and redraws it at most four times a second —
on every batch of progress events and on a 250 ms timer, so the elapsed time and the estimate keep
moving while one large asset is being converted — printing a finished line when the stage changes:

```
Textures     61%  [overall  72%]  412 items/s  61.2 MB/s  00:12:31 elapsed  ~00:04:50 left
```

`~… left` is an EWMA estimate over `overall` and appears only once the rate has settled (three
samples and five seconds). The shown `overall` never moves backwards, even when a stage finishes
short of its total. A redraw puts the cursor back at the start of the row and pads the line with
spaces over the one it replaces: no escape sequences, which legacy Windows consoles print
literally.

When stderr is not a terminal (a log file, CI), one plain line per stage is printed every few
seconds, and on every stage change, each stamped with the run's elapsed time. `--verbose` restores
the old behaviour: one line per converted asset.

A warning about one asset (`ProgressEvent::notice`, sent when a dangling texture reference is
pruned) is printed as its own line: on a terminal the renderer ends the open status line first, so
a warning never splices into a redrawn line.

### The summary

A finished run prints the converted, reused and failed counts, the total time, the size of the
converted artifacts, the manifest and `--report-json` paths, and when each stage ran (first event
to last, so overlapping stages are still readable). An incomplete run names the first few skipped
inputs and exits non-zero.

`--report-json` includes `lod_elapsed_ms` and `publication_elapsed_ms` measured with a
monotonic clock. LOD time covers compilation; publication time covers runtime pack
construction, seals, verification and directory publication in normal conversion, or
the new-directory rename in a metadata rebuild. It excludes later ingestion-cache
persistence/pruning. Old reports without these fields or `lod_chunks` deserialize with
zero values; zero in an old report is not a recorded measurement.

### Interruption and failure

The command-line converter installs a Ctrl+C handler (`tokio::signal::ctrl_c`). The first Ctrl+C
sets the pipeline's cancellation flag: the asset in flight finishes, no new asset is started, and
the run stops with the staging folder kept. The flag is checked between archives, between stages,
in front of every asset and once more after the last stage, so an interrupt that lands while the
run is packing up still stops it before the publish rename. A second Ctrl+C exits immediately.

**A failed run keeps its staging folder too.** The folder is everything the run has done, and
either way the converter prints what went wrong, the first few assets that failed, where the folder
is, and the exact command that resumes from it:

```
Conversion failed after 0:12:31 during Textures: failed to convert textures\rock.dds: ...
  assets that failed (first 1):
    - textures\rock.dds
  The staging folder was kept: <output>.staging-<pid>-<stamp>
  Resume where it stopped with:
    converter "<Data>" "<output>" --resume-staging "<output>.staging-<pid>-<stamp>"
  Delete that folder to free the space if you would rather start over: <output>.staging-<pid>-<stamp>
```

`--resume-staging` accepts the folder and re-verifies the work already in it, so a stop costs the
asset in flight rather than the run. The manifest is written once, at the end of a run, so a
stopped run has recorded nothing as converted: the resume re-checks each staged file instead, and
deleting the folder only costs the work the next run has to redo.
