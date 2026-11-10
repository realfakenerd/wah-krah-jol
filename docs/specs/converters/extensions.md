# Dataset capabilities

Dataset capabilities describe optional, versioned payloads alongside the upstream core. The core
is the ordinary converter's tables, columns, files, glTF data and manifest fields. An extension
owns whole additional tables, indexes, files or glTF extension names. It must not change core
rows, add columns to core tables, or add keys inside a core glTF extension. Promoting payloads
into core requires an ordinary core change and any applicable schema version change.

This contract provides the generic capability mechanism and compatibility for legacy door-link
declarations. It registers no production payloads and adds no engine feature beyond that
compatibility. Core-only output has no `extensions` key. Existing core datasets need no
reconversion, and this contract does not bump any core schema version.

The implementation is `shared::extensions` for declarations, status and hashing,
`converter::extensions` for descriptors and publication, and `engine::extensions` for runtime
capability state and ordinary-door compatibility.

## Declarations and loading

`shared::extensions::{ExtensionEntry, ExtensionOwns}` are format-independent types implementing
Serde serialization and deserialization. They do not depend on a particular producer, payload
reader or enclosing manifest format. A JSON document can embed an optional `extensions` array;
an absent array loads as empty. The base `conversion-manifest.json` omits the array when empty.
Existing core fields retain their meanings.

```json
{
  "extensions": [
    {
      "id": "example.payload",
      "version": 1,
      "owns": {
        "tables": ["example_payload"],
        "indexes": [],
        "files": ["example/payload/"],
        "gltf_extensions": []
      },
      "sha256": "<64 hex digits>",
      "built_from": "<current core build identity>",
      "stats": { "example_payload": 2 }
    }
  ]
}
```

| Field | Contract |
|---|---|
| `id` | Stable dotted name; never reused for different data. |
| `version` | Payload contract version, independent of the core schema. |
| `owns` | Sorted lists of table names, index names, relative file paths and glTF extension names. Ownership must be disjoint between extensions. SQL names and file ownership compare ASCII case-insensitively on every platform, including nested directory prefixes. glTF names are case-sensitive. Declared spelling remains part of the content hash. |
| `sha256` | `mudcrab-ext-hash-v1` digest of the owned content. |
| `built_from` | Core build identity from which the payload was derived. Base dataset loading obtains the current identity from `lod_build.build_identity`; other manifest loaders supply it explicitly. |
| `stats` | Optional map of unsigned 64-bit integer counts; omission means an empty map. Table-producing descriptors emit row counts keyed by table name, as in `example_payload: 2` above. Other meanings belong to the producer. |

`DataExtensions::from_manifest_json(text, core_identity)` reads the document's `extensions`
array and ignores unrelated document fields. Invalid JSON or a non-object document is an error.
An absent or `null` array means no declarations; another non-array value records a parse issue
and supplies no entries. Unrelated manifest fields are skipped without building a JSON value tree.
Array entries are decoded independently: `null`, missing required
fields, floating-point or otherwise invalid statistics, unknown fields inside `owns`, and
invalid names reject only the affected entry. Missing `stats` is valid. Every occurrence of a
duplicate ID is rejected, leaving unrelated valid entries available.

`issues()` exposes rejected declarations and their reasons, retaining a string ID when one can
be recovered. `is_declared(id)` includes those identifiable rejected entries as well as accepted
entries. Readers must use it when deciding whether a declaration gates access: a malformed
reader-owned entry must not become an undeclared core fallback. Converter manifest loading uses
the same per-entry parsing and logs rejected optional declarations.

An overlay document such as `overlay.json` can embed the same `extensions` array. Its loader
passes the document text and the actual current base dataset identity to this API; it must not
use an extension's own `built_from` claim as the current identity. This API performs no file or
overlay discovery. `DataExtensions::load(root)` remains a base-layout convenience for readers
that explicitly need all declarations: it reads `conversion-manifest.json` and consults the
database only when an accepted entry has a nonempty `built_from`. The engine loader instead
filters against its own known readers before deciding whether any identity lookup is needed.

`get` and `entries` expose declarations. `status(id, supported_versions)` returns `Missing`
when the ID is absent, `UnsupportedVersion` when its version is outside the reader's range,
`Stale` when its `built_from` differs from a recorded current identity, or `Present` otherwise.
Version support is checked before staleness. Without a recorded current identity, status does
not classify a declaration as stale. A rejected entry has no accepted declaration and therefore
reports `Missing`; its parse issue and `is_declared` distinguish it from absence. Status does
not recompute the payload hash or certify that a payload reader can open the data. Unknown IDs
remain available to their owners.

The recorded base freshness anchor is `lod_build.build_identity`. An absent identity table or
no row means no recorded identity; schema, value-type and query failures are errors when an
identity lookup is required. Ordinary core datasets, including `--no-lod` output, still load
without this anchor under the existing core validation rules. New capability publication
requires a nonempty identity and fails with an explicit freshness-anchor error when it is
missing. Legacy compatibility cannot detect staleness without a recorded anchor; it does not
prove that the payload matches the current core.

## Owned file paths

Every input file path must be canonical and relative to the payload root, using `/` separators.
Reject absolute paths, any colon (including a drive prefix), backslashes, control characters,
and empty, `.` or `..` segments, including segments ending in a dot or space. A single trailing `/` is allowed for a directory ownership
entry; it is not an empty interior segment. Reject noncanonical spellings rather than silently
normalizing them before validation, overlap detection or hashing. Extension IDs and all declared
table, index, file and glTF names must also be nonempty and contain no control characters.
The shared declaration check (`ExtensionEntry::validate_names`) enforces these file path rules, so a reader
rejects a declaration the converter would refuse to hash; it is reported as an issue, never as present.

An exact file entry must exist. A directory entry owns every file below that directory; an
absent or empty directory contributes no files. Directory ownership overlaps any owned file or
subdirectory below that prefix. The shared ownership checker applies ASCII case-insensitive
comparison to SQL names and file paths on every platform, including nested file prefixes.
For example, `example/payload/` conflicts with `EXAMPLE/Payload/data.bin` even though the
declarations are different strings. This comparison does not normalize stored spelling or
change the host filesystem's path lookup rules. glTF names compare case-sensitively.

## Converter API and publication

`ExtensionSpec` describes an extension's ID, version and ownership. The public `specs()` and
`build_entries(output)` APIs remain available, but the built-in descriptor list is empty and
`build_entries` emits zero production entries. Finding extra tables, files or glTF values does
not register a capability. An empty registration list returns without opening the database or
reading its core identity. A feature must introduce its own producer and declaration contract.

`refresh_manifest(manifest, output)` rebuilds built-in entries and preserves still-valid
declarations from other finalizers. Each retained entry must have an actual matching content
hash and valid, disjoint, non-core ownership. Its recorded identity is preserved even when the
current base identity changes, so readers can report it as stale. Invalid or changed payload
declarations are dropped with a warning; refresh does not replace their hashes or identities.
Legacy door declarations are retired because their payload is now
core. With no retained declarations, serialization omits `extensions`. Full conversion,
metadata rebuild and asset repair refresh declarations after payload finalization and before
saving the staged manifest. Metadata and repair reports retain refresh warnings. A full conversion
offers the previous pack's declarations to the refresh: one whose payload is valid in the staging tree is
kept and its owned files are published with the pack, every other one is dropped with a warning in the
report (also with `--invalidate-cache`), and such a warning makes the conversion incomplete.

`refresh_manifest_with_finalized(manifest, output, finalized)` also accepts explicit entries
from payload finalizers. The generic mechanism can hash and validate these entries even though
it registers no built-in payloads. With no supplied entries, it behaves like `refresh_manifest`.
With explicit entries, it publishes the built-in entries plus that supplied set; the caller
must supply every foreign declaration it intends to publish. This API updates a base manifest;
an overlay producer remains responsible for serializing its own declarations into its overlay
document with the actual current base identity.

The caller must hold the existing exclusive asset lock and supply entries from finalizers that
ran under it. Publishing explicit finalized entries requires a nonempty current core identity,
matching `built_from` values, unique IDs, disjoint ownership including nested file prefixes,
canonical file paths and hashes matching the actual owned content. Errors leave the in-memory
manifest unchanged.
`recompute_hash(output, entry)` hashes an explicit entry's owned content without requiring a
built-in descriptor; missing owned tables, indexes or exact files are errors.

Finalized publication validates declared names and rejects `DOOR_LINKS`
(`mudcrab.door_links`) and known core table/index claims, including the six core RTree shadow
tables. It also rejects file ownership of `skyrim_world.db`, `conversion-manifest.json`,
`cell_cache.rkyv`, `lod-manifest.json`, `integration-report.json`, `record-reader.json`,
`metadata-rebuild.json` and `metadata-source-conversion-manifest.json`, and ownership of or
below the core `meshes/`, `textures/`, `scripts/`, `lod/` and `terrain/` directories.
SQL and file guards use ASCII case-insensitive comparison. Unrelated sibling prefixes and
files below a separate add-on directory remain available to extension producers.
Core glTF names `OPEN_SKYRIM_material`, `KHR_texture_transform`,
`KHR_materials_emissive_strength` and `KHR_materials_specular` cannot be claimed either.
Producers remain responsible for preserving any additional core ownership. Versions,
statistics and derivation rules belong to each producer.
The generic mechanism does not establish provenance.

Existing asset locks, provenance checks, record-data completeness, schema add-on identity,
derived-output proofs and transactional publication remain responsible for dataset publication.

## Content hash: `mudcrab-ext-hash-v1`

`ExtensionHasher` accepts owned tables, indexes, files and glTF extension values, then returns
the canonical digest with `finish`. The digest is SHA-256 over UTF-8 lines terminated by LF:
the algorithm name, extension ID and version, followed by owned tables, indexes, files and glTF
values in deterministic order. Table rows are canonically encoded and sorted before column
declarations. Indexes contribute their stored SQL; files contribute their content SHA-256.
Strings contribute UTF-8 hex and floating values contribute their binary64 bits.

JSON numeric hashing is defined by the parsed `serde_json::Value` and `serde_json::Number`,
not by raw JSON token text. A number accepted by `as_u64` or `as_i64` always contributes `i`
followed by its exact decimal integer value. Every remaining finite `f64` contributes `f`
followed by exactly 16 lowercase hexadecimal digits encoding its binary64 bits. Thus parsed
`1` contributes `i1`, while parsed `1.0` contributes `f3ff0000000000000`; their hashes differ.
Native signed and unsigned 64-bit integers remain exact, including adjacent integers above
`2^53` within those bounds.

The default JSON parser stores integer tokens outside `[-2^63, 2^64 - 1]` as `f64`; those
values may round or collapse to the same parsed value. Raw spelling is intentionally lost
before hashing, and the contract makes no exact-integer or decimal-precision claim for those
values or other parsed floating-point numbers. The token `-0` is parsed as floating negative
zero. Integer `0` contributes `i0`, floating `0.0` contributes `f0000000000000000`, and both
`-0` and `-0.0` contribute `f8000000000000000`. Floating zero sign bits are retained.
The default parser rejects non-finite and out-of-range numbers. Hashing requires finite
floating-point values and reports an error rather than encoding a NaN placeholder.
This contract uses the default production parser without `arbitrary_precision`,
`float_roundtrip`, `RawValue`, or other production parser changes. Core serialization and
output retain their existing behavior.

SQL hashing is not streaming: it buffers the canonical encoded rows of all owned tables until
`finish`, with one byte range per row for sorting. Hex-encoded text and blobs take approximately
two characters per input byte, in addition to row framing, range storage and allocation
overhead. Available memory is a material limit for large owned tables or multiple large tables.

For each owned glTF name declared in `extensionsUsed`, hashing recursively covers its
occurrences throughout the JSON document, including nodes and mesh primitives. Occurrence
locations use escaped JSON pointers, so moving a payload to a different location changes the
hash. Root and material occurrences use the same location-aware encoding.
Extension names are matched after JSON parsing: an escaped spelling of a name has the same
meaning as its unescaped spelling and cannot bypass discovery or hashing.

When glTF ownership is active, the converter scans `.gltf` and `.glb` documents throughout the
payload root. A malformed scanned document fails the hash or publication operation even if it
does not contain the owned name; it is not silently skipped. A UTF-8 BOM is accepted
consistently for glTF JSON. Before allocating the GLB JSON chunk, the reader validates version
2, the declared total size against the actual file size, a first JSON chunk, its length within
the file and four-byte chunk alignment. These checks bound the allocation by the actual file
size; they do not impose a separate small-file memory cap. An empty registry does not scan
meshes.

`shared::extensions::ExtensionHasher` defines the Rust stream. The independent standard-library
reference is `crates/converter/tests/support/extension_hash_reference.py`. It can check content
hashes recorded in a base manifest. Its test-only numeric parsing callbacks match the default
`serde_json` parser's numeric behavior; they do not change production parsing or serialization:

```text
python -I crates/converter/tests/support/extension_hash_reference.py <dataset-root>
```

Exit 0 means every listed content hash matches. This check does not establish producer
provenance, certify producer-defined statistics or enforce runtime version support. Qualified
database hashing and core identity lookup reject duplicate objects and temporary/view shadowing
rather than silently reading a different object.

## Runtime state and legacy doors

The engine loads base declaration state under its existing shared asset lock.
`engine::extensions::DataSetExtensions::data` exposes the generic registry to runtime consumers.
`read_data_extensions` reads the base manifest, and `open_features(data)` returns
`(DataSetExtensions, ExtensionNotices, WorldDatabaseFeatures)`. The app inserts the two capability
resources once, before plugins build, and passes the database policy to the ordinary database.
A missing manifest supplies no declarations; existing
runtime asset validation still decides whether the dataset can open. Invalid JSON or a
non-object manifest is an error; malformed optional entries follow the per-entry rules above.
Payload readers must validate their data before installing resources or systems; capability
metadata alone does not install a reader.

The engine consults core identity only for an accepted, supported legacy door declaration with
nonempty `built_from`. An empty registry, unknown IDs, rejected entries, unsupported versions or
an empty `built_from` do not trigger that lookup. Unrelated declarations therefore cannot make
ordinary core loading depend on an unused identity table. When a relevant identity lookup is
required, malformed identity data is an error rather than an absent anchor.

Current core owns `door_links` and `emittance`. New converter output does not declare
`DOOR_LINKS`, and refreshing a manifest retires an old declaration. The engine retains support
for legacy `mudcrab.door_links` version 1 declarations:

| Door state | Ordinary database behavior |
|---|---|
| No legacy declaration, complete core table | Read core doors using the existing ten-column detector. |
| No declaration, absent or incomplete old core table | Preserve the existing optional-table behavior; references have no door link. |
| Supported legacy declaration, not stale | Require a complete table at open; missing or incomplete schema is an error. |
| Stale or unsupported legacy declaration | Disable the door join, even when a complete core table is present. |
| Malformed or duplicate identifiable legacy declaration | Disable the door join and report its parse issue; do not fall back to core discovery. |

Undeclared core packs receive no capability notice, including older packs with no door table.
The database owns door-schema validation: `WorldDatabase::open` retains the default core policy,
and `open_with` requires a complete declared legacy table before carrying the policy through the
existing worker and reference query. NULL column positions, arrival handling and enable-parent
resolution are preserved. Legacy door-link support is read-only compatibility; no producer is
registered by this mechanism.
