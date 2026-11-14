# Mudcrab SQLite 3 Database Schema (`skyrim_world.db`)

This specification details the canonical DDL schema, tables, indices, and column constraints for `skyrim_world.db`, as implemented in [`crates/converter/src/esm/exporter.rs`](../../../crates/converter/src/esm/exporter.rs).

The in-house reader is the default; `--record-reader legacy` selects the
hand-written frontend. Both record frontends use the same runtime column contract and TXST slot
interpretation: diffuse TX00, normal TX01, mask TX02, glow
TX03, height TX04, environment TX05, detail TX06, and specular TX07. The existing
`detail_path` column carries TX06 multilayer data; TX03 is the source's
glow/detail-map slot and TX07 its backlight-mask/specular slot. These role names
retain the runtime approximation and do not imply complete shader support.
ARMO model projection uses MOD2 with MOD4 as the female world-model fallback
for both frontends. MOD3 is an ARMA model slot, not an ARMO world-model path.

---

## 1. Schema Overview

`skyrim_world.db` is built by `crates/converter` by parsing master files (`Skyrim.esm`) and plugin files (`.esp`/`.esl`). With `PipelineConfig.plugins_file` (CLI `--plugins-file`), the converter reads Windows-1252 selection, keeps the last duplicate entry's position and checkbox, and matches installed filenames case-insensitively. Installed Skyrim, Update, Dawnguard, HearthFires and Dragonborn masters are always active in that fixed order, followed by installed entries of readable `Skyrim.ccc`. `skyrim_ccc_file` (`--skyrim-ccc-file`) overrides the default file beside Data. Only checked player entries load, including mod masters; official and Creation entries keep their implicit positions. An explicit `skyrim_ini_file` (`--skyrim-ini-file`) supplies General `sTestFile1` through `sTestFile10`: listed plugins retain their listed positions even if unchecked, while unlisted entries append by ascending file modification time (filename breaks ties). No ambient user profile is read. Missing entries and optional CCC/INI read errors warn and continue. Plugins with missing, disabled or unusable masters and their transitive dependents are omitted with bounded notices; an empty usable selection gives a clear error.

MO2 profiles reuse the shared selection rules and the converter's header/missing-master closure recovery over winning overlay paths. Installed official masters and game-folder CCC entries remain implicitly active; other selected plugins follow `loadorder.txt`. Missing ordered entries append in last-occurrence `plugins.txt` order, with a warning. Both profile lists use Windows-1252 and retain only the last duplicate. Missing files and unusable dependency closures skip with profile-context notices, preserving usable neighbors. Classic Skyrim still activates bare profile entries. Plugin-name keys use the shared Unicode `normalize_plugin_name` function (`to_lowercase`) across player lists, MO2 lists, merged Data lookup, master matching and identity maps. Windows-1252 `CAFÉ.esp` and `café.esp` therefore match; Unicode normalization and full Unicode case folding are not implemented. Record signatures, EditorIDs and other asset keys retain their existing rules. MO2 preserves its existing final order validation rather than applying player-list priority normalization.

The existing explicit-order normalization gives ESM-flagged plugins and `.esm`/`.esl` files priority, preserving listed order within each category except that regular dependencies are moved ahead of master files that need them. Cyclic plugins and all their dependents are omitted before normalization. The established final order admits at most 254 full and 4,096 light plugins; overflow and its transitive dependents are omitted without backfilling later entries or changing survivor order. One bounded warning summarizes each recovery cause; the conversion report's `skipped_plugins` retains every original filename removed by selection recovery, once each in pre-recovery selection order, including attempted ambiguous filenames, unusable headers and missing-master closures. Unchecked plugins are not skips. `load_order` records surviving original filenames in the same order as the published `plugins` table. Both vectors default to empty when absent from older reports. A plugin the normalized order still places before one of its masters is omitted with its dependents rather than moved. Surviving paths feed slot assignment, archive priority, database merging and terrain caches. An ESL-flagged `.esp` takes a light slot but stays among regular plugins.

Player and MO2 selection use a shared fallback when `Skyrim.ccc` is missing or unreadable: the installed subset of `ccBGSSSE001-Fish.esm`, `ccQDRSSE001-SurvivalMode.esl`, `ccBGSSSE037-Curios.esl`, `ccBGSSSE025-AdvDSGS.esm` and `_ResourcePack.esl`, in that order after the five official masters. A readable CCC, even empty, stays authoritative. `selection_warnings` records the fallback's CCC path and read failure separately from conversion failures; CLI/progress/launcher display it as a warning without making usable output incomplete. The optional vector defaults to empty for older reports.

Archive ownership and load-order priority share the same exact-or-space/dash/underscore stem matcher. The longest installed owner must survive recovery before a companion can contribute inputs, including repair and metadata rebuilds. Active `_` and `-` companions are admitted in MO2; unowned archives remain excluded there and admitted outside MO2. Equal active stems keep their first slot. Direct MO2 plugin-name ambiguity in the winning layer is recoverable; higher-layer unique replacements retain normal overlay behavior.

Without `plugins_file` or an MO2 profile, only plugins directly in Data are selected and dependencies precede dependents. Among ready plugins, ESM-flagged plugins and `.esm`/`.esl` files take priority, followed by the five official files' conventional order and Unicode-normalized filename order. Unusable headers and missing-master closures are omitted with bounded notices, as in player selection, and this path shares cycle and slot-overflow recovery. A nonempty plugin selection reduced to no usable plugins fails clearly, while asset-only inputs retain their existing behavior. Automatic discovery cannot infer override priorities between unrelated mods; nested backup/optional plugins are ignored while nested assets remain discoverable.

The database stamps its version in `schema_info`; the current version is **9**
(`shared::WORLD_DATABASE_SCHEMA_VERSION`). Schema 4 added lights and
`references.radius_override`. Schema 5 adds grass data in #152;
the combined producer exports grass and LOD tables. Schema 6 adds LOD origins, chunk metadata,
its spatial index, and a build identity. Schema 9 combines the corrected texture-set slot
projection: `mask_path` comes from TX02, `glow_path` from TX03, `height_path`
from TX04 and `environment_path` from TX05. TX00 and TX01 provide diffuse and
normal paths. Both frontends project TX06 into `detail_path` and TX07 into
`specular_path`, as documented above. Schema 9 also
projects only XESP byte 4 into `enable_parent_flags`. Separate candidate fixes
used producer 25/world 8 for different contracts; the combined output first shipped
as 26/9, and producer 27 keeps world schema 9.
The engine, `world-inspect` and launcher accept world schemas **3 through 9**, using
`shared::supports_runtime_world_database_schema`. Complete converter packages
support schemas **15 through 28**. Legacy worlds render full detail without
LOD; an advertised LOD package requires the current database contract.

Converter producer 26 also corrects the texture encodings chosen from those
columns. Producer 27 adds sphere, multi-sphere and cylinder collision shapes
and more physical layers to the GLB collision data; the database stays at
schema 9. Producer 28 changes the KTX2 mip layout and alignment; the database
still uses schema 9. Producer-27 KTX2 files need regeneration, while
producer-27 GLBs remain reusable because their bytes are unchanged. At producer
28, GLB producers earlier than 27 are incompatible. Normal conversion rebuilds the
database, cell cache and derived LOD outputs, and regenerates assets whose
producer contract or source/configuration/output proof is incompatible.
Unchanged scripts and archive ingestion remain reusable when their own source,
dependency, configuration and output-byte proofs match.
Metadata-only rebuilds preserve the original retained producer/configuration
and mesh and texture bytes. Run normal conversion to repair those retained
encodings and collision data.

The legacy TX06/TX07 alignment changes the SQL role assignment while retaining
the current texture-byte encoding: multilayer/detail and specular/backlight are
both color uses in `TextureEncoding::from_semantics`. Exchanging those roles
leaves the selected encoding unchanged, including normal/data precedence for
textures with several uses. ARMO model-path alignment also changes the SQL
projection; mesh conversion discovers NIF inputs independently of that column.
These projection changes require rebuilding metadata. Any subsequent change to
converted mesh or texture bytes must invalidate the affected asset cache key;
metadata retention preserves the producer identity of the retained bytes.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      `skyrim_world.db` Implemented Schema                   │
│  ┌──────────────────────────┬──────────────────────┬─────────────────────┐  │
│  │   `plugins`              │   `records`          │ `worldspaces`       │  │
│  │   (Active Plugin Order)  │   (Raw FormID Data)  │ (Worldspace EDIDs)  │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `cells`                │   `references`       │ `exterior_spatial`  │  │
│  │   (Cell Grid & Names)    │   (3D World Placements)│ (3D Spatial R-Tree) │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `land`                 │   `lod`              │ `scripts`           │  │
│  │   (Terrain Heightmaps)   │  (Unused LOD Table)  │ (Papyrus Bytecode)  │  │
│  ├──────────────────────────┴──────────────────────┴─────────────────────┤  │
│  │   `formid_map` & `conversion_cache`                                   │  │
│  │   (32-bit to 64-bit ID Bridge & Cache Hashes)                        │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Table Definitions

### 1. Active Plugin Registry (`plugins`)

Stores loaded `.esm`/`.esp`/`.esl` plugin file metadata, load order priority, and checksums.

```sql
CREATE TABLE IF NOT EXISTS plugins (
    id INTEGER PRIMARY KEY,             -- load-order index; records.load_order refers to it
    name TEXT NOT NULL UNIQUE,          -- plugin filename as installed
    priority INTEGER NOT NULL,          -- same as id
    checksum BLOB NOT NULL              -- SHA-256 of the plugin file
);
```

---

### 2. Primary Record Database (`records`)

Stores subrecord byte payloads indexed by 32-bit Skyrim `FormID` and 4-character
record type codes, encoded as rkyv `ArchivedRecordData`. Both frontends resolve
known embedded links to load-order numbering. The in-house frontend projects
validated decoded fields into this existing encoding. Its auxiliary experimental
`inhouse_source_records(form_id, source_form_id, load_order, record_type, flags,
payload)` table holds original
decompressed source bytes, including file-relative IDs and unused framing. Those
source bytes are provenance, not decoded runtime fields. `load_order` names the
winning source plugin through `plugins.id`. Required movement and terrain
semantics are validated before winner selection; rejected candidates leave an
earlier usable winner intact and are counted in per-plugin diagnostics. The pack's
`record-reader.json` identifies the frontend and schema.

```sql
CREATE TABLE IF NOT EXISTS records (
    form_id INTEGER PRIMARY KEY,        -- winning record, load-order numbering
    record_type TEXT NOT NULL,          -- 'CELL', 'REFR', 'NPC_', 'WEAP', 'ARMO', 'SPEL', etc.
    cell_id INTEGER,                    -- parent CELL of a placed reference or LAND; NULL otherwise
    worldspace_id INTEGER,              -- parent WRLD of an exterior CELL or reference; NULL otherwise
    load_order INTEGER NOT NULL,        -- plugins.id of the plugin that supplied the winning record
    data BLOB NOT NULL                  -- rkyv ArchivedRecordData: the subrecord payloads
);

CREATE INDEX IF NOT EXISTS idx_records_type ON records(record_type);
CREATE INDEX IF NOT EXISTS idx_records_cell_id ON records(cell_id) WHERE cell_id IS NOT NULL;
```

---

### Draft: optional in-house override history (`record_overrides`)

The prototype in-house producer adds a tooling table containing each physically
framed source record, including singletons, deleted versions and rejected
candidates. Runtime readers continue to query the existing winning-record
tables. No losing payload or decoded field set is retained for history during
conversion. Singletons are explicit: this makes complete physical-header audits
and lazy version lookup uniform without a second implicit representation.

`record_overrides` has a physical primary key `(plugin_id, source_record_offset)`
and a `(form_id, chain_position)` lookup index. Columns are canonical `form_id`,
zero-based `chain_position`, `plugin_id` naming `plugins.id`, native
`source_form_id`, `deleted`, `record_type`, `flags`, `source_record_offset`,
`source_record_size`, `status`, `reason`, native enclosing cell/world/topic IDs
and topic-group offset. The preservation extension also stores proven
`original_owner`, unmasked `full_local`, `source_identity_state`, exclusive
`source_group_end`, `frame_complete`, and exact four-byte `record_signature`
BLOB. A source lookup index covers `(original_owner,full_local,plugin_id,
source_record_offset)`. The record signature does not create a separate source
identity namespace; changing kinds at one owner/local remains a type conflict.
`source_record_offset` points at the 24-byte native
header; size is the declared on-disk payload length, including the compressed
size word when present and excluding the header. Unmappable identities are NULL
and remain identifiable by plugin, offset and native FormID. Positions follow
load order, then physical source offset; duplicate physical IDs stay distinct.

`accepted` versions participate in the existing winner/deletion policy;
`skipped` versions retain a reason and do not displace usable predecessors.
The last accepted version is the source winner unless it deletes the record.
The existing torn-override rule skips an incomplete payload that would replace
an earlier complete version; its physical history row retains that reason.
GMST history uses the reader's canonical EditorID aliases, including an already
decoded EditorID when a later mandatory field rejects that version. Broken
EditorID framing retains the raw numeric alias; history does not guess an ID.
Each TES4 row uses a NULL canonical identity
and status `header`, and stores source path and the authored schema hash.
`inhouse_plugin_masters(plugin_id,master_index,master_name)` stores original MAST
order as scalar rows with contiguous zero-based indices; a header with no child
rows has no masters. New snapshots have no `plugin_masters` JSON column.
The history loader still accepts that column in retained packs when the child
table is absent. These small metadata rows allow replay to reconstruct
original full/light mapping without reading another missing or changed plugin.
They are excluded from ordinary record-chain counts. Their deterministic positions
in the NULL-identity group are header ordering, not a semantic record chain.

`inhouse-records history <pack> <hex FormID> [--data DIR] [--json]` opens the
database read-only and verifies each source against `plugins.checksum` before
decoding the requested offset with the producing schema. Changed, missing,
oversized or undecodable versions remain local diagnostics; other versions
continue. The text view aligns authored field names and occurrences and marks
differences. This is decoded source provenance, not a merge or patch format;
partial decoding and unsupported semantic roles retain their existing limits.
Retained 18-column history layouts remain queryable. If any of the three
physical replay safety columns (group end, frame completeness or exact record
signature) is absent or NULL, that version is reported as undecodable with its
stored provenance; replay never invents missing bounds or reads unchecked bytes.
Older ASCII-folded owner spellings are accepted only through an alias derived
from the authenticated stored plugin basename and its original master list.

These optional tooling tables do not change the runtime database
contract, accepted version range or asset encodings. World and converter asset
schema constants therefore remain unchanged. The existing committed history
contribution uses in-house adapter 5; subsequent adapter identities cover the
preservation extension and required-header master-index compatibility. The
independent `diagnostics_version` in the reader identity covers complete scalar
diagnostics and the normalization of stored tooling lists. Older
in-house provenance output is invalidated through the existing reader/configuration
identity. Runtime schema constants and reusable asset encodings remain unchanged,
and older texture or mesh bytes are not relabeled. The broad configuration hash
includes the reader identity for metadata provenance. Normal conversion can
reuse compatible assets through separately recorded per-kind settings and source
proofs. Adapter-11 metadata recognizes the exact adapter-10 combined settings
proof for independently eligible asset producers, without changing retained
bytes or their recorded producer. The metadata-only path accepts the current
reader identity and that explicit adapter-10 retention contract with matching
schema/add-on bytes and complete reader fields. Older in-house packs without
`diagnostics_version` remain refused rather than relabeled with the new tooling
contract; unknown adapter versions cannot authorize retained settings.
Header rows bind the exact authored
schema; a different schema is reported as undecodable rather than silently
interpreting an old pack with new definitions. Deterministic sorted insertion
and an explicit physical primary key make ordered history contents reproducible.

`inhouse-records history-source <pack> <owner basename> <hex full local>
[--data DIR]` also reaches nullable runtime identities. Proven source ownership
comes only from the original supplier's MAST list: lower high-byte indices name
masters, the equal index names self, and a generic greater index remains
`master_index_out_of_range` with no invented self owner. The existing GMST
greater-index convention is separately labelled `gmst_editor_id_convention`;
its physical rows and decoded EDID/runtime aliases do not establish native
owner proof. Source words of zero remain a separate null state, while nonzero
self/master words with a local component of zero retain that exact source key.
Missing-master load-order validation remains a producer limitation: this
extension does not admit or scan a wholly rejected plugin sequence.

`history-physical <pack> <decimal plugin id> <decimal source offset> [--data DIR]`
reaches every stored header, including unproven owner words. Source/physical
access requires the caller to authenticate the complete database against a
trusted completed-producer receipt/pin and preserve that integrity through
the call, retaining before/after bundle guards. Read-only SQL is not integrity
validation. Original-file checksums prove read bytes, not edited stored MAST,
slot, group-bound or candidate/winner context. These APIs do not independently
verify arbitrary edited databases; their context authority is the authenticated
producing bundle. Callers must consume/drop streamed versions to keep one-body
memory, and must reject partial delivery without a successful complete summary.
Both new commands
emit one version JSONL row at a time followed by a summary. The Rust streaming
callback exposes exact original 24-byte header and on-disk stored payload
(including compression prefix) with full original-file checksum guards before
and after reading through the same handle. It does not inflate, normalize,
repair flags, mask locals, or manufacture typed/runtime decoding. Complete
stored frames have a 64 MiB body access ceiling. A frame crossing its original
containing-group end yields header-only material even when later outer siblings
would satisfy its nominal size. Changed or unavailable originals remain local
access failures. Source queries have an explicit 4096-version capacity; callback
errors and exceeded query capacity return partial/error rather than claiming
complete access. The caller must retain the emitted partial report. Files are
not embedded or copied during conversion, so immutable originals are required
for future body access, and each queried version entails whole-original
checksum passes. The conversion retains header structs plus a sorted-reference
vector; this is not a disk-bounded all-candidate conversion design.

The report distinguishes the latest physical source candidate, latest
reader-admitted candidate, current accepted-source winner, runtime presence,
and surviving earlier runtime fallback. Deletions remain admitted history but
do not become a live source/runtime winner. Wide light locals remain proven
owner/full-local source keys with nullable runtime IDs and exact raw access;
they never spill or mask into an adjacent light slot. Generic required header
indices beyond the original MAST list use the existing supplier fallback for
runtime mapping and increment a separate bounded compatibility diagnostic.
Original ownership stays unresolved, and the full 24-bit local component is
retained in source history. Optional links keep their existing clear-or-reject
policy. Width and null-runtime checks still reject unusable candidates locally,
with their header/reason retained and valid neighbors continuing.
Full conversion atomically recreates only the owned
tooling table within the provenance transaction, so its older optional schema
does not silently survive with missing columns. There are no new sidecar files,
asset conversions or runtime schema changes. Normal asset-cache identity
invalidation remains an existing global adapter coupling for full conversion,
despite unchanged reusable asset encodings.

### 3. Worldspace Registry (`worldspaces`)

Stores worldspace hierarchy and parent world relations (e.g. Tamriel `0x0000003C`, Solstheim).

```sql
CREATE TABLE IF NOT EXISTS worldspaces (
    id INTEGER PRIMARY KEY,             -- WorldSpace FormID
    editor_id TEXT NOT NULL,            -- EDID string (e.g. 'Tamriel')
    parent_world INTEGER,               -- Parent WorldSpace FormID (if child worldspace)
    flags INTEGER NOT NULL,
    lod_origin_x INTEGER,               -- Validated sidecar/explicit origin; NULL if unresolved
    lod_origin_y INTEGER,
    full_name TEXT                      -- Display name (FULL, e.g. 'Skyrim'); see "Display names"
);
```

---

### 4. Cell Registry (`cells`)

Stores exterior cell grid coordinates and each cell's Editor ID.

```sql
CREATE TABLE IF NOT EXISTS cells (
    id INTEGER PRIMARY KEY,             -- CELL FormID
    worldspace_id INTEGER,              -- Parent WorldSpace FormID; NULL for an interior
    grid_x INTEGER,                     -- Exterior cell Grid X (NULL if interior)
    grid_y INTEGER,                     -- Exterior cell Grid Y (NULL if interior)
    interior_name TEXT,                 -- The cell's EDID (Editor ID) when it has one, exterior or
                                        -- interior; not the display name
    flags INTEGER NOT NULL,
    data BLOB,                          -- Optional cell binary payload
    full_name TEXT                      -- Display name (FULL); see "Display names" below
);
```

#### Display names (`full_name`)

`worldspaces.full_name`, `cells.full_name` and `statics.full_name` hold the record's `FULL`, the
name the game shows (door prompts, the crosshair label, container menus, the map). `npcs.full_name`
follows the same rules. `cells.interior_name` is still the cell's Editor ID (`EDID`), not its
name; it keeps its column name for compatibility.

`FULL` is a localized string. In a plugin with the TES4 `Localized` flag (0x80) it is a four-byte
ID into `Strings/<plugin>_<language>.STRINGS`, looked up in the configured language's table for the
plugin that supplied the winning record: a loose `Data/Strings` file first, then the archives.
English text uses Windows-1252; other languages use UTF-8 with Windows-1252 fallback. Plain plugin
text uses Windows-1252. Both record readers publish the same UTF-8 text:

- The in-house reader resolves every field its schema types as `lstring`.
- The legacy reader (`--record-reader legacy`) resolves the subrecords the in-house schema types
  as `lstring` in every occurrence for that record type, using the same table lookup
  (`crates/converter/src/esm/legacy_text.rs`). That covers `FULL` on every record type, and
  `DESC`, `RNAM` and others where the subrecord is unambiguous. This changed the legacy
  projection; version 4 introduced language/encoding. Version 5 also publishes every schema-declared
  projectile/hazard placement and its base model (#498). The in-house adapter is version 13.
  Version-3 retained proofs are accepted only for their historical English setting; all older
  metadata rebuilds use the configured language. Retained assets keep their independent proof.

A name whose ID is 0 is the existing null sentinel and is NULL without a loss warning. A nonzero ID
missing from the table, or a table that cannot be read, leaves NULL and adds one bounded warning
per plugin with a count and first example. The canonical `records.data` payloads hold the same
resolved text for both readers.

The columns are additive and optional. They did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`:
`create_tables` adds them to an older reused table, and a reader must probe for them.

`cells.flags` and `worldspaces.flags` are the record-header flags. The `DATA` flags (interior,
has water, show sky, ...) are in `cell_environment.data_flags` and
`worldspace_environment.data_flags` below.

#### Cell and Worldspace Environment (`cell_environment`, `cell_regions`, `cell_lighting`, `worldspace_environment`)

Authored lighting, image space, climate, music, ownership and flags (#269 gap 1). Before these
tables existed the data was only in `cells.data` / `records.data`, and the engine hard-coded
`SkyrimClear` lighting for every space (#264). Layouts follow xEdit 4.1.5f
(`Core/wbDefinitionsTES5.pas`, `CELL` and `WRLD`).

```sql
CREATE TABLE IF NOT EXISTS cell_environment (  -- one row per CELL
    cell_id INTEGER PRIMARY KEY,
    data_flags INTEGER,          -- DATA (u8 in old plugins, u16 in SSE): 0x1 interior, 0x2 has water,
                                 -- 0x4 can travel from here, 0x8 no LOD water, 0x20 public area,
                                 -- 0x40 hand changed, 0x80 show sky, 0x100 use sky lighting
    image_space_id INTEGER,      -- XCIM -> IMGS
    acoustic_space_id INTEGER,   -- XCAS -> ASPC
    music_id INTEGER,            -- XCMO -> MUSC
    sky_region_id INTEGER,       -- XCCM -> REGN the cell takes its sky and weather from
    location_id INTEGER,         -- XLCN -> LCTN
    encounter_zone_id INTEGER,   -- XEZN -> ECZN
    owner_id INTEGER,            -- XOWN -> FACT, NPC_ or ACHR
    owner_rank INTEGER,          -- XRNK, the faction rank that owns it
    water_id INTEGER,            -- XCWT -> WATR
    water_height REAL            -- XCLW; NULL when absent or not finite
);
CREATE TABLE IF NOT EXISTS cell_regions (      -- XCLR, one row per region
    cell_id INTEGER NOT NULL,
    region_id INTEGER NOT NULL,
    PRIMARY KEY (cell_id, region_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS cell_lighting (     -- one row per CELL with an XCLL or an LTMP
    cell_id INTEGER PRIMARY KEY,
    lighting_template_id INTEGER,              -- LTMP -> LGTM
    xcll_size INTEGER,                         -- 92, or 72 for the short form; NULL without XCLL
    ambient_r INTEGER, ambient_g INTEGER, ambient_b INTEGER,             -- XCLL[0..3]
    directional_r INTEGER, directional_g INTEGER, directional_b INTEGER, -- XCLL[4..7]
    fog_near_r INTEGER, fog_near_g INTEGER, fog_near_b INTEGER,          -- XCLL[8..11]
    fog_near REAL, fog_far REAL,                                         -- XCLL[12], [16]
    directional_rotation_xy INTEGER, directional_rotation_z INTEGER,     -- XCLL[20], [24], degrees
    directional_fade REAL, fog_clip_distance REAL, fog_power REAL,       -- XCLL[28], [32], [36]
    dalc_x_pos_r INTEGER, dalc_x_pos_g INTEGER, dalc_x_pos_b INTEGER,    -- XCLL[40..72]: directional
    dalc_x_neg_r INTEGER, dalc_x_neg_g INTEGER, dalc_x_neg_b INTEGER,    -- ambient X+, X-, Y+, Y-,
    dalc_y_pos_r INTEGER, dalc_y_pos_g INTEGER, dalc_y_pos_b INTEGER,    -- Z+, Z-, specular, scale
    dalc_y_neg_r INTEGER, dalc_y_neg_g INTEGER, dalc_y_neg_b INTEGER,
    dalc_z_pos_r INTEGER, dalc_z_pos_g INTEGER, dalc_z_pos_b INTEGER,
    dalc_z_neg_r INTEGER, dalc_z_neg_g INTEGER, dalc_z_neg_b INTEGER,
    dalc_specular_r INTEGER, dalc_specular_g INTEGER, dalc_specular_b INTEGER,
    dalc_scale REAL,
    fog_far_r INTEGER, fog_far_g INTEGER, fog_far_b INTEGER,             -- XCLL[72..75]
    fog_max REAL, light_fade_begin REAL, light_fade_end REAL,            -- XCLL[76], [80], [84]
    inherit_flags INTEGER                                                -- XCLL[88]
);
CREATE TABLE IF NOT EXISTS worldspace_environment ( -- one row per WRLD
    worldspace_id INTEGER PRIMARY KEY,
    data_flags INTEGER,          -- DATA: 0x1 small world, 0x2 can't fast travel, 0x8 no LOD water,
                                 -- 0x10 no landscape, 0x20 no sky, 0x40 fixed dimensions, 0x80 no grass
    parent_flags INTEGER,        -- PNAM byte 0: 0x1 use land, 0x2 LOD, 0x4 map, 0x8 water,
                                 -- 0x10 climate, 0x40 sky cell data from the parent (WNAM)
    climate_id INTEGER,          -- CNAM -> CLMT
    water_id INTEGER,            -- NAM2 -> WATR
    lod_water_id INTEGER,        -- NAM3 -> WATR
    lod_water_height REAL,       -- NAM4
    default_land_height REAL,    -- DNAM[0]
    default_water_height REAL,   -- DNAM[4]
    lighting_template_id INTEGER, -- LTMP -> LGTM (interior lighting)
    music_id INTEGER,            -- ZNAM -> MUSC
    location_id INTEGER,         -- XLCN -> LCTN
    encounter_zone_id INTEGER,   -- XEZN -> ECZN
    bounds_min_x REAL, bounds_min_y REAL, bounds_max_x REAL, bounds_max_y REAL, -- NAM0, NAM9
    lod_multiplier REAL,         -- NAMA
    center_cell_x INTEGER, center_cell_y INTEGER -- WCTR, fixed-dimensions centre cell
);
```

Rules:

- Colours are 8-bit sRGB; each one's fourth byte is unused and not stored. The `dalc_*` columns
  are the cell's directional ambient lighting colours (what `LGTM` and `WTHR` keep in a `DALC`).
- `XCLL` is 92 bytes in Skyrim SE. xEdit accepts it from 72 bytes, ending after the directional
  ambient block; the fields past the end are then NULL. An `XCLL` under 72 bytes is ignored.
  Floats that are not finite are NULL. `inherit_flags` says which fields the lighting template
  supplies instead: 0x1 ambient, 0x2 directional, 0x4 fog colour, 0x8 fog near, 0x10 fog far,
  0x20 directional rotation, 0x40 directional fade, 0x80 clip distance, 0x100 fog power,
  0x200 fog max, 0x400 light fade distances. The template is in `lighting_templates` below.
- Every link column holds a load-order FormID, and only when the winning record it names exists
  and has the type listed; otherwise it is NULL (a `cell_regions` row is skipped). The in-house
  reader already clears links that do not resolve and the legacy reader keeps them, so this check
  is what makes both readers publish the same rows.
- A typed field whose length the in-house schema rejects (for example a `DATA` that is neither 1
  nor 2 bytes, or an `XCLW` that is not 4) is NULL for both readers: the in-house reader leaves it
  out of its runtime projection, so the projection applies the same sizes to legacy records.
- The tables are derived data: a full load-order export recreates them from the winning records.
  The movement-annotation subset export leaves them alone, and diagnostic archives leave them
  empty (they are deferred runtime views, like the room tables).

#### Weather, Climate, Image Space and Lighting Templates (`weathers`, `weather_colors`, `weather_directional_ambient`, `climates`, `climate_weathers`, `region_weathers`, `image_spaces`, `lighting_templates`)

The records Skyrim's sky and interior lighting come from (#269 gap 2). Together with the tables
above, this is the data the engine needs to replace its hard-coded `SkyrimClear` day (#264).

```sql
CREATE TABLE IF NOT EXISTS weathers (          -- one row per WTHR
    id INTEGER PRIMARY KEY, editor_id TEXT,
    fog_day_near REAL, fog_day_far REAL, fog_night_near REAL, fog_night_far REAL,   -- FNAM (32 bytes)
    fog_day_power REAL, fog_night_power REAL, fog_day_max REAL, fog_night_max REAL,
    wind_speed INTEGER, transition_delta INTEGER, sun_glare INTEGER, sun_damage INTEGER, -- DATA (19 bytes),
    precipitation_begin INTEGER, precipitation_end INTEGER,                           -- raw u8 values
    lightning_begin INTEGER, lightning_end INTEGER, lightning_frequency INTEGER,
    classification_flags INTEGER,              -- DATA[11]: 0x1 pleasant, 0x2 cloudy, 0x4 rainy, 0x8 snow, ...
    lightning_r INTEGER, lightning_g INTEGER, lightning_b INTEGER,
    visual_effect_begin INTEGER, visual_effect_end INTEGER,
    wind_direction INTEGER, wind_direction_range INTEGER,
    disabled_cloud_layers INTEGER,             -- NAM1 bits
    precipitation_id INTEGER,                  -- MNAM -> SPGD
    visual_effect_id INTEGER,                  -- NNAM -> RFCT
    sky_static_id INTEGER,                     -- TNAM -> STAT
    sun_glare_lens_id INTEGER,                 -- GNAM -> LENS
    image_space_sunrise_id INTEGER, image_space_day_id INTEGER,              -- IMSP -> IMGS
    image_space_sunset_id INTEGER, image_space_night_id INTEGER,
    volumetric_lighting_sunrise_id INTEGER, volumetric_lighting_day_id INTEGER, -- HNAM -> VOLI
    volumetric_lighting_sunset_id INTEGER, volumetric_lighting_night_id INTEGER
);
CREATE TABLE IF NOT EXISTS weather_colors (    -- NAM0: one row per colour type and time of day
    weather_id INTEGER NOT NULL,
    color_type TEXT NOT NULL,                  -- sky_upper, fog_near, unknown, ambient, sunlight, sun,
                                               -- stars, sky_lower, horizon, effect_lighting,
                                               -- cloud_lod_diffuse, cloud_lod_ambient, fog_far,
                                               -- sky_statics, water_multiplier, sun_glare, moon_glare
    time_of_day TEXT NOT NULL,                 -- sunrise, day, sunset, night
    r INTEGER NOT NULL, g INTEGER NOT NULL, b INTEGER NOT NULL,
    PRIMARY KEY (weather_id, color_type, time_of_day)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS weather_directional_ambient ( -- the four DALC subrecords, in that order
    weather_id INTEGER NOT NULL, time_of_day TEXT NOT NULL,
    x_pos_r INTEGER, x_pos_g INTEGER, x_pos_b INTEGER, ...,   -- x_pos, x_neg, y_pos, y_neg, z_pos,
    specular_r INTEGER, specular_g INTEGER, specular_b INTEGER, -- z_neg, then specular and scale
    scale REAL,
    PRIMARY KEY (weather_id, time_of_day)
);
CREATE TABLE IF NOT EXISTS climates (          -- one row per CLMT
    id INTEGER PRIMARY KEY, editor_id TEXT, sun_texture TEXT, sun_glare_texture TEXT, -- FNAM, GNAM
    sunrise_begin INTEGER, sunrise_end INTEGER,   -- TNAM (6 bytes), in units of 10 minutes
    sunset_begin INTEGER, sunset_end INTEGER,
    volatility INTEGER, moon_phase INTEGER        -- TNAM[4], TNAM[5] (moons and phase length bits)
);
CREATE TABLE IF NOT EXISTS climate_weathers (  -- CLMT WLST, in file order
    climate_id INTEGER NOT NULL, position INTEGER NOT NULL,
    weather_id INTEGER,                        -- -> WTHR
    chance INTEGER NOT NULL,
    chance_global_id INTEGER,                  -- -> GLOB
    PRIMARY KEY (climate_id, position)
);
CREATE TABLE IF NOT EXISTS region_weathers (   -- REGN RDWT, in file order across the record
    region_id INTEGER NOT NULL, position INTEGER NOT NULL,
    weather_id INTEGER, chance INTEGER NOT NULL, chance_global_id INTEGER,
    PRIMARY KEY (region_id, position)
);
CREATE TABLE IF NOT EXISTS image_spaces (      -- one row per IMGS
    id INTEGER PRIMARY KEY, editor_id TEXT,
    eye_adaptation_speed REAL, bloom_blur_radius REAL, bloom_threshold REAL, bloom_scale REAL, -- HNAM
    receive_bloom_threshold REAL, white REAL, sunlight_scale REAL, sky_scale REAL,              -- (36 bytes)
    eye_adaptation_strength REAL,
    saturation REAL, brightness REAL, contrast REAL,       -- CNAM
    tint_amount REAL, tint_r REAL, tint_g REAL, tint_b REAL, -- TNAM, colour as 0..1 floats
    dof_strength REAL, dof_distance REAL, dof_range REAL,  -- DNAM (12 or 16 bytes)
    dof_sky_blur_radius INTEGER                            -- DNAM[14], raw u16 code
);
CREATE TABLE IF NOT EXISTS lighting_templates ( -- one row per LGTM
    id INTEGER PRIMARY KEY, editor_id TEXT,
    data_size INTEGER,                          -- 64, 68, 72, 76, 80, 88 or 92; NULL otherwise
    ...,                                        -- DATA: the same columns as cell_lighting, from
                                                -- ambient_r to light_fade_end (DATA[88..92] unknown)
    subrecord_dalc_x_pos_r INTEGER, ...,        -- the separate DALC subrecord, laid out like
    subrecord_dalc_scale REAL                   -- weather_directional_ambient
);
```

Rules beyond the ones above:

- `weather_colors` has a row for each colour type `NAM0` holds: 13 to 17 types of 16 bytes (sunrise,
  day, sunset, night; each RGB and an unused byte). A `NAM0` of another length gives no rows.
- A `DALC` is 24, 28 or 32 bytes (the specular colour and the scale are optional); missing fields
  are NULL.
- Links follow the same existence and type check as above. `CLMT` `WLST` and `WTHR` `IMSP`, `HNAM`,
  `MNAM`, `NNAM`, `TNAM`, `GNAM` and `SNAM` are now remapped by the legacy reader too, as the
  in-house schema already did (see the remapped-field table).

**These tables are optional and did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`.**
`create_tables` adds them to an older reused database. The engine reads them into
`world::environment::EnvironmentCatalog` (cell lighting, worldspace environment, lighting
templates, weathers with their colours and directional ambient, image spaces, climate weather
lists). A table that is missing or lacks a column the reader needs loads as empty, with one log
line. Nothing renders from the catalog yet: the hard-coded `SkyrimClear` lighting in `sky.rs` is
unchanged.

| Case | Converter writes | Engine |
|---|---|---|
| Old database, tables absent | n/a | empty catalog, one log line, built-in lighting |
| Table of another shape | n/a | empty catalog for that table, one log line |
| Cell without XCLL or LTMP | `cell_environment` row only | no authored lighting for it |
| Link to a missing or wrong-type record | NULL | n/a |

---

### 5. Placed World References (`references`)

Stores 3D positions, rotations, scales, and cell parentage for placed world objects (`REFR`,
`ACHR`, `ACRE`, `PGRE`, `PMIS`, `PHZD`, `PARW`, `PBAR`, `PBEA`, `PCON`, `PFLA`).

```sql
CREATE TABLE IF NOT EXISTS "references" (
    id INTEGER PRIMARY KEY,
    cell_id INTEGER NOT NULL,           -- Parent CELL FormID
    worldspace_id INTEGER,              -- Parent WorldSpace FormID; NULL in an interior
    base_form_id INTEGER NOT NULL,      -- Base Object FormID (NAME)
    is_exterior INTEGER NOT NULL,       -- 1 when the parent cell has a worldspace or a grid
    pos_x REAL NOT NULL,                -- 3D X Coordinate
    pos_y REAL NOT NULL,                -- 3D Y Coordinate
    pos_z REAL NOT NULL,                -- 3D Z Coordinate
    local_x REAL,                       -- Exterior only: pos_x within its 4096-unit grid cell
    local_y REAL,                       -- (from the position, not the owning CELL); NULL inside
    rot_x REAL NOT NULL,                -- Rotation X (Radians)
    rot_y REAL NOT NULL,                -- Rotation Y (Radians)
    rot_z REAL NOT NULL,                -- Rotation Z (Radians)
    scale REAL NOT NULL DEFAULT 1.0,    -- Scale multiplier
    radius_override REAL,               -- signed XRDS radius offset in Creation units (NULL when absent/invalid)
    header_flags INTEGER NOT NULL DEFAULT 0, -- Winning record header flags, stored raw
    enable_parent_id INTEGER,           -- Remapped XESP parent FormID; NULL when absent
    enable_parent_flags INTEGER,        -- XESP byte 4 (0 through 255); NULL when absent
    emittance_id INTEGER,               -- Remapped XEMI light or region FormID; NULL when absent
    data BLOB                           -- Subrecords payload
);

-- Ordinary SQLite index used for interior cell reference lookup
CREATE INDEX IF NOT EXISTS idx_references_cell ON "references"(cell_id);
```

The reference flag and enable-parent columns preserve winning raw-record
metadata for initial spawn filtering (#165) and later object LOD (#106).
The engine resolves parent chains before spawning. With no parent, header bit
`0x800` marks an initially disabled reference; the documented rule and
runtime limits are in [reference enable-state evidence](../../research/reference-enable-state-20261005.md).
Current terrain LOD does not consume them. `header_flags` comes from the record header. `enable_parent_id` is
XESP's little-endian parent FormID (bytes 0-3), resolved through plugin load order;
`enable_parent_flags` is byte 4 alone in both readers. Bit 0 inverts the parent's
state and bit 1 enables pop-in. The three unused bytes (5-7) remain unchanged
in the canonical subrecords serialized in `data`. Older legacy projections could
include padding in the normalized flags value; reconversion corrects that
projection. An invalid parent link is published as
`enable_parent_id = 0` (no parent) with its flags kept; a malformed `XESP` is
dropped, so both columns are NULL. See the remapped-field table below.

`emittance_id` is the reference's `XEMI`: the light or region whose colour the
`External_Emittance` surfaces of its model take (§16). A cleared link is stored as NULL.

#### Base object models (`statics`)

`references.base_form_id` joins `statics.id`. The runtime spawns a reference only when its base
has a `statics` row whose `model_path` it can resolve, so a base type missing from this table
never renders.

```sql
CREATE TABLE IF NOT EXISTS statics (
    id INTEGER PRIMARY KEY,                    -- base record FormID, load-order numbering
    editor_id TEXT,                            -- EDID
    model_path TEXT,                           -- MODL (ARMO: MOD2, then MOD4); NULL when absent
    flags INTEGER NOT NULL,                    -- record header flags
    full_name TEXT,                            -- FULL, resolved localized text when available
    bounds_min_x REAL NOT NULL DEFAULT -64, bounds_min_y REAL NOT NULL DEFAULT -64,
    bounds_min_z REAL NOT NULL DEFAULT -64, bounds_max_x REAL NOT NULL DEFAULT 64,
    bounds_max_y REAL NOT NULL DEFAULT 64, bounds_max_z REAL NOT NULL DEFAULT 64,
    bounds_valid INTEGER NOT NULL DEFAULT 0    -- 1 once integration measured the converted GLB
);
```

Rows come from every placeable base type with a world model: `STAT`, `MSTT`, `FURN`, `TREE`,
`FLOR`, `CONT`, `DOOR`, `ACTI`, `TACT`, `WEAP`, `MISC`, `BOOK`, `AMMO`, `ALCH`, `INGR`, `SLGM`,
`KEYM`, `SCRL`, `ARMO`, `APPA`, `ADDN`, `ARTO`, `PROJ`, `HAZD`, and `LIGH` when it has a model. `TACT`
(talking activators), `APPA` (apparatus), `ADDN` (addon nodes) and `ARTO` (art objects) were
added for #269; before that their references had no row and did not render. The engine still
skips models under `effects/`, `sky/` or `markers/`
(`converted_model_path` in `crates/engine/src/streaming.rs`), which covers many `ADDN` and `ARTO`
models; that is render policy, not data the table lacks. Marker filenames alone are not excluded:
the base record's `Is Marker` flag controls editor-marker admission. They are additive
rows, so no `shared::WORLD_DATABASE_SCHEMA_VERSION` bump: an older database keeps loading and
simply lacks them until reconverted. Mesh conversion discovers NIF inputs independently of this
table, so their models were already converted. Both record readers produce the same rows.

#### Reference Links and Details (`reference_linked_refs`, `reference_activate_parents`, `reference_details`)

What a placed reference is linked to, what activates it, who owns it, how it is locked and
whether it is a map marker (#269 gap 3). Patrols, idle markers, traps, levers, pressure plates,
stealing and locked doors need these; before they were only in `references.data`. Written for
every exported placed reference (the eleven source types listed above). Layouts follow xEdit
4.1.5f (`Core/wbDefinitionsTES5.pas`, `REFR`).

```sql
CREATE TABLE IF NOT EXISTS reference_linked_refs (       -- XLKR, one row per subrecord in file order
    reference_id INTEGER NOT NULL,                       -- references.id
    position INTEGER NOT NULL,
    keyword_id INTEGER,                                  -- -> KYWD; NULL for the 4-byte form
    linked_ref_id INTEGER,                               -- -> a placed reference
    PRIMARY KEY (reference_id, position)
);
CREATE TABLE IF NOT EXISTS reference_activate_parents (  -- XAPR, one row per subrecord
    reference_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    parent_ref_id INTEGER,                               -- -> a placed reference
    delay REAL,                                          -- seconds; NULL if not finite
    PRIMARY KEY (reference_id, position)
);
CREATE TABLE IF NOT EXISTS reference_details (           -- one row per reference with any of these
    reference_id INTEGER PRIMARY KEY,
    owner_id INTEGER,                    -- XOWN -> FACT, NPC_ or ACHR
    owner_rank INTEGER,                  -- XRNK, the faction rank that owns it
    activate_parent_flags INTEGER,       -- XAPD: 0x1 parent activate only
    lock_level INTEGER,                  -- XLOC[0]: 1 novice, 25 apprentice, 50 adept, 75 expert,
                                         -- 100 master, 255 requires key
    lock_key_id INTEGER,                 -- XLOC[4..8] -> KEYM, remapped here (see below)
    lock_flags INTEGER,                  -- XLOC[8]: 0x4 leveled lock
    open_by_default INTEGER NOT NULL DEFAULT 0, -- ONAM present (doors)
    map_marker INTEGER NOT NULL DEFAULT 0,      -- XMRK present
    map_marker_flags INTEGER,            -- FNAM: 0x1 visible, 0x2 can travel to, 0x4 "show all" hidden
    map_marker_type INTEGER,             -- TNAM[0], the marker icon enum
    location_id INTEGER,                 -- XLCN -> LCTN (persistent location)
    location_ref_type_id INTEGER,        -- XLRL -> LCRT or LCTN
    encounter_zone_id INTEGER,           -- XEZN -> ECZN
    attached_ref_id INTEGER              -- XATR -> a placed reference
);
```

Rules:

- These detail rows are projected from every winning placed source type listed in §5.
  Target types are field-specific and are not the same set as source types:
  `XLKR.keyword_id` accepts `KYWD`, `PLYR`, `ACHR`, `REFR`, `PGRE`, `PHZD`, `PMIS`, `PARW`,
  `PBAR`, `PBEA`, `PCON` and `PFLA`; `XLKR.linked_ref_id` and `XAPR.parent_ref_id` accept
  `PLYR`, `ACHR`, `REFR`, `PGRE`, `PHZD`, `PMIS`, `PARW`, `PBAR`, `PBEA`, `PCON` and `PFLA`.
  `XATR.attached_ref_id` accepts `REFR`, `PGRE`, `PHZD`, `PMIS`, `PARW`, `PBAR`, `PBEA`, `PCON`
  and `PFLA` (not `ACHR` or `ACRE`). `owner_id` accepts `FACT`, `ACHR` or `NPC_`; `lock_key_id`
  accepts `KEYM`; `XLCN` accepts `LCTN`; `XLRL` accepts `LCRT` or `LCTN`; and `XEZN` accepts
  `ECZN`. A zero ID resolves to NULL. A nonzero hardcoded ID below `0x800` is retained if no
  plugin record exists; a present winner must be non-deleted and have an allowed type. Other
  targets resolve to NULL. Valid-length `XLKR` and `XAPR` rows are kept in source order with
  contiguous positions among accepted-length entries, even when a target resolves to NULL. This
  makes legacy projection match the in-house reader's invalid-link clearing behavior.
- A field whose length the in-house schema rejects is skipped for both readers: `XLKR` must be 4
  or 8 bytes, `XAPR` 8, `XAPD` 1, `XOWN`/`XRNK`/`XLCN`/`XLRL`/`XEZN`/`XATR` 4, `FNAM` 1 and `TNAM`
  2.
- `XLOC` is untyped bytes in the in-house schema, so neither reader remaps its key in
  `references.data`. The projection remaps the key through the supplying plugin's master list
  (`records::source_identity::header_runtime_id`) and applies the normal `KEYM` target check,
  including retention of a nonzero absent hardcoded ID below `0x800`. A subset export without a
  load order leaves `lock_key_id` NULL.
- The map marker's name (`FULL`, a localized string) is not projected into these tables yet.
  Both readers retain its resolved text in the canonical record payload after #271.
- Derived data like the other runtime views: a full load-order export recreates the tables, and
  diagnostic archives leave them empty.

**These tables are optional and did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`.**
`create_tables` adds them to an older reused database. No engine code reads them yet; a reader
must probe for them first.


---

### 6. Hybrid Spatial Indexing (`exterior_spatial` & Interior `cell_id` Index)

Exterior references use a spatial index with a worldspace filter; interior references use direct cell lookup:

1. **Exterior Worldspace R-Tree (`exterior_spatial`):** Bounds are the reference's global X/Y/Z point position, with equal minimum and maximum values. The exporter does not constrain coordinate magnitudes. SQLite stores single-precision bounds; `cell_id` and `worldspace_id` are auxiliary columns, not spatial dimensions. Cell-local X/Y offsets remain separate columns in `references`.
2. **Interior Cell Index (`idx_references_cell`):** Interior cells do not use the R-Tree. An ordinary SQLite index on `references(cell_id)` supports the engine's interior reference selection; this description makes no latency or complexity claim.

```sql
-- R-Tree virtual table for Exterior 3D bounding box spatial queries
CREATE VIRTUAL TABLE IF NOT EXISTS exterior_spatial USING rtree(
    id,                                 -- Matches internal reference ID
    minX, maxX,                         -- Global reference X position
    minY, maxY,                         -- Global reference Y position
    minZ, maxZ,                         -- World Z Height units
    +cell_id,                           -- Exterior CELL FormID
    +worldspace_id                      -- Parent WorldSpace FormID (e.g. 0x0000003C for Tamriel)
);
```

---

### 7. Terrain Heightmaps (`land`)

Stores 33x33 terrain heightmap data, vertex textures (`vtex`), vertex colors (`vclr`) and
vertex normals extracted from the winning `LAND` record of each cell.

```sql
CREATE TABLE IF NOT EXISTS land (
    cell_id INTEGER PRIMARY KEY,        -- Parent CELL FormID
    heightmap BLOB NOT NULL,            -- VHGT: 33x33 height deltas
    vtex BLOB,                          -- VTEX: land texture FormIDs
    vclr BLOB,                          -- VCLR: vertex colors
    normals BLOB                        -- VNML: vertex normals
);
```

---

### 8. Level of Detail (`lod_chunks`, `lod_chunks_spatial`, `lod_build`)

Schema 5 stores spatial metadata and file-backed GLB payloads, not geometry
blobs. Source-cell names are serialized text used for full-detail handoff.
Bounds are world-space Creation units; `payload_path` is relative to the asset
root and `content_hash` is the payload's SHA-256. The single build row matches
`lod-manifest.json`.

```sql
CREATE TABLE IF NOT EXISTS lod_chunks (
    worldspace_id INTEGER NOT NULL, tier INTEGER NOT NULL,
    anchor_x INTEGER NOT NULL, anchor_y INTEGER NOT NULL,
    payload_path TEXT NOT NULL, content_hash TEXT NOT NULL,
    bounds_min_x REAL NOT NULL, bounds_min_y REAL NOT NULL, bounds_min_z REAL NOT NULL,
    bounds_max_x REAL NOT NULL, bounds_max_y REAL NOT NULL, bounds_max_z REAL NOT NULL,
    source_cells TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (worldspace_id, tier, anchor_x, anchor_y)
);
CREATE VIRTUAL TABLE IF NOT EXISTS lod_chunks_spatial USING rtree(
    id, minX, maxX, minY, maxY, +worldspace_id, +tier, +anchor_x, +anchor_y
);
CREATE TABLE IF NOT EXISTS lod_build (
    id INTEGER PRIMARY KEY CHECK (id = 1), build_identity TEXT NOT NULL
);
```

The unused pre-schema-6 `lod` blob placeholder is retained. Terrain LOD
uses `lod_chunks` for indexing and external GLB files for payloads. See
[ADR-0010](../../adr/0010-lod-chunk-payload-format.md).

---

### 9. Script attachments (`scripts`)

In-house output stores attached Papyrus script names and canonical VMAD bytes.
These are property bindings, not compiled PEX bytecode. Script status and
authored order remain in VMAD. The last exact-name attachment for a record keeps
the existing SQL overwrite behavior.

```sql
CREATE TABLE scripts (
    form_id INTEGER NOT NULL,
    script_name TEXT NOT NULL,
    vmad BLOB NOT NULL,
    PRIMARY KEY (form_id, script_name)
);
```

`script_properties` has `(form_id,script_name,property_index)` as its primary key,
with scalar property name, native `property_type`, status and `value_count`.
`script_property_values` adds a zero-based `value_index` to that key and stores
nullable `object_form_id`, `alias_id`, `string_value`, `integer_value`,
`float_bits` and `boolean_value` columns. Scalar properties have one value row;
arrays keep their authored element order. Native type zero and empty arrays
have no value rows and remain distinguishable by their property headers. Float
bits preserve all f32 representations without JSON or SQLite numeric coercion.

These tables replace `scripts.properties_json` for in-house exports inside the
existing record transaction. Complete exports refresh their owned script
snapshot; record archives leave all three runtime script tables empty. The
legacy reader's existing JSON projection remains separate. Runtime engine
consumers continue using the canonical record/VMAD data. Public partial exports
into a normalized database use scalar script rows without refreshing unrelated
attachments; fresh legacy databases retain the legacy JSON projection.

---

### 10. FormID Translation Map (`formid_map`)

Maps each resolved 32-bit FormID to its stable owning plugin and plugin-local
ID. `plugin_name` is the lowercase owning filename, such as `skyrim.esm`;
`internal_id` is the low 24 bits for full plugins or the low 12 bits for light
plugins. Full and light slots are assigned independently. The pair
`(plugin_name, internal_id)` survives changes to the load-order slots.
`plugin_name` uses the reader's shared Unicode lowercase filename key; join to
`plugins` by applying that same normalization to `plugins.name`. SQLite's
built-in `lower` alone does not normalize accented Windows-1252 case pairs.

Ownership differs from override provenance: `records.load_order` identifies
the winning plugin's priority. Game settings override by case-insensitive
EditorID and retain the first definition's identity. A deletion with no EDID
resolves through any previously encountered non-null FormID alias; an unknown
header-only deletion is skipped with a warning. Later restorations keep the
original identity. Ambiguous aliases and live settings without an EDID are
errors, rather than silently replacing or dropping another record.

Known FormID fields inside the published subrecord blobs (`records.data`,
`cells.data`, `references.data`) are rewritten into the same load-order numbering
as the record keys, so a consumer can look them up directly. That covers the
single-FormID fields in the converter's `is_form_id_subrecord`, the
LAND/GRAS/LTEX payloads, the primary VMAD script-property references, and these
reference and cell fields, validated against their expected sizes:

| Field | Records | FormIDs |
| --- | --- | --- |
| `XTEL` | placed references | door destination reference (bytes 0-3 of 32) |
| `XESP` | placed references | enable parent (bytes 0-3 of 8; flags and unused bytes preserved) |
| `XLKR` | placed references | keyword and linked reference (8 bytes), or the linked reference alone (legacy 4 bytes) |
| `XNDP` | placed references | navmesh (bytes 0-3 of 8) |
| `XEMI` | placed references | emitted light or region |
| `XATR` | placed references | attached reference (the in-house reader already remapped it; the legacy reader does since #269) |
| `SNAM`, `LNAM` | LIGH | sound and lens flare (the in-house reader already remapped them; the legacy reader does since #269) |
| `XAPR` | placed references | activate-parent reference (one 8-byte subrecord per parent) |
| `XLRT` | placed references | location reference types (array) |
| `XHOR` | ACHR | horse reference |
| `LTMP` | CELL, WRLD | lighting template |
| `XCIM`, `XCMO`, `XCAS` | CELL | image space, music type, acoustic space |
| `XCCM` | CELL | region the cell takes its sky and weather from |
| `XCLR` | CELL | regions (array) |
| `ZNAM` | WRLD | music (the in-house reader already remapped it; the legacy reader does since #269) |
| `RDWT` | REGN | weathers: (weather, chance, global) entries of 12 bytes; weather and global are FormIDs |
| `WLST` | CLMT | weathers, laid out like `RDWT` |
| `MNAM`, `NNAM`, `TNAM`, `GNAM` | WTHR | precipitation, visual effect, sky static, sun glare lens |
| `IMSP`, `HNAM` | WTHR | image spaces and volumetric lighting at sunrise, day, sunset, night (16 bytes) |
| `SNAM` | WTHR | sound descriptor (bytes 0-3 of 8; one subrecord per sound) |
| `XPOD` | REFR | the rooms a portal marker joins: an array of (origin, destination) pairs; every u32 a FormID (xEdit 4.1.5f defs) |
| `XLRM` | REFR | linked room of a room marker |
| `LNAM`, `INAM` | REFR | room marker lighting template, image space (other record types keep their own handling) |

For blob remapping, placed references are REFR, ACHR, ACRE, PGRE, PMIS, PHZD,
PARW, PBAR, PBEA, PCON and PFLA. This does not expand the record types exported
to the `references` table; the six newly covered types retain their blobs in
`records` without adding `references` rows.
Fields not covered may still hold plugin-local FormIDs. In particular, do not
consume `WRLD.RNAM` large-reference lists, `XLOC` lock keys (use `reference_details.lock_key_id`), `XPWR` water
reflections as resolved IDs. Other
unconverted fields include `XLIB`, `XMBR`, `XTNM`, `PDTO` and
`CELL.XILL`. Check the converter's field-specific handling before using
these payloads; this list is not exhaustive.

The fields in this table validate master indices and light-plugin local IDs.
An out-of-range index or light-plugin local ID wider than 12 bits is an invalid
optional link: the converter sets that FormID to zero and reports a warning,
aggregated per source plugin with the count and first record/field diagnostic.
Other records and valid links continue to convert. Malformed field lengths cause
the entire subrecord to be dropped and counted in the same per-plugin warning,
since its FormID offsets cannot be decoded safely. An array with an incomplete
entry is dropped in full; valid repeated subrecords are retained. Existing
validation of record headers and required fields is unchanged. Zero FormIDs
remain zero. Other bytes in retained subrecords (such as teleport coordinates,
enable flags, navmesh triangles and activation delays) are preserved. Unlisted fields remain opaque,
not an assurance that all FormIDs in arbitrary Skyrim or mod subrecords have
been resolved.

Existing packs must be converted again to obtain these corrected links. This
does not change the schema-4 table or blob layout, so the database and asset
cache versions are unchanged. Every pipeline run, including resume, rebuilds
the database from source plugins while reusing unaffected assets. Old packs
are still accepted by the runtime: schema 4 alone does not certify these links
were remapped. A future consumer of these fields must account for old packs.
`export_to_db` without a load order refreshes already-resolved records (for
example movement annotations), preserving established `formid_map` ownership;
it neither resolves plugin-local IDs nor upgrades an old pack.

```sql
CREATE TABLE IF NOT EXISTS formid_map (
    form_id INTEGER PRIMARY KEY,       -- Resolved 32-bit Skyrim FormID
    plugin_name TEXT NOT NULL,          -- Owning plugin filename
    internal_id INTEGER NOT NULL,       -- Plugin-local ID (24 or 12 bits)
    record_type TEXT NOT NULL           -- Record type ('REFR', 'NPC_', etc.)
);
```

---

### 11. Asset Conversion Cache (`conversion_cache`)

Stores plugin file path hashes and timestamps to bypass re-converting unchanged files.

```sql
CREATE TABLE IF NOT EXISTS conversion_cache (
    plugin_path TEXT PRIMARY KEY,
    file_hash BLOB NOT NULL,
    last_converted INTEGER NOT NULL
);
```

---

### 12. Point Light Sources (`lights`)

One row per `LIGH` base record: radius, colour, flags, falloff exponent and the
optional `FNAM` fade, which is what the runtime places a point light from. The
radius is a `DATA` `u32` in Creation units widened to a float, and the row is
written whether or not the record has a `MODL`: an invisible light still lights
the space, and most `LIGH` records in `Skyrim.esm` are invisible. A record whose
`DATA` is missing or holds fewer than the 20 bytes these columns need gets no
row rather than invented values, and a `LIGH` without a `MODL` gets no `statics`
row either - there would be no mesh to draw.

A reference that places a light usually carries a signed `XRDS` radius offset, stored in
`references.radius_override` (10,810 of the 12,148 `LIGH` references in
`Skyrim.esm`). Add this signed offset to the base radius for that placement.

```sql
CREATE TABLE IF NOT EXISTS lights (
    id INTEGER PRIMARY KEY,        -- LIGH FormID
    editor_id TEXT,
    radius REAL NOT NULL,          -- Creation units (DATA u32)
    color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
    flags INTEGER NOT NULL,        -- DATA flags (dynamic, can carry, negative, flicker, off by default, ...)
    falloff REAL NOT NULL,
    fade REAL,                     -- FNAM, if present
    time INTEGER,                  -- DATA[0] i32, the light's duration
    field_of_view REAL,            -- DATA[20], spotlight cone
    near_clip REAL,                -- DATA[24]
    flicker_period REAL,           -- DATA[28], raw (xEdit shows it multiplied by 0.01)
    flicker_intensity_amplitude REAL, -- DATA[32]
    flicker_movement_amplitude REAL,  -- DATA[36]
    value INTEGER,                 -- DATA[40] u32
    weight REAL,                   -- DATA[44]
    sound_id INTEGER,              -- SNAM -> SNDR
    lens_id INTEGER                -- LNAM -> LENS (lens flare)
);
```

The columns from `time` on were added for #269 gap 6. They come from the full 48-byte `DATA`
(xEdit 4.1.5f `wbDefinitionsTES5.pas`, `LIGH`); the in-house schema accepts only that length, so
a shorter `DATA` leaves them NULL for both readers, and floats that are not finite are NULL.
`sound_id` and `lens_id` keep a FormID only when the winning record exists with that type (the
in-house reader clears links that do not resolve and the legacy reader keeps them), and the legacy
reader now remaps `LIGH` `SNAM`/`LNAM` as the in-house schema already did. The columns are
additive and optional, with no `shared::WORLD_DATABASE_SCHEMA_VERSION` bump: `create_tables` adds
them to an older reused `lights` table. The engine reads radius, colour, flags, `fade`, `falloff` and `fade_status`.

---

### 13. Water Definitions (`waters`)

Stores one row per `WATR` record: Skyrim's per-water colours and reflectivity, decoded from the
record's `DNAM` subrecord (offsets in `docs/research/water.md` section 1.2), plus the raw
subrecords for anything not broken out into a column.

```sql
CREATE TABLE IF NOT EXISTS waters (
    id INTEGER PRIMARY KEY,             -- WATR FormID
    editor_id TEXT,
    opacity INTEGER,                    -- ANAM, 0-100
    flags INTEGER NOT NULL,             -- record header flags
    shallow_color INTEGER,              -- DNAM+40: packed 0x00BBGGRR
    deep_color INTEGER,                 -- DNAM+44: packed 0x00BBGGRR
    reflection_color INTEGER,           -- DNAM+48: packed 0x00BBGGRR
    fresnel REAL,                       -- DNAM+24: Fresnel Amount (Schlick F0)
    reflectivity REAL,                  -- DNAM+20: Reflectivity Amount
    flow_normal_path TEXT,              -- NAM5, canonicalised (SSE flowmap waters only)
    data BLOB NOT NULL                  -- Serialized subrecords payload
);
```

`shallow_color`/`deep_color`/`reflection_color`/`fresnel`/`reflectivity` are additive columns: a
`skyrim_world.db` built before they existed has a `waters` table without them, and
`AssetCatalog::water_colors` (`crates/engine/src/world/database.rs`) returns `None` for every
water against such a database rather than failing to open it. The engine falls back to Skyrim's
DefaultWater values (`render::DEFAULT_WATER_FRESNEL` / `render::DEFAULT_WATER_REFLECTIVITY`, and
`streaming.rs`'s deep-colour constant) until a reconversion populates them. Adding them did **not**
bump `shared::WORLD_DATABASE_SCHEMA_VERSION`: nothing that already reads `waters` depends on their
presence, and the fallback exists specifically so a reconversion is not required.

---

### 14. Grass Definitions and Landscape Associations (`grass_types`, `landscape_texture_grasses`)

`grass_types` projects each effective `GRAS` record, including its canonical converted
model path (`MODL` through the existing mesh asset-path mapping). Grass models use the
same NIF-to-GLB converter as other meshes. `flags` below contains the `DATA` grass
flags rather than record header flags. `load_order` identifies the winning
override; `formid_map` retains owning-plugin identity independently.

The layout follows [xEdit's TES5 GRAS and LTEX definitions](https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.5/Core/wbDefinitionsTES5.pas)
and [CommonLibSSE-NG's TESGrass definition](https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/main/include/RE/T/TESGrass.h).
`DATA` is 32 bytes; its padding and unprojected fields stay in `records.data`.
The converter preserves authored values without clamping to vanilla ranges. A field
whose bytes are missing, or a non-finite float, becomes NULL. Missing, empty or unsafe
model paths also become NULL; the original subrecords remain available for diagnostics.
Path normalization does not verify that the referenced asset exists. Even vanilla
records can name an absent mesh, so a non-NULL `model_path` is not an availability
guarantee. Grass consumers must tolerate missing models, skip the unavailable
model with a useful diagnostic, and continue processing other grass types.

```sql
CREATE TABLE IF NOT EXISTS grass_types (
    id INTEGER PRIMARY KEY,            -- resolved GRAS FormID
    editor_id TEXT,
    model_path TEXT,                    -- canonical meshes/...glb from MODL
    density INTEGER,                    -- DATA+0: u8
    min_slope INTEGER,                  -- DATA+1: u8, degrees
    max_slope INTEGER,                  -- DATA+2: u8, degrees
    units_from_water INTEGER,           -- DATA+4: u16, distance from water level
    water_comparison INTEGER,           -- DATA+8: u32, enum below
    position_range REAL,                -- DATA+12: f32
    height_range REAL,                  -- DATA+16: f32
    color_range REAL,                   -- DATA+20: f32
    wave_period REAL,                   -- DATA+24: f32
    flags INTEGER,                     -- DATA+28: u8
    load_order INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS landscape_texture_grasses (
    ltex_id INTEGER NOT NULL,           -- resolved LTEX FormID
    gras_id INTEGER NOT NULL,           -- resolved GNAM GRAS FormID
    PRIMARY KEY (ltex_id, gras_id)
);
```

`water_comparison` values 0 through 7 mean Above At Least, Above At Most,
Below At Least, Below At Most, Either At Least, Either At Most,
Either At Most Above and Either At Most Below, respectively. `flags` bits 0,
1 and 2 mean Vertex Lighting, Uniform Scaling and Fit to Slope. These definitions
establish the authored contract, not Skyrim's exact placement equations.

Each non-null repeated `LTEX.GNAM` yields an association; duplicate pairs collapse.
The winning LTEX list replaces the earlier list in full. There is no foreign key on
the grass target: a deleted or unresolved grass can still be named by a texture,
and consumers resolve it against `grass_types`. Both projections refresh atomically
from a complete effective load-order export, removing stale grass and associations
on deletions or later overrides. Deleted GRAS records therefore do not reappear as
grass definitions. The subset export used by movement annotation preserves unrelated
grass rows; if it includes an LTEX, only that texture's association list is replaced.
Movement annotation continues to accept schema 4 through the current schema and
preserves the existing database version; it does not perform a full reconversion.

---

### Other Runtime and In-House Tables

These tables are written by `create_tables` / the export and had no DDL here before #269. Their
DDL below is copied from `crates/converter/src/esm/exporter.rs` and
`crates/converter/src/esm/inhouse*.rs`; `statics` is documented with `references` (§5) by #270.

```sql
CREATE TABLE IF NOT EXISTS npcs (               -- one row per NPC_
    id INTEGER PRIMARY KEY, editor_id TEXT,
    full_name TEXT,                             -- FULL
    race_id INTEGER,                            -- RNAM
    class_id INTEGER,                           -- CNAM
    flags INTEGER NOT NULL                      -- record header flags
);
CREATE INDEX IF NOT EXISTS idx_npcs_editor_id ON npcs(editor_id);

CREATE TABLE IF NOT EXISTS movement_types (     -- MOVT 0003580D (NPC_Default_MT) only, today
    id INTEGER PRIMARY KEY,
    editor_id TEXT NOT NULL,                    -- written as the literal 'NPC_Default_MT'
    left_walk REAL NOT NULL, left_run REAL NOT NULL,       -- SPED, Creation units per second
    right_walk REAL NOT NULL, right_run REAL NOT NULL,
    forward_walk REAL NOT NULL, forward_run REAL NOT NULL,
    back_walk REAL NOT NULL, back_run REAL NOT NULL,
    load_order INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS race_movement_links (  -- one row per RACE
    id INTEGER PRIMARY KEY,
    walk_movt_id INTEGER,                       -- WKMV
    run_movt_id INTEGER,                        -- RNMV
    load_order INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS movement_game_settings ( -- fMoveCharWalkBase and fJumpHeightMin only
    id INTEGER PRIMARY KEY, editor_id TEXT NOT NULL,
    value REAL NOT NULL, load_order INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS texture_sets (       -- one row per TXST; slot roles in the preamble
    id INTEGER PRIMARY KEY, editor_id TEXT,
    diffuse_path TEXT,                          -- TX00
    normal_path TEXT,                           -- TX01
    glow_path TEXT,                             -- TX03
    height_path TEXT,                           -- TX04
    environment_path TEXT,                      -- TX05
    mask_path TEXT,                             -- TX02
    specular_path TEXT,                         -- TX07
    detail_path TEXT                            -- TX06
);
CREATE TABLE IF NOT EXISTS landscape_textures ( -- one row per LTEX
    id INTEGER PRIMARY KEY, editor_id TEXT,
    texture_set_id INTEGER,                     -- TNAM -> TXST
    material_type INTEGER,                      -- MNAM: the material type's FormID (MATT), not a number
    friction REAL,                              -- HNAM byte 0
    restitution REAL                            -- HNAM byte 1
);
```

The in-house reader also writes these auxiliary tables (diagnostic and provenance data; the
engine does not read them):

```sql
CREATE TABLE IF NOT EXISTS inhouse_source_records (     -- original decompressed source bytes
    form_id INTEGER PRIMARY KEY, source_form_id INTEGER NOT NULL, load_order INTEGER NOT NULL,
    record_type TEXT NOT NULL, flags INTEGER NOT NULL, payload BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS inhouse_localized_fields (   -- every localized string field it met
    form_id INTEGER NOT NULL, field_index INTEGER NOT NULL,
    signature TEXT NOT NULL, field_name TEXT NOT NULL,
    string_table TEXT NOT NULL,                         -- strings, dlstrings or ilstrings
    string_id INTEGER NOT NULL, load_order INTEGER NOT NULL,
    resolved_text TEXT,
    status TEXT NOT NULL CHECK(status IN ('resolved','null_id','missing')),
    PRIMARY KEY(form_id, field_index)
);
CREATE TABLE IF NOT EXISTS inhouse_terrain_source_order (
    form_id INTEGER PRIMARY KEY, source_record_offset INTEGER NOT NULL
);
CREATE TABLE inhouse_terrain_layers (                   -- recreated with every complete export
    form_id INTEGER PRIMARY KEY, cell_id INTEGER NOT NULL,
    load_order INTEGER NOT NULL, layer_data BLOB NOT NULL
);
CREATE TABLE inhouse_unresolved_terrain_textures (      -- LAND layer textures that do not resolve
    form_id INTEGER NOT NULL, texture_form_id INTEGER NOT NULL,
    PRIMARY KEY(form_id, texture_form_id)
) WITHOUT ROWID;
```

---

### 15. Interior Room Graph (`room_bounds`, `room_portals`, `room_links`, `reference_rooms`, `room_graph_stats`)

Skyrim marks the rooms of an interior with `RoomMarker` references (base `0x1F`) and the
doorways between them with `PortalMarker` references (base `0x20`). The converter decodes both
and records which room boxes each placed object belongs to. The tables hold facts only: which
objects to draw is the engine's policy (design: `docs/research/interior-room-graph-converter.md`,
upstream issue #188). Only interior cells and only box rooms (`XPRM` shape 1) are written;
exterior markers are skipped silently, and rooms of other shapes, and rooms or portals with a non-finite size or transform, are skipped and counted in the conversion report.
All FormIDs are in load-order numbering, like `references.id`.
The two marker base forms are the ones `Skyrim.esm` defines (always load-order slot 0, so the numbers do not change with the load order). A plugin that places rooms with a marker base of its own is not recognised and produces no rows; the conversion report does not count that case.

```sql
CREATE TABLE IF NOT EXISTS room_bounds (       -- one row per interior RoomMarker with a box XPRM
    ref_id INTEGER PRIMARY KEY,                -- references.id of the marker
    cell_id INTEGER NOT NULL,
    pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,   -- Creation units
    rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,   -- radians, as references
    half_x REAL NOT NULL, half_y REAL NOT NULL, half_z REAL NOT NULL, -- XPRM[0..12]
    lighting_template_id INTEGER,              -- LNAM, NULL when absent
    image_space_id INTEGER,                    -- INAM, NULL when absent
    flags INTEGER NOT NULL DEFAULT 0           -- XRMR flags byte | (XRMR u16 << 8), kept raw
);
CREATE INDEX IF NOT EXISTS idx_room_bounds_cell ON room_bounds(cell_id);

CREATE TABLE IF NOT EXISTS room_portals (      -- one row per XPOD pair of an interior PortalMarker with an XPRM
    ref_id INTEGER NOT NULL,                   -- references.id of the marker; NOT unique, see pair_index
    pair_index INTEGER NOT NULL DEFAULT 0,     -- position of the (origin, destination) pair in XPOD
    cell_id INTEGER NOT NULL,
    pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,
    rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,
    half_x REAL NOT NULL, half_z REAL NOT NULL, -- doorway half-width and half-height
    room_a INTEGER,                            -- the pair's origin; NULL when 0, absent or not a room of this cell
    room_b INTEGER,                            -- the pair's destination; same
    PRIMARY KEY (ref_id, pair_index)
);
CREATE INDEX IF NOT EXISTS idx_room_portals_cell ON room_portals(cell_id);

CREATE TABLE IF NOT EXISTS room_links (        -- XLRM: rooms seen together without a portal
    room_ref INTEGER NOT NULL,
    linked_ref INTEGER NOT NULL,
    PRIMARY KEY (room_ref, linked_ref)
);

CREATE TABLE IF NOT EXISTS reference_rooms (   -- room membership, interiors with rooms only
    reference_id INTEGER NOT NULL,             -- references.id
    room_ref INTEGER NOT NULL,                 -- room_bounds.ref_id
    contained INTEGER NOT NULL,                -- 1: pivot inside the box; 0: only the bounds overlap it
    PRIMARY KEY (reference_id, room_ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_reference_rooms_room ON reference_rooms(room_ref);

CREATE TABLE IF NOT EXISTS room_graph_stats (  -- conversion report counters, one row per name
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
```

Columns and rules:

- **`room_bounds`**: the box is centred on the marker's position and oriented by its rotation
  (the same Euler convention as any reference, `shared::coordinates`). `half_*` are the `XPRM`
  half-extents in the room's own frame. `lighting_template_id` (`LNAM`) and `image_space_id`
  (`INAM`) are decoded for the lighting work in #129-#135; nothing in the engine uses them yet.
- **`room_portals`**: a portal's doorway is the marker's local X-Z rectangle (normal along local
  +Y); `half_x`/`half_z` are its half-width and half-height. A portal marker without `XPRM`
  has no size and gets no row. `XPOD` is an array of 8-byte (origin, destination) FormID pairs
  (xEdit 4.1.5f defs, `Core/wbDefinitionsTES5.pas`: `wbArray(XPOD, 'Portal Data',
  wbStruct('References', [wbFormIDCk('Origin', [REFR, NULL]), wbFormIDCk('Destination',
  [REFR, NULL])]))`), so a portal gets **one row per pair**, numbered by `pair_index` from 0
  in stored order; the geometry columns repeat on every row. A portal whose `XPOD` is absent,
  empty or shorter than one pair gets one row with `pair_index` 0 and both sides NULL. Bytes
  after the last whole pair are ignored. Every `XPOD` in the official masters is one pair, so
  vanilla output has `pair_index` 0 only. **Readers must not assume `ref_id` is unique**: join
  or group on `(ref_id, pair_index)`, or on `ref_id` and expect several rows. The key changed
  from `ref_id` alone, and `create_tables` migrates an older-shaped `room_portals` (no
  `pair_index`) in place, inside one savepoint: every row is kept with `pair_index` 0 until the
  next full export rebuilds the table.
  In the plugin remap `XPOD` is an array of FormIDs: every four bytes are remapped, and a
  length that is not a multiple of four is dropped with a warning, like any other FormID array.
- **`room_links`**: one row per `XLRM` as stored on the room, with no mirror rows. A link joins
  rooms that may have no portal between them but must be seen together: if one is visible, so is
  the other. The target is stored as written, even if it is not a `room_bounds` row.
- **`reference_rooms`**: written for every placed object (`REFR`, `ACHR`, `ACRE`, `PGRE`,
  `PMIS`) in an interior cell that has at least one room, except the room and portal markers
  themselves. `contained = 1` rows come from the ESM export: the object's pivot (its position)
  lies inside the box, edges included. `contained = 0` rows are added after model bounds are
  known (the integration stage, once `statics.bounds_*` come from the converted GLBs): the
  pivot is outside, but the object's bounds, placed with the reference's scale, rotation and
  position, overlap the box. `statics.bounds_*` are glTF-axis (Y-up) numbers in Creation units,
  with no metre scale. The test is conservative (the placed bounds' corners, taken into the room
  frame, as an axis-aligned box against the half-extents), so it can report an overlap that does
  not exist, which only makes the engine draw more. A `contained = 1` row is never also written
  as `contained = 0`.
  Overlap rows need a base with bounds in `statics`, which holds every base type with a world
  model (`STAT`, `MSTT`, `FURN`, `TREE`, `FLOR`, `CONT`, `DOOR`, `ACTI`, `TACT`, `APPA`, `ADDN`,
  `ARTO`, placed items and armour, and `LIGH` with a model; see §5). Placed references of other base types get pivot rows only: actors
  (`NPC_`, which move and stay ordinary entities), lights without a model (their reach is a
  lighting question, not geometry), decals (`TXST`), and bases with no geometry (`IDLM`, `SOUN`,
  `ASPC`). In the all-plugins conversion these were 22,251 of 364,262 pivot rows (measured before `TACT`, `APPA`, `ADDN` and `ARTO` joined `statics`).
- **`room_graph_stats`**: the counters the conversion report shows in `integration-report.json`
  under `room_graph`: `rooms`, `portals` (portal markers written, not rows), `links`,
  `portals_with_missing_side` (`room_portals` rows with a NULL side, so a portal with several
  pairs can add several), `portals_with_extra_pairs` (markers whose `XPOD` holds more than one
  pair), `portals_with_trailing_xpod_bytes` (markers whose `XPOD` length is not a multiple of 8),
  `non_box_rooms_skipped`, `portals_without_size_skipped`, `invalid_markers_skipped` (rooms and portals with a non-finite size or transform), `objects_in_no_room` and
  `objects_in_several_rooms` (both by pivot), and `overlap_rows`.

The tables are derived data: a full load-order export clears and rebuilds them from the winning
records on every conversion, like the grass tables, and the subset export used by movement
annotation leaves them alone. `create_tables` adds them to an older reused database.

**These tables are optional and did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`.** No engine code reads
them yet; a reader must probe for them first, as the engine does for `lights` and the additive
`waters` columns, so a database without them behaves as before.

| Case | Converter writes | Engine |
|---|---|---|
| Interior without room markers | no rows | draw everything, as today |
| Old database, tables absent | n/a | feature off, no error |
| Portal side 0, missing or not a room | `room_a`/`room_b` NULL | ignore that edge |
| Object in no room / several | no `contained = 1` row / several | always drawn |
| Camera outside every room | n/a | draw everything |
| Non-box room shape | skipped, counted | its objects are in no room, so drawn |
| Exterior markers | not written | out of scope (#106) |

---

### 16. Reference Emittance Sources (`emittance`)

A NIF shader with `SLSF1_External_Emittance` (bit 29) takes its emission colour from outside the
model: from the light or region the placing reference names in `XEMI`
(`references.emittance_id`). Skyrim keeps building windows dark by day this way. The Sleeping Giant
Inn's reference in Riverwood (`0002D763`) names `FXLightRegionInvertWindowWhiterun` (`0008277B`),
whose one weather `FXWthrInvertWindowsWhiterun` (`0008277A`) has a black Effect Lighting colour by
day and an amber one at night (#242).

The complete load-order export rebuilds the table, writing one row per distinct `XEMI` target it can
resolve, as 8-bit sRGB at sunrise, day, sunset and night:

- `LIGH`: the `DATA` colour (bytes 8-10), the same at every time of day.
- `REGN`: the `RDWT` weather with the highest chance (the first on a tie), and that weather's
  `NAM0` Effect Lighting colour, the tenth colour type. `NAM0` holds 16 bytes per colour type
  (sunrise, day, sunset and night, each RGB and an unused byte); the official plugins store 13,
  14 or 17 types. A region changes weather with its climate, which the runtime does not model, so
  its likeliest weather stands in.

Effect Lighting rather than Sunlight: the inn's window shader has a white tint and a neutral grey
glow map, so the warm colour of Skyrim's lit windows at night can only come from the source. Both
colours are black by day for the window regions.

A target that is missing, deleted, not a `LIGH` or `REGN`, or whose weather has no Effect Lighting
colour gets no row and a warning; the runtime then keeps the model's own emission. The movement
annotator's subset export does not touch this table.

Measured on Skyrim SE with its official plugins: 9,655 references carry `XEMI`, naming 211
sources. 166 lights and 31 regions resolve; 14 regions list no weather at all (the largest,
`RiverwoodMountainsSnow` `0002E2CC`, is named by 479 references), and what Skyrim lends from such
a region is unverified, so their 651 references keep the model's emission.

```sql
CREATE TABLE IF NOT EXISTS emittance (
    id INTEGER PRIMARY KEY,        -- XEMI target: a LIGH or REGN FormID
    source_type TEXT NOT NULL,     -- 'LIGH' or 'REGN'
    weather_id INTEGER,            -- REGN: the WTHR its colours come from
    sunrise_r INTEGER NOT NULL, sunrise_g INTEGER NOT NULL, sunrise_b INTEGER NOT NULL,
    day_r INTEGER NOT NULL, day_g INTEGER NOT NULL, day_b INTEGER NOT NULL,
    sunset_r INTEGER NOT NULL, sunset_g INTEGER NOT NULL, sunset_b INTEGER NOT NULL,
    night_r INTEGER NOT NULL, night_g INTEGER NOT NULL, night_b INTEGER NOT NULL
);
```

The engine reads the day columns, its one time of day; see
[`color-pipeline.md`](../engine/color-pipeline.md#external-emittance-242).

**The table and `references.emittance_id` are optional and did not bump
`shared::WORLD_DATABASE_SCHEMA_VERSION`**, for the same reason as the room tables: they only add a
table and a column, and the engine probes for both. `create_tables` adds `emittance_id` to an older
reused `references` table. A database without them loads with every reference unlit by `XEMI`, so
`External_Emittance` surfaces keep the model's own emission.

---

### 17. Load Door Links (`door_links`)

A load door is a `REFR` of a `DOOR` base record that carries an `XTEL` subrecord. `XTEL` is 32
bytes in Skyrim SE: the destination door reference (FormID, bytes 0..4), the arrival position
(three `f32`, bytes 4..16), the arrival rotation (three `f32` radians, bytes 16..28) and a `u32`
flags word (bytes 28..32, `1` = no alarm). The arrival frame is the one the player is placed in
on this side's destination, not the position of the destination door. Both record readers keep
the 32 bytes unchanged in `records.data` and remap the destination FormID with the load order
(`form_id_layout` in `crates/converter/src/esm/mod.rs`; the in-house schema describes the
subrecord as `teleport` in `crates/converter/src/records/schema.json`). The `door_links` table is
projected from those canonical `records` and `references` rows, not from either reader, so the
legacy and in-house frontends publish identical rows.

```sql
CREATE TABLE IF NOT EXISTS door_links (        -- one row per load door that can be followed
    ref_id INTEGER PRIMARY KEY,                -- references.id of the door
    destination_ref_id INTEGER NOT NULL,       -- XTEL[0..4], after FormID remapping
    pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,   -- XTEL[4..16], Creation units
    rot_x REAL NOT NULL, rot_y REAL NOT NULL, rot_z REAL NOT NULL,   -- XTEL[16..28], radians
    destination_cell_id INTEGER NOT NULL,      -- cells.id of the cell holding the destination door
    destination_worldspace_id INTEGER,         -- that cell's worldspace, NULL for an interior
    destination_name TEXT                      -- interior cell FULL (else EDID), or the worldspace EDID
);
```

A door is skipped, with no row, when it has no `XTEL`, when the `XTEL` is not exactly 32 bytes,
when the destination is `0`, when any arrival value is not finite, or when the destination door
is not a placed `DOOR` reference in a stored cell (an exterior cell needs its grid). The winning
record decides: an override that removes `XTEL` removes the link. The `XTEL` flags word is not
projected; it stays in `records.data`.

A full load-order export clears and rebuilds the table on every conversion, like the grass and
room tables. The movement-annotation subset export leaves it alone, and a diagnostic archive
(which defers every runtime view) leaves it empty. `create_tables` adds the table to an older
reused database, and adds `destination_name` to a table that predates it.
`door-links-annotate <skyrim_world.db>` rebuilds it in one transaction from a converted
database's own winning records, with no plugin reparsed, so an existing package gains doors
without a full conversion. It takes the asset lock exclusively (close the engine first), accepts
schemas 4 through the current one, and leaves meshes, terrain, `schema_info`, reader identity and
the other tables unchanged. A corrupt `records` row fails the rebuild and keeps the old table.

**This table is optional and did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`**, for the same
reason as the room tables: it only adds a table, and the engine probes for it. The engine reads
it through a `LEFT JOIN` on `references`, only when the table exists with every column above.

| Case | Converter writes | Engine |
|---|---|---|
| Old database, table absent | n/a | logs once, doors stay scenery |
| Table of another shape (missing column) | n/a | warns, doors stay scenery |
| Door without `XTEL`, or with a malformed one | no row | scenery |
| Destination cell has no resolvable record | no row | scenery |
| Row with a NULL arrival value | n/a | not a load door |

---

### 18. Actor Model Catalog (`actor_models`, `actor_model_parts`)

Every non-deleted `NPC_` base in the merged record map gets one `actor_models` row describing its
race, sex, effective skin armor, body/head resolution and head path, plus one `actor_model_parts`
row per biped slot the effective skin resolves. The catalog is resolved once, after the record
loop, from the merged record map; the tables are projected from canonical records, not from either
reader's raw `records.data`, so the legacy and in-house frontends publish identical rows.

```sql
CREATE TABLE IF NOT EXISTS actor_models (
    id INTEGER PRIMARY KEY,          -- NPC_ FormID
    race_id INTEGER NOT NULL,
    sex TEXT NOT NULL,               -- 'male' | 'female'
    skin_armo_id INTEGER,            -- effective NPC WNAM override, else effective RACE/WNAM
    head_model TEXT,                 -- FaceGen or default head NIF path
    head_source TEXT,                -- 'facegen' | 'default'
    resolution TEXT NOT NULL,        -- 'body', 'body_head', 'unresolved'
    skin_tone_r REAL,                -- NPC_/QNAM, 0..1, encoded scale
    skin_tone_g REAL,
    skin_tone_b REAL,
    weight REAL                      -- NPC_/NAM7, 0..100
);
CREATE TABLE IF NOT EXISTS actor_model_parts (
    npc_id INTEGER NOT NULL,
    slot TEXT NOT NULL,              -- 'body' | 'hands' | 'feet'
    model_path TEXT NOT NULL,
    armature_race_id INTEGER,        -- ARMA/RNAM: the NPC's race or DefaultRace (25)
    skin_texture_set_id INTEGER,     -- ARMA/NAM0 (male) or NAM1 (female), a TXST
    PRIMARY KEY (npc_id, slot)
);
```

**Traits.** When `NPC_/ACBS` `u16` at offset 18 has bit 0 set, `TPLT` is followed while each
target also has that bit set. The final Traits root supplies race (`RNAM`), sex (`ACBS` bit 0 at
offset 0), and skin override (`WNAM`). A Traits-enabled NPC's own valid `WNAM` is ignored. A
visited set detects cycles; there is no fixed depth limit. If the chain cannot resolve, the
original NPC still gets one unresolved row with `race_id = 0`, placeholder `sex = 'male'`, no
skin, parts, or head. Diagnostics identify missing, deleted, wrong-type, cyclic, or `LVLN`
targets. The detailed source and evidence boundaries are in
[`actor-catalog-inheritance-policy.md`](../../research/actor-catalog-inheritance-policy.md).

**Armature skin texture set.** The game draws a part's `skin_tint` shapes with the armature's skin
`TXST` for the NPC's sex (`NAM0` male, `NAM1` female), not with the part NIF's own texture set:
retail `femalehands_1.nif` has no `_s`, while `NakedHands` (`00000D6C`) gives female hands
`SkinHandFemale_1` (`0003EDE9`), whose `TX07` is `FemaleHands_1_S.dds`. `skin_texture_set_id`
stores that FormID (`NULL` when the armature has none for the sex; the other sex's set is not
used), and `armature_race_id` the armature's own `RNAM`, so `25` marks a part that came from the
shared `DefaultRace` armature. The converter composes the default humanoid body from the
`DefaultRace` sets: per sex and slot, the set most parts with `armature_race_id = 25` use, applied
in NIF slot order (`TX00`→0, `TX01`→1, `TX03`→2, `TX04`→3, `TX05`→4, `TX02`→5, `TX06`→6,
`TX07`→7; slots the set leaves empty keep the NIF's path) when the armature's model file is the
part being composed. Both columns arrived after the first catalog export without a schema bump;
`create_tables` adds them to an older reused `actor_model_parts`, and a database without them
composes the body from the NIFs' own textures.

**Skin tone.** `NPC_/QNAM` is three little-endian `f32` (red, green, blue), stored as written:
0..1 on the encoded scale (retail values are whole steps of 1/255; Alvor `00013475` is
198,176,168). The game passes it to the `skin_tint` shaders of the body, hands and feet as their
tint colour. A base without `QNAM`, or with a short or non-finite one, stores `NULL` in all three. Since
QNAM is not a confirmed Traits inheritance field, a Traits-enabled source NPC stores `NULL` even
when it or its resolved root carries QNAM; this conservative catalog policy avoids inventing root
inheritance.
The columns arrived after the first catalog export without a schema bump: `create_tables` adds
them to an older reused `actor_models`, and the engine probes for them, so a database without them
still loads and its actors stay untinted.

**Body weight.** `NPC_/NAM7` is one little-endian `f32` in 0..100: the weight the game blends
between the thin (`_0`) and heavy (`_1`) body meshes (0 is thinnest, 100 is heaviest). The column
is nullable; a base with no `NAM7`, or with a short, non-finite or out-of-range one, stores `NULL`
and the engine draws the heavy body. The weight comes only from the base's own record: `TPLT`
template inheritance is not resolved for it, matching `QNAM` and `RNAM`. The column arrived after
the first catalog export without a schema bump (`WORLD_DATABASE_SCHEMA_VERSION` stays 9):
`create_tables` adds it to an older reused `actor_models`, and the engine probes for it, so a
database without it still loads.

**Sex.** `NPC_/ACBS` bit 0 (`1` = female). A base with no `ACBS` defaults to male and records a
`missing_acbs` diagnostic.

Without Traits, the NPC supplies its own race and sex. A missing effective-root `ACBS` defaults
to male and records `missing_acbs`.

**Body resolver chain.** Effective Traits-root `NPC_/RNAM` -> the `RACE` record -> effective skin
armor -> every `ARMO/MODL` armature in authored order -> each `ARMA`:

1. Skin is the effective Traits-root NPC's `WNAM` override when it names a live `ARMO`; otherwise
   it falls back to the effective race's `WNAM`. An invalid explicit override records its
   diagnostic and uses that race fallback. This is a conservative catalog policy, not proof of
   Skyrim engine fallback behavior.
2. An armature is compatible when its primary `ARMA/RNAM` equals the effective race, or a
   resolvable additional-race `ARMA/MODL` names that race. These matches share one tier and keep
   authored `ARMO/MODL` order.
3. Compatible armatures are considered before armatures whose primary `RNAM` is `25`
   (`DefaultRace`), whatever order `ARMO/MODL` lists them in. If neither tier matches, no part is
   written and `no_matching_arma` is recorded (the resolver never picks the first `ARMA`).
4. `MOD2` is the male path. Female NPCs use `MOD3`, falling back to `MOD2` when `MOD3` is absent
   or empty.
5. The slot is the armature's biped bit: bit 2 = body, bit 3 = hands, bit 7 = feet (slot number
   minus 30). The resolver reads `ARMA/BOD2` when present and falls back to `ARMA/BODT`, which is
   the field retail Skyrim's `ARMA` records actually carry (the same `u32` slot word at offset 0;
   `ARMO` is the record with `BOD2`). Outside the creature whole-skin exception below, an armature with neither recognized bit is rejected loudly
   with an `unrecognized_slot` diagnostic; a slot is never inferred from the filename.
6. Paths are deduplicated case-insensitively, and a slot already filled keeps the higher-priority
   armature.

**Creature whole skins.** A non-FaceGen race with an authored project outside the shared
character project family may store its whole skin in biped slot 30 (`BODT`/`BOD2` bit 0).
When no ordinary body resolved, exactly one compatible head-slot model becomes the body.
An ordinary body wins regardless of armature order; multiple distinct candidates remain
unresolved with `ambiguous_creature_body`. Missing race flags or project metadata do not
establish this exception. This preserves humanoid and child body selection.

**Authored idle identity.** Every export creates the typed projection below with the other
runtime tables. Full runtime exports replace its rows in the record transaction; archive-only
record bundles keep the same empty table, like the other deferred runtime views. Metadata
rebuilds therefore invalidate retained idle bindings when their race project or skeleton
changes. The runtime accepts a catalog binding only when this table has a matching row. Older
databases need no migration to render actors; they have no authenticated generic idle library
until normal conversion produces one.

```sql
CREATE TABLE IF NOT EXISTS race_animation_sources (
    race_id INTEGER NOT NULL,
    sex TEXT NOT NULL,
    skeleton_nif TEXT,
    project_hkx TEXT,
    PRIMARY KEY (race_id, sex)
);
```

The converter follows project/character HKX metadata and publishes pose clips plus
`meshes/actor-idles/catalog.json`. Runtime lookup requires race, sex, full body path, project,
and race skeleton identity to agree. See [animal idle investigation](../../research/animal-idle-471.md)
for discovery, supported playback, provenance, and validation limits.

**Head.** The `RACE/DATA` `facegen_head` flag (bit 1 of the flags word at offset 32) selects the
FaceGen geometry `meshes/actors/character/facegendata/facegeom/<plugin>/<formid>.nif`, lowercased,
with `<plugin>` and plugin-local FormID from the effective Traits root's defining-owner
`formid_map` identity (`plugin_name`/`internal_id`). The local ID uses eight-digit lower-case hex
without the load-order slot prefix, so the runtime mod index never appears in the file name
(`head_source = 'facegen'`). Using the Traits root is a conservative catalog policy, not proof of
engine copy-body or FaceGen ownership behavior. Otherwise, when the
resolved body is humanoid (a part path contains `malebody` or `femalebody`), the race/sex default
head is used (`head_source = 'default'`):
`meshes/actors/character/character assets/` plus `malehead.nif`/`femalehead.nif`, with the
`maleheadargonian.nif`/`maleheadkhajiit.nif` variants when the race EditorID names Argonian or
Khajiit. A creature body never gets a humanoid default head.

**Path bases.** `head_model` is archive-root-relative: it starts with `meshes/` and uses forward
slashes. `actor_model_parts.model_path` preserves the authored `ARMA/MOD2` or `ARMA/MOD3`
string, including its case and separators. Retail part paths are relative to `meshes/` and
omit that prefix; consumers must normalize these authored paths before asset lookup.

This slice computes the head *path string* only; it does not compose a head NIF and does not treat
`HDPT/PNAM` as the head mesh. FaceGen presence is the race flag, not a per-NPC file probe: the ESM
export path has no asset index, so PR-6 validates the path before loading.

**Reason codes.** A base that fails to resolve records the reason it was rejected: `no_rnam`,
`no_race_record`, `race_no_wnam`, `no_armo_record`, `armo_no_modl`, `no_matching_arma`. Traits
resolution and skin overrides add nine codes: `missing_traits_template`, `deleted_traits_template`,
`wrong_type_traits_template`, `traits_template_cycle`, `leveled_traits_template`,
`zero_npc_skin_override`, `missing_npc_skin_override`, `deleted_npc_skin_override`, and
`wrong_type_npc_skin_override`. A creature base with more than one slot-30 whole-skin candidate
and no normal body records `ambiguous_creature_body`. A base that reaches head resolution but has no head path also
records `no_facegen_and_no_default`; early returns for unresolved Traits, missing race links, or
missing race records do not. The reasons are not mutually exclusive, and a report that counts
them counts a base once per reason.
`unrecognized_slot` and `missing_acbs` are recorded wherever they occur. Diagnostics are printed
by the exporter and returned by `converter::actor_models::resolve_actor_models`; they are not
stored in the database.

**Resolution.** `body_head` when at least one part and a head path resolve, `body` when parts
resolve without a head, and `unresolved` otherwise. A base with no effective-root `RNAM` is still
written with `race_id = 0` and `resolution = 'unresolved'`. Any unresolved Traits chain also
produces one row for the original NPC with `race_id = 0`, placeholder `sex = 'male'`, and no parts
or head, so it is recorded rather than dropped.

**Ownership.** A complete load-order export drops and rebuilds both tables in the record
transaction. A subset export, such as movement annotation, leaves them alone: a partial record map
cannot resolve an `NPC_` whose race or skin it did not receive, so it must not overwrite a complete
row with an unresolved one. A diagnostic archive (which defers every runtime view) leaves both
tables empty. `NPC_` bases are never written into `statics`.

**These tables are optional and do not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`** (still 9):
they only add tables, and consumers must check that they exist before reading them. No mandatory
join is added to `REFERENCE_JOIN`; a database without them still opens and renders statics.
### 19. Projection Omissions (`projection_omission_stats`, `projection_omission_samples`)

The light and emittance projections write no row for a record whose data cannot supply one. Each
skip is counted under a stable category, so the conversion report can account for it instead of
a terminal message alone (#297). The categories come from `insert_light` and
`insert_emittance_sources` in `crates/converter/src/esm/exporter.rs`; the code is
`crates/converter/src/esm/projection_omissions.rs`.

```sql
CREATE TABLE IF NOT EXISTS projection_omission_stats (   -- one row per category with omissions
    category TEXT PRIMARY KEY,     -- stable name, see the table below
    severity TEXT NOT NULL,        -- 'data_loss' or 'fallback'
    count INTEGER NOT NULL         -- exact number of records left out
);
CREATE TABLE IF NOT EXISTS projection_omission_samples ( -- the lowest FormIDs, at most 8 per category
    category TEXT NOT NULL,
    form_id INTEGER NOT NULL,      -- the LIGH, or the XEMI target
    reason TEXT NOT NULL,          -- human-readable detail, e.g. the DATA length
    PRIMARY KEY (category, form_id)
) WITHOUT ROWID;
```

| Category | Record left out | Severity |
|---|---|---|
| `light_missing_data` | `LIGH` without `DATA`: no `lights` row | data loss |
| `light_short_data` | `LIGH` whose `DATA` is shorter than the 20 bytes the row needs: no `lights` row | data loss |
| `emittance_target_absent` | `XEMI` target not in the load order, or deleted: no `emittance` row | fallback |
| `emittance_light_unusable_data` | `XEMI` target `LIGH` without a usable `DATA`: no `emittance` row | fallback |
| `emittance_region_no_weather` | `XEMI` target `REGN` whose `RDWT` names no weather: no `emittance` row | fallback |
| `emittance_weather_no_lighting` | `XEMI` target `REGN` whose weather has no Effect Lighting colours: no `emittance` row | fallback |
| `emittance_target_wrong_type` | `XEMI` target that is neither a `LIGH` nor a `REGN`: no `emittance` row | fallback |

**Severity.** A light without a `lights` row loses a light the runtime would draw, so a
`data_loss` category adds one line to the conversion's `warnings` and makes the output
incomplete. A reference whose emittance target does not resolve keeps the model's own emission
(section 16), the documented fallback, so a `fallback` category adds one line to `notices` and the
output stays complete. A severity the reader does not know is read as `data_loss`, so a category
added by a newer converter cannot pass as success.

A full load-order export clears and rewrites both tables inside its transaction, so a re-export
replaces the counts instead of adding to them, and a repaired load order leaves the tables empty.
The movement-annotation subset export leaves them alone, and a diagnostic archive leaves them
empty. `finalize_world_database` reads them into `projection_omissions` in
`integration-report.json`, so a metadata rebuild or a reused database reports the same
omissions. A database without the tables reports none.

**These tables are optional and did not bump `shared::WORLD_DATABASE_SCHEMA_VERSION`**: they only
add tables, `create_tables` adds them to an older reused database, and the engine does not read
them.

### 20. Authored local-light projections (#264/#364)

Optional additions preserve compatibility with schema 3–9 packages. `lights.fade_status` and
`references.radius_status` are nullable INTEGER columns: NULL means an older projection, 0 absent,
1 valid (including zero; signed XRDS is legal), 2 malformed/non-finite (negative FNAM unsupported).
For a valid LIGH reference the effective radius is base DATA radius plus raw XRDS, not replacement.
Invalid inputs retain their original bytes in the existing canonical record archive.

`reference_light_data(reference_id INTEGER PRIMARY KEY, data_size INTEGER NOT NULL, fov_offset REAL,
fade_offset REAL, end_distance_cap REAL, shadow_depth_bias REAL)` projects REFR XLIG without
evaluating it. Missing XLIG has no row. Only 16/20-byte layouts yield floats; non-finite values are
NULL. A complete export recreates the table; subset export removes/replaces rows for each exported
reference. No FormIDs occur within XLIG, so remapping is unnecessary.

See [point-lights.md](../engine/point-lights.md) for pinned layout sources, the bounded backend
transform, fallback dispositions and unresolved native equations. Reconversion into a new pack is
required for new columns; registered packs remain immutable.
