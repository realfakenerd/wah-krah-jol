# Phase 1: Asset Modernization Pipeline (`converter`)

> **Goal:** Ingest legacy Bethesda Skyrim SE assets (`.bsa`, `.esm`, `.nif`, `.dds`, `.pex`) and compile them offline into modern, GPU-native standard formats (`SQLite 3`, `glTF 2.0`, `KTX2`, `Luau`).

---

## 📋 Overview & Conversion Matrix

| Legacy Format                | Target Modern Format               | Purpose                                                     | Conversion Library / Crate    |
| :--------------------------- | :--------------------------------- | :---------------------------------------------------------- | :---------------------------- |
| **`.bsa` / `.ba2`**          | Extracted VFS Hierarchy            | Unpacked file system access                                 | `flate2`, `lz4_flex`          |
| **`.esm` / `.esp` / `.esl`** | **`SQLite 3` (`skyrim_world.db`)** | Conflict-resolved record database & R-Tree spatial indexing | `rusqlite`, `libsql`          |
| **`.nif`**                   | **`glTF 2.0` (`.glb`)**            | Geometry, PBR materials, skeletal skinning & joints         | `mesh_tools` / `gltf` crate   |
| **`.dds` (BC1-BC7)**         | **`KTX2` Basis Universal**         | Supercompressed GPU texture streaming                       | `ddsfile`, `basis-universal`  |
| **`.pex` (Papyrus)**         | **`Luau` Scripts**                 | Sandboxed, high-performance scripting                       | Papyrus AST Decompiler ➔ Luau |

---

## 🎯 Phase 1 Modules & Specifications

## Implementation status

The Phase 1 implementation is integrated in `crates/converter` and consumed by the launcher. A run is considered successful only when every discovered supported input is converted, every generated artifact passes structural validation, and `conversion-manifest.json` is published with `complete: true`. A texture reference whose source is absent from the installed game data is not a conversion failure: the converter publishes the mesh without that reference and records the dropped path under the mesh's `pruned_texture_references`. That record is an audit trail for whoever inspects a published asset set: the launcher, the acceptance preflight and the engine accept the set on `complete` plus the converter schema version, and none of them reads the pruned reference list.

The integration report distinguishes conversion-closure failures from references to source assets that are absent from the installed game data. Missing converted artifacts remain fatal when their source exists. Undistributed ESM references and converted animated/effect GLBs without static bounds are reported as coverage metrics, but do not make an otherwise complete asset conversion fail.

- BSA v104/v105 and optional BA2 GNRL extraction use a read-only memory map, path-traversal protection, bounded archive concurrency, deterministic overlay order, and atomic output publication.
- ESM/ESP/ESL records are merged in `plugins.txt` priority order with regular/light FormID remapping, deletion handling, semantic world tables, exterior R-Tree indexing, and an rkyv cell cache.
- Static and skinned NIF geometry is exported to GLB. Collision-only and control-only NIFs are preserved as deterministic empty-scene GLBs, while files that declare unconvertible render geometry remain hard failures. Diffuse, normal, glow, and specular/environment paths are normalized to the generated KTX2 hierarchy and core glTF metallic-roughness materials.
- 2D BC1–BC7 DDS textures are decoded and encoded as runtime-compatible UASTC KTX2. Color, normal and data transfer functions are selected from material/database slot semantics, and authored mip levels and alpha are preserved.
- Skyrim PEX bytecode is parsed, verified, lowered to deterministic Luau state-machine modules, and backed by the sandboxed Papyrus compatibility runtime in `crates/engine`.
- Conversion work is bounded by `cpu_jobs` and `io_jobs`; cache hits require matching source, configuration, output size, and output SHA-256. Archive ingestion is cached separately as deduplicated SHA-256 blobs, so unchanged BSA/BA2 files rebuild the VFS without decompression.

DDS cubemaps are preserved as six-face KTX2 assets, and volume textures preserve their depth slices and original mip chain in 3D KTX2 assets. Ordinary texture arrays are rejected explicitly because they are outside the Phase 1 Skyrim SE texture contract. Unsupported or corrupt inputs leave the manifest incomplete and make the CLI/launcher report failure instead of silently publishing a successful conversion.

Validation commands:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p converter -- <Skyrim Data> <output> --fail-fast --report-json conversion-report.json
```

Use `--invalidate-cache` to force fresh archive extraction and asset conversion. Cache integrity is verified by default; `--no-verify-cache` skips blob hashing when the faster size-only check is acceptable.

### 1.1 BSA / BA2 Archive Extractor

- Extracted virtual filesystem (`vfs`) using async decompression threads.
- Preserves directory layout (`meshes/`, `textures/`, `scripts/`, `sound/`).

### 1.2 Relational World & Spatial Database Transpiler (`ESM_TO_SQLITE`)

- Parse legacy binary records (`CELL`, `REFR`, `LAND`, `NPC_`, `STAT`, `WRLD`).
- Merge plugin overrides in priority order defined by `plugins.txt` to eliminate runtime conflict resolution overhead.
- **Hybrid Spatial Indexing Architecture:**
  - **Exterior R-Tree (`exterior_spatial`):** Indexes exterior world references (`REFR`) with normalized coordinates relative to cell centers (values constrained between $-2048.0$ and $+2048.0$) to guarantee single-precision `float32` accuracy at large worldspace extremes.
  - **Interior Direct Lookup (`cell_id` Hash Index):** Indexes interior references by `cell_id` FormID for instant $O(1)$ object loading upon entering interior doors, avoiding R-Tree spatial collisions.
- **Zero-Copy Cache (`cell_cache.rkyv`):** Memory-mapped (`mmap`) binary heightmaps and vertex normals for instant RAM reads.

### 1.3 NIF Geometry & Material Exporter (`NIF_TO_GLTF`)

- Convert `BSTriShape` and `BSDynamicTriShape` into glTF primitives.
- Map `BSLightingShaderProperty` slots:
  - Diffuse ➔ glTF `baseColorTexture`
  - Normal ➔ glTF `normalTexture`
  - Specular/Environment ➔ glTF `metallicRoughnessTexture`
- Convert `NiSkinInstance` bone weights and transforms directly into glTF `skins` and `joints`.

### 1.4 Texture Compressor (`DDS_TO_KTX2`)

- Transcode DirectDraw Surface files into supercompressed **Basis Universal KTX2**.
- Target runtime transcoding for Desktop (BC7), Mobile (ASTC), and Web (ETC2).

### 1.5 Papyrus Bytecode Transpiler (`PEX_TO_LUA`)

- Decompile `.pex` binary bytecode into an Abstract Syntax Tree (AST).
- Transpile Papyrus Events, Properties, Functions, and State Machines directly into `Luau` modules.

### 1.6 Unified Async Pipeline Orchestration (`tokio`)

- **Async Task Concurrency:** Uses `tokio::spawn` for I/O tasks and `tokio::task::spawn_blocking` for CPU-intensive mesh/texture transcoding (`dds` ➔ `ktx2`, `nif` ➔ `glb`).
- **Async Progress Channel:** Emits real-time progress events over `tokio::sync::mpsc` channels to the Launcher GUI and CLI runners without thread blocking.
- **Deep High-Leverage API:** `AssetPipeline::run_async(config, progress_tx).await` orchestrates all sub-converters concurrently with atomic cache validation.
