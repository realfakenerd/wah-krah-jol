//! The data set extensions this converter writes, and their manifest entries.
//!
//! The core of a converted data set is exactly what `main`'s converter writes. Everything else is an
//! *extension* (see [`shared::extensions`]): whole tables, indexes, files or glTF extension names
//! with an id and a version, listed in `conversion-manifest.json` with a content hash.
//!
//! An extension never changes a core table or a core glTF extension. Moving one upstream means a
//! core change plus a version bump, after which the extension is dropped from [`specs`].
//! Spec: `docs/specs/converters/extensions.md`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use color_eyre::eyre::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use shared::extensions::{
    DOOR_LINKS, ExtensionEntry, ExtensionHasher, ExtensionOwns, core_build_identity,
};
use walkdir::WalkDir;

use crate::cache::ConversionManifest;

/// File name of the world database inside a converted data set.
const DATABASE: &str = "skyrim_world.db";

const GLB_MAGIC: &[u8; 4] = b"glTF";
const GLB_JSON_CHUNK: &[u8; 4] = b"JSON";

/// One extension this converter can write: its identity and everything it owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionSpec {
    pub id: &'static str,
    pub version: u32,
    pub owns: ExtensionOwns,
}

/// Persistent core tables owned by the ordinary exporter and its provenance writers,
/// including the shadow tables SQLite creates for core RTrees.
const MAIN_CORE_TABLES: &[&str] = &[
    "actor_model_parts",
    "actor_models",
    "cell_environment",
    "cell_lighting",
    "cell_regions",
    "cells",
    "climate_weathers",
    "climates",
    "conversion_cache",
    "door_links",
    "emittance",
    "exterior_spatial",
    "exterior_spatial_node",
    "exterior_spatial_parent",
    "exterior_spatial_rowid",
    "formid_map",
    "grass_types",
    "image_spaces",
    "inhouse_localized_fields",
    "inhouse_plugin_masters",
    "inhouse_schema_addons",
    "inhouse_source_records",
    "inhouse_terrain_layers",
    "inhouse_terrain_source_order",
    "inhouse_unresolved_terrain_textures",
    "land",
    "landscape_texture_grasses",
    "landscape_textures",
    "lighting_templates",
    "lights",
    "lod",
    "lod_build",
    "lod_chunks",
    "lod_chunks_spatial",
    "lod_chunks_spatial_node",
    "lod_chunks_spatial_parent",
    "lod_chunks_spatial_rowid",
    "movement_game_settings",
    "movement_types",
    "npcs",
    "plugins",
    "projection_omission_samples",
    "projection_omission_stats",
    "race_movement_links",
    "record_diagnostics",
    "record_overrides",
    "records",
    "reference_activate_parents",
    "reference_details",
    "reference_linked_refs",
    "reference_rooms",
    "references",
    "region_weathers",
    "room_bounds",
    "room_graph_stats",
    "room_links",
    "room_portals",
    "schema_info",
    "script_properties",
    "script_property_values",
    "scripts",
    "statics",
    "texture_sets",
    "waters",
    "weather_colors",
    "weather_directional_ambient",
    "weathers",
    "worldspace_environment",
    "worldspaces",
];
/// Persistent indexes owned by the core exporter and in-house provenance writers.
const MAIN_CORE_INDEXES: &[&str] = &[
    "idx_cells_grid",
    "idx_npcs_editor_id",
    "idx_records_cell_id",
    "idx_records_type",
    "idx_reference_activate_parents_parent",
    "idx_reference_linked_refs_target",
    "idx_reference_rooms_room",
    "idx_references_cell",
    "idx_room_bounds_cell",
    "idx_room_portals_cell",
    "idx_statics_editor_id",
    "record_diagnostics_form",
    "record_diagnostics_source",
    "record_overrides_chain",
    "record_overrides_source",
];

/// Built-in payload registrations. The ordinary converter currently writes only core output.
/// A feature adds its descriptors together with its producer, rather than claiming absent data.
pub fn specs() -> Vec<ExtensionSpec> {
    Vec::new()
}

/// Builds the manifest entries of the finished output folder `output`.
///
/// An extension is listed when the converter wrote it, even with zero rows: every table it owns
/// exists in `skyrim_world.db`. An extension that owns only glTF extensions is listed when an
/// output mesh contains an owned payload occurrence. If a table is missing the extension is not listed.
pub fn build_entries(output: &Path) -> Result<Vec<ExtensionEntry>> {
    build_entries_with_specs(output, specs())
}

fn build_entries_with_specs(
    output: &Path,
    specifications: Vec<ExtensionSpec>,
) -> Result<Vec<ExtensionEntry>> {
    if specifications.is_empty() {
        return Ok(Vec::new());
    }
    let database = output.join(DATABASE);
    let connection = if database.is_file() {
        Some(
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .wrap_err_with(|| format!("opening {}", database.display()))?,
        )
    } else {
        None
    };
    struct Pending {
        spec: ExtensionSpec,
        hasher: ExtensionHasher,
        stats: BTreeMap<String, u64>,
        /// Whether any mesh actually uses one of the extension's glTF extension names.
        gltf_used: bool,
    }
    let mut pending = Vec::new();
    for spec in specifications {
        color_eyre::eyre::ensure!(
            spec.owns.indexes.is_empty() || connection.is_some(),
            "extension {} owns indexes but {} is missing",
            spec.id,
            database.display()
        );
        let owns_database = !spec.owns.tables.is_empty();
        if owns_database {
            let Some(connection) = &connection else {
                continue;
            };
            let mut all_present = true;
            for table in &spec.owns.tables {
                all_present &= table_exists(connection, table)?;
            }
            if !all_present {
                continue;
            }
        }
        let mut hasher = ExtensionHasher::new(spec.id, spec.version);
        let mut stats = BTreeMap::new();
        if let Some(connection) = &connection {
            for table in &spec.owns.tables {
                hasher.add_table(connection, table)?;
                stats.insert(table.clone(), count_rows(connection, table)?);
            }
            for index in &spec.owns.indexes {
                hasher.add_index(connection, index)?;
            }
        }
        for (relative, path) in owned_files(output, &spec.owns.files)? {
            let bytes = std::fs::read(&path)
                .wrap_err_with(|| format!("reading owned file {}", path.display()))?;
            hasher.add_file(&relative, &bytes);
        }
        pending.push(Pending {
            spec,
            hasher,
            stats,
            gltf_used: false,
        });
    }

    // Every mesh is read once (header and JSON chunk only) for all glTF-owning extensions.
    if pending
        .iter()
        .any(|entry| !entry.spec.owns.gltf_extensions.is_empty())
    {
        for mesh in mesh_files(output)? {
            let relative = mesh
                .strip_prefix(output)
                .expect("mesh paths start at the output folder")
                .to_string_lossy()
                .replace('\\', "/");
            let json = read_gltf_json(&mesh)
                .wrap_err_with(|| format!("reading the glTF header of {}", mesh.display()))?;
            let document: serde_json::Value =
                serde_json::from_slice(json.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&json))
                    .wrap_err_with(|| {
                        format!("{} has an invalid glTF JSON chunk", mesh.display())
                    })?;
            for entry in &mut pending {
                for name in &entry.spec.owns.gltf_extensions {
                    entry.gltf_used |= entry.hasher.add_gltf(name, &relative, &document)?;
                }
            }
        }
        // An extension that owns only glTF extension names is listed when some mesh uses one of
        // them: a manifest entry for data nothing contains would hide a manifest/content mismatch.
        pending.retain(|entry| {
            let only_gltf = !entry.spec.owns.gltf_extensions.is_empty()
                && entry.spec.owns.tables.is_empty()
                && entry.spec.owns.indexes.is_empty()
                && entry.spec.owns.files.is_empty();
            !only_gltf || entry.gltf_used
        });
    }

    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let built_from = match &connection {
        Some(connection) => core_build_identity(connection)?.unwrap_or_default(),
        None => String::new(),
    };
    color_eyre::eyre::ensure!(
        !built_from.is_empty(),
        "cannot publish capabilities without a core build identity (lod_build); --no-lod datasets have no freshness anchor"
    );
    let entries: Vec<_> = pending
        .into_iter()
        .map(|entry| ExtensionEntry {
            id: entry.spec.id.to_owned(),
            version: entry.spec.version,
            owns: entry.spec.owns,
            sha256: entry.hasher.finish(),
            built_from: built_from.clone(),
            stats: entry.stats,
        })
        .collect();
    for entry in &entries {
        entry.validate_names()?;
    }
    let problems = shared::extensions::ownership_problems(&entries);
    color_eyre::eyre::ensure!(
        problems.is_empty(),
        "extension ownership conflicts: {}",
        problems.join("; ")
    );
    Ok(entries)
}

/// Refreshes built-in entries and retains still-valid declarations from other finalizers.
/// Rejected or changed declarations are dropped with a warning; no identity or hash is minted.
pub fn refresh_manifest(manifest: &mut ConversionManifest, output: &Path) -> Result<()> {
    refresh_manifest_with_notices(manifest, output).map(|_| ())
}

pub(crate) fn refresh_manifest_with_notices(
    manifest: &mut ConversionManifest,
    output: &Path,
) -> Result<Vec<String>> {
    let mut entries = build_entries(output)?;
    let own_ids: std::collections::BTreeSet<_> = specs().into_iter().map(|spec| spec.id).collect();
    let mut notices = Vec::new();
    for entry in &manifest.extensions {
        if own_ids.contains(entry.id.as_str()) {
            continue;
        }
        let result = (|| -> Result<()> {
            reject_core_claims(entry)?;
            validate_content_hash(output, entry)?;
            let problems = shared::extensions::ownership_problems(
                entries.iter().chain(std::iter::once(entry)),
            );
            color_eyre::eyre::ensure!(
                problems.is_empty(),
                "extension ownership conflicts: {}",
                problems.join("; ")
            );
            Ok(())
        })();
        match result {
            Ok(()) => entries.push(entry.clone()),
            Err(error) => {
                let notice = format!("dropped capability {} during refresh: {error:#}", entry.id);
                log::warn!("{notice}");
                notices.push(notice);
            }
        }
    }
    manifest.extensions = entries;
    Ok(notices)
}

/// Dataset-relative paths of every file the manifest's retained declarations own, as found in
/// `output` now. A conversion publishes only the files its report lists, so the payloads of
/// declarations that survived [`refresh_manifest_with_notices`] have to be listed too; otherwise
/// the published manifest would declare a capability whose files were left behind.
pub(crate) fn retained_payload_paths(
    manifest: &ConversionManifest,
    output: &Path,
) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = std::collections::BTreeSet::new();
    for entry in &manifest.extensions {
        for (relative, _) in owned_files(output, &entry.owns.files)? {
            paths.insert(std::path::PathBuf::from(relative));
        }
    }
    Ok(paths.into_iter().collect())
}

/// Recomputes `mudcrab-ext-hash-v1` of `entry` from what `output` holds now, reading only what the
/// entry's `owns` lists. Errors when an owned table, index or exact file is missing.
pub fn recompute_hash(output: &Path, entry: &ExtensionEntry) -> Result<String> {
    entry.validate_names()?;
    let mut hasher = ExtensionHasher::new(&entry.id, entry.version);
    if !entry.owns.tables.is_empty() || !entry.owns.indexes.is_empty() {
        let database = output.join(DATABASE);
        let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .wrap_err_with(|| format!("opening {}", database.display()))?;
        for table in &entry.owns.tables {
            hasher.add_table(&connection, table)?;
        }
        for index in &entry.owns.indexes {
            hasher.add_index(&connection, index)?;
        }
    }
    for (relative, path) in owned_files(output, &entry.owns.files)? {
        let bytes =
            fs::read(&path).wrap_err_with(|| format!("reading owned file {}", path.display()))?;
        hasher.add_file(&relative, &bytes);
    }
    if !entry.owns.gltf_extensions.is_empty() {
        for mesh in mesh_files(output)? {
            let relative = mesh
                .strip_prefix(output)
                .expect("mesh paths start at the output folder")
                .to_string_lossy()
                .replace('\\', "/");
            let json = read_gltf_json(&mesh)
                .wrap_err_with(|| format!("reading the glTF header of {}", mesh.display()))?;
            let document: serde_json::Value =
                serde_json::from_slice(json.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&json))
                    .wrap_err_with(|| {
                        format!("{} has an invalid glTF JSON chunk", mesh.display())
                    })?;
            for name in &entry.owns.gltf_extensions {
                hasher.add_gltf(name, &relative, &document)?;
            }
        }
    }
    Ok(hasher.finish())
}

/// Publishes built-in entries plus the explicitly finalized declarations.
/// The caller holds the exclusive asset lock. Every supplied entry needs a current nonempty
/// core identity, disjoint non-core ownership, and the actual content hash. Errors leave the
/// manifest untouched. With no supplied entries, existing declarations are refreshed in place.
pub fn refresh_manifest_with_finalized(
    manifest: &mut ConversionManifest,
    output: &Path,
    finalized: &[ExtensionEntry],
) -> Result<()> {
    if finalized.is_empty() {
        return refresh_manifest(manifest, output);
    }
    let mut entries = build_entries(output)?;
    let current = publication_identity(output)?;
    for entry in finalized {
        validate_finalized(output, entry, &current)?;
    }
    entries.extend(finalized.iter().cloned());
    let problems = shared::extensions::ownership_problems(&entries);
    color_eyre::eyre::ensure!(
        problems.is_empty(),
        "extension ownership conflicts: {}",
        problems.join("; ")
    );
    manifest.extensions = entries;
    Ok(())
}

fn publication_identity(output: &Path) -> Result<String> {
    let database = output.join(DATABASE);
    let current = if database.is_file() {
        let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .wrap_err_with(|| format!("opening {}", database.display()))?;
        core_build_identity(&connection)?.unwrap_or_default()
    } else {
        String::new()
    };
    color_eyre::eyre::ensure!(
        !current.is_empty(),
        "{} has no core build identity (lod_build); cannot publish capabilities for --no-lod datasets without a freshness anchor",
        output.display()
    );
    Ok(current)
}

fn reject_core_claims(entry: &ExtensionEntry) -> Result<()> {
    entry.validate_names()?;
    let claims_core = entry.id == DOOR_LINKS
        || entry.owns.tables.iter().any(|table| {
            MAIN_CORE_TABLES
                .iter()
                .any(|core| core.eq_ignore_ascii_case(table))
        })
        || entry.owns.indexes.iter().any(|index| {
            MAIN_CORE_INDEXES
                .iter()
                .any(|core| core.eq_ignore_ascii_case(index))
        })
        || entry.owns.gltf_extensions.iter().any(|name| {
            [
                "OPEN_SKYRIM_material",
                "KHR_texture_transform",
                "KHR_materials_emissive_strength",
                "KHR_materials_specular",
            ]
            .contains(&name.as_str())
        })
        || entry.owns.files.iter().any(|file| {
            let file = file.to_ascii_lowercase();
            let exact = file.trim_end_matches('/');
            [
                DATABASE,
                "conversion-manifest.json",
                "cell_cache.rkyv",
                "lod-manifest.json",
                "integration-report.json",
                "record-reader.json",
                "metadata-rebuild.json",
                "metadata-source-conversion-manifest.json",
            ]
            .contains(&exact)
                || ["meshes/", "textures/", "scripts/", "lod/", "terrain/"]
                    .iter()
                    .any(|core| exact == core.trim_end_matches('/') || file.starts_with(*core))
        });
    color_eyre::eyre::ensure!(
        !claims_core,
        "extension {} claims promoted or core-owned output",
        entry.id
    );
    Ok(())
}

fn validate_finalized(output: &Path, entry: &ExtensionEntry, current: &str) -> Result<()> {
    reject_core_claims(entry)?;
    color_eyre::eyre::ensure!(
        entry.built_from == current,
        "extension {} was built from {:?}, but the data set's core is {current}",
        entry.id,
        entry.built_from
    );
    validate_content_hash(output, entry)
}

fn validate_content_hash(output: &Path, entry: &ExtensionEntry) -> Result<()> {
    let actual = recompute_hash(output, entry)
        .wrap_err_with(|| format!("hashing extension {}", entry.id))?;
    color_eyre::eyre::ensure!(
        actual == entry.sha256,
        "extension {} lists sha256 {}, but its content hashes to {actual}",
        entry.id,
        entry.sha256
    );
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(shared::extensions::locate_object(connection, "table", table)?.is_some())
}

fn count_rows(connection: &Connection, table: &str) -> Result<u64> {
    Ok(connection.query_row(
        &format!("SELECT count(*) FROM \"{}\"", table.replace('"', "\"\"")),
        [],
        |row| row.get(0),
    )?)
}

/// The files an extension owns: exact paths, and every file below a directory entry (an entry
/// ending in `/`; a directory that does not exist covers nothing). Paths are relative to `root`
/// with `/` separators. A missing exact file is an error.
fn owned_files(root: &Path, entries: &[String]) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut files = BTreeMap::new();
    for entry in entries {
        shared::extensions::validate_owned_file_path(entry)?;
        if entry.ends_with('/') {
            let folder = root.join(entry.trim_end_matches('/'));
            if !folder.is_dir() {
                continue;
            }
            for item in WalkDir::new(&folder) {
                let item = item?;
                if item.file_type().is_file() {
                    let relative = item
                        .path()
                        .strip_prefix(root)
                        .expect("walked paths start at the output folder")
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.insert(relative, item.into_path());
                }
            }
        } else {
            let path = root.join(entry);
            if !path.is_file() {
                color_eyre::eyre::bail!("owned file {entry} is missing");
            }
            files.insert(entry.clone(), path);
        }
    }
    Ok(files.into_iter().collect())
}

/// Every `.glb` and `.gltf` under `root`, in any directory.
fn mesh_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut meshes = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let is_mesh = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("glb") || extension.eq_ignore_ascii_case("gltf")
            });
        if is_mesh {
            meshes.push(entry.into_path());
        }
    }
    Ok(meshes)
}

/// The JSON document of a mesh: for a `.glb` only the header and the first (JSON) chunk are read.
fn read_gltf_json(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gltf"))
    {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        return Ok(bytes);
    }
    let mut header = [0u8; 20];
    file.read_exact(&mut header)?;
    if &header[0..4] != GLB_MAGIC {
        bail!("not a GLB");
    }
    color_eyre::eyre::ensure!(
        u32::from_le_bytes(header[4..8].try_into()?) == 2,
        "unsupported GLB version"
    );
    if &header[16..20] != GLB_JSON_CHUNK {
        bail!("no JSON chunk first");
    }
    let file_length = file.metadata()?.len();
    let declared = u64::from(u32::from_le_bytes(header[8..12].try_into()?));
    let length = u32::from_le_bytes(header[12..16].try_into()?) as usize;
    color_eyre::eyre::ensure!(
        declared == file_length && declared >= 20,
        "GLB declared length does not match file size"
    );
    color_eyre::eyre::ensure!(
        length as u64 <= file_length - 20,
        "GLB JSON chunk length exceeds file size"
    );
    color_eyre::eyre::ensure!(
        length.is_multiple_of(4),
        "GLB JSON chunk length is not aligned"
    );
    let mut json = vec![0u8; length];
    file.read_exact(&mut json)?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalized_entries_cannot_claim_core_asset_folders_or_provenance_files() {
        let mut allowed = Vec::new();
        for path in [
            "textures/",
            "textures/example.ktx2",
            "scripts/",
            "scripts/example.luau",
            "integration-report.json",
            "record-reader.json",
            "metadata-rebuild.json",
            "metadata-source-conversion-manifest.json",
        ] {
            for spelling in [path.to_owned(), path.to_ascii_uppercase()] {
                let entry = ExtensionEntry {
                    id: "example.extra".into(),
                    version: 1,
                    owns: ExtensionOwns {
                        files: vec![spelling.clone()],
                        ..Default::default()
                    },
                    sha256: "0".repeat(64),
                    built_from: "core".into(),
                    stats: BTreeMap::new(),
                };
                if reject_core_claims(&entry).is_ok() {
                    allowed.push(spelling);
                }
            }
        }
        assert!(
            allowed.is_empty(),
            "core output claims were accepted: {allowed:?}"
        );
        for path in [
            "textures_extra/payload.bin",
            "scripts-addon/payload.bin",
            "addons/integration-report.json",
        ] {
            let entry = ExtensionEntry {
                id: "example.extra".into(),
                version: 1,
                owns: ExtensionOwns {
                    files: vec![path.into()],
                    ..Default::default()
                },
                sha256: "0".repeat(64),
                built_from: "core".into(),
                stats: BTreeMap::new(),
            };
            assert!(
                reject_core_claims(&entry).is_ok(),
                "unrelated namespaced output was rejected: {path}"
            );
        }
    }
    use std::collections::BTreeSet;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn output_with_tables(without: &[&str]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        crate::esm::exporter::create_tables(&connection).unwrap();
        for table in without {
            connection
                .execute_batch(&format!(
                    "DROP TABLE IF EXISTS {}",
                    shared::extensions::quote_identifier(table)
                ))
                .unwrap();
        }
        directory
    }

    fn gltf_spec() -> ExtensionSpec {
        ExtensionSpec {
            id: "example.payload",
            version: 1,
            owns: ExtensionOwns {
                gltf_extensions: vec!["EXAMPLE_payload".into()],
                ..Default::default()
            },
        }
    }

    #[test]
    fn ordinary_converter_registers_no_optional_payloads() {
        assert!(specs().is_empty());
        let directory = output_with_tables(&[]);
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        connection
            .execute_batch("CREATE TABLE extra_payload(value INTEGER);")
            .unwrap();
        assert!(build_entries(directory.path()).unwrap().is_empty());
        let mut manifest = ConversionManifest::default();
        manifest.extensions.push(ExtensionEntry {
            id: DOOR_LINKS.into(),
            version: 1,
            owns: ExtensionOwns::default(),
            sha256: String::new(),
            built_from: String::new(),
            stats: BTreeMap::new(),
        });
        refresh_manifest(&mut manifest, directory.path()).unwrap();
        assert!(manifest.extensions.is_empty());
    }

    #[test]
    fn explicit_descriptors_hash_their_own_tables_indexes_and_files() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "core");
        let finalized = custom_entry(
            directory.path(),
            "example.payload",
            "extra_payload",
            "payload",
        );
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        connection
            .execute_batch("CREATE INDEX idx_payload ON extra_payload(a);")
            .unwrap();
        let spec = ExtensionSpec {
            id: "example.payload",
            version: 1,
            owns: ExtensionOwns {
                tables: vec!["extra_payload".into()],
                indexes: vec!["idx_payload".into()],
                files: vec!["payload/".into()],
                ..Default::default()
            },
        };
        let entries = build_entries_with_specs(directory.path(), vec![spec]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].stats["extra_payload"], 1);
        assert_eq!(entries[0].built_from, "core");
        assert_eq!(
            entries[0].sha256,
            recompute_hash(directory.path(), &entries[0]).unwrap()
        );
        assert_ne!(entries[0].sha256, finalized.sha256);
        assert!(build_entries(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn every_converter_table_is_core_or_owned() {
        let connection = Connection::open_in_memory().unwrap();
        crate::esm::exporter::create_tables(&connection).unwrap();
        let owned: BTreeSet<String> = specs()
            .into_iter()
            .flat_map(|spec| spec.owns.tables)
            .collect();
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap();
        let tables: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for table in tables {
            assert!(
                MAIN_CORE_TABLES.contains(&table.as_str()) || owned.contains(&table),
                "table {table} is neither main's core nor owned by an extension"
            );
        }
    }

    #[test]
    fn every_converter_index_is_core_or_owned() {
        let connection = Connection::open_in_memory().unwrap();
        crate::esm::exporter::create_tables(&connection).unwrap();
        let owned: BTreeSet<String> = specs()
            .into_iter()
            .flat_map(|spec| spec.owns.indexes)
            .collect();
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL")
            .unwrap();
        let indexes: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for index in indexes {
            assert!(
                MAIN_CORE_INDEXES.contains(&index.as_str()) || owned.contains(&index),
                "index {index} is neither core nor owned by an extension"
            );
        }
    }

    #[test]
    fn finalized_entries_cannot_claim_current_core_tables_or_indexes() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let template = custom_entry(directory.path(), "example.extra", "extra_table", "extra");
        // Exercise every persistent core name, including in-house provenance output which
        // create_tables alone does not produce. The guard must run before content hashing.
        for (kind, names) in [("table", MAIN_CORE_TABLES), ("index", MAIN_CORE_INDEXES)] {
            for name in names {
                for spelling in [(*name).to_owned(), name.to_ascii_uppercase()] {
                    let mut entry = template.clone();
                    entry.owns = ExtensionOwns::default();
                    if kind == "table" {
                        entry.owns.tables.push(spelling);
                    } else {
                        // Index-only ownership must not bypass the core table guard.
                        entry.owns.indexes.push(spelling);
                    }
                    assert!(
                        refused(directory.path(), &[entry]).contains("core-owned"),
                        "{kind} {name}"
                    );
                }
            }
        }
    }

    #[test]
    fn finalized_entries_cannot_own_generated_core_rtree_shadows() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        let shadows: Vec<String> = connection.prepare(
            "SELECT name FROM pragma_table_list WHERE schema='main' AND type='shadow' ORDER BY name"
        ).unwrap().query_map([], |row| row.get(0)).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(
            shadows,
            [
                "exterior_spatial_node",
                "exterior_spatial_parent",
                "exterior_spatial_rowid",
                "lod_chunks_spatial_node",
                "lod_chunks_spatial_parent",
                "lod_chunks_spatial_rowid",
            ]
        );
        drop(connection);
        for name in shadows {
            for spelling in [name.clone(), name.to_ascii_uppercase()] {
                let mut entry = ExtensionEntry {
                    id: "example.extra".into(),
                    version: 1,
                    owns: ExtensionOwns {
                        tables: vec![spelling],
                        ..Default::default()
                    },
                    sha256: String::new(),
                    built_from: "abc".into(),
                    stats: BTreeMap::new(),
                };
                // A real, correctly hashed claim must fail ownership, not an earlier hash check.
                entry.sha256 = recompute_hash(directory.path(), &entry).unwrap();
                assert!(
                    refused(directory.path(), &[entry]).contains("core-owned"),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn finalized_sql_aliases_conflict_with_converter_and_foreign_owners() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let one = custom_entry(directory.path(), "example.a", "extra_table", "extra");
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        connection
            .execute_batch("CREATE INDEX idx_extra ON extra_table(a);")
            .unwrap();
        drop(connection);
        for owns in [
            ExtensionOwns {
                tables: vec!["EXTRA_TABLE".into()],
                ..Default::default()
            },
            ExtensionOwns {
                indexes: vec!["IDX_EXTRA".into()],
                ..Default::default()
            },
        ] {
            let mut a = one.clone();
            a.owns.files.clear();
            a.owns.indexes = vec!["idx_extra".into()];
            a.sha256 = recompute_hash(directory.path(), &a).unwrap();
            let mut b = a.clone();
            b.id = "example.b".into();
            b.owns = owns;
            b.sha256 = recompute_hash(directory.path(), &b).unwrap();
            assert!(
                refused(directory.path(), &[a.clone(), b.clone()]).contains("ownership conflicts")
            );
            assert!(refused(directory.path(), &[b, a]).contains("ownership conflicts"));
        }
    }

    #[test]
    fn malformed_identity_blocks_publication_but_preserves_existing_payloads() {
        for sql in [
            "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, renamed_identity TEXT);",
            "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, build_identity);
             INSERT INTO lod_build VALUES(1, 42);",
        ] {
            let directory = output_with_tables(&[]);
            with_identity(directory.path(), "abc");
            let entry = custom_entry(directory.path(), "example.extra", "extra_table", "extra");
            let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
            connection.execute_batch("DROP TABLE lod_build;").unwrap();
            connection.execute_batch(sql).unwrap();
            drop(connection);
            assert!(build_entries(directory.path()).unwrap().is_empty());
            let mut manifest = ConversionManifest {
                extensions: vec![entry.clone()],
                ..Default::default()
            };
            let notices = refresh_manifest_with_notices(&mut manifest, directory.path()).unwrap();
            assert_eq!(manifest.extensions, vec![entry.clone()]);
            assert!(notices.is_empty());
            assert!(refused(directory.path(), &[entry]).contains("reading core build identity"));
        }
    }

    #[test]
    fn finalized_extensions_cannot_claim_core_tables_or_retired_door_identity() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let mut entry = custom_entry(directory.path(), "mudcrab.extra", "extra_table", "extra");
        for table in ["door_links", "Door_Links", "emittance", "EMITTANCE"] {
            entry.owns = ExtensionOwns {
                tables: names(&[table]),
                ..Default::default()
            };
            entry.sha256 = recompute_hash(directory.path(), &entry).unwrap();
            assert!(refused(directory.path(), &[entry.clone()]).contains("core-owned"));
        }
        entry.id = DOOR_LINKS.to_owned();
        entry.owns = ExtensionOwns::default();
        entry.sha256 = recompute_hash(directory.path(), &entry).unwrap();
        assert!(refused(directory.path(), &[entry]).contains("core-owned"));
    }

    #[test]
    fn no_database_lists_no_table_extension() {
        let directory = tempfile::tempdir().unwrap();
        assert!(build_entries(directory.path()).unwrap().is_empty());
    }

    #[test]
    fn empty_registry_does_not_open_a_malformed_database() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(DATABASE), b"not a database").unwrap();
        fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"extensions":[]}"#,
        )
        .unwrap();
        assert_eq!(
            shared::extensions::DataExtensions::load(directory.path())
                .unwrap()
                .entries()
                .count(),
            0
        );
        assert!(build_entries(directory.path()).unwrap().is_empty());
        refresh_manifest(&mut ConversionManifest::default(), directory.path()).unwrap();
    }

    #[test]
    fn index_only_spec_requires_its_database() {
        let directory = tempfile::tempdir().unwrap();
        let spec = ExtensionSpec {
            id: "example.index",
            version: 1,
            owns: ExtensionOwns {
                indexes: names(&["idx_payload"]),
                ..Default::default()
            },
        };
        assert!(
            build_entries_with_specs(directory.path(), vec![spec])
                .unwrap_err()
                .to_string()
                .contains("owns indexes")
        );
    }

    #[test]
    fn table_discovery_matches_hasher_case_and_view_rules() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE payload(a); CREATE VIEW payload_view AS SELECT * FROM payload;",
            )
            .unwrap();
        assert!(table_exists(&connection, "PAYLOAD").unwrap());
        assert!(
            table_exists(&connection, "payload_view")
                .unwrap_err()
                .to_string()
                .contains("view")
        );
    }

    #[test]
    fn foreign_declarations_survive_refresh_until_payload_bytes_change() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let entry = custom_entry(
            directory.path(),
            "example.foreign",
            "extra_payload",
            "payload",
        );
        let mut manifest = ConversionManifest {
            extensions: vec![entry.clone()],
            ..Default::default()
        };
        assert!(
            refresh_manifest_with_notices(&mut manifest, directory.path())
                .unwrap()
                .is_empty()
        );
        assert_eq!(manifest.extensions, vec![entry.clone()]);
        // A changed base identity must retain the original anchor so readers can report Stale.
        with_identity(directory.path(), "changed-core");
        assert!(
            refresh_manifest_with_notices(&mut manifest, directory.path())
                .unwrap()
                .is_empty()
        );
        assert_eq!(manifest.extensions, vec![entry.clone()]);
        let data = shared::extensions::DataExtensions::from_manifest_json(
            &serde_json::to_string(&manifest).unwrap(),
            Some("changed-core".into()),
        )
        .unwrap();
        assert_eq!(
            data.status(&entry.id, 1..=1),
            shared::extensions::ExtensionStatus::Stale {
                built_from: "abc".into(),
            }
        );
        fs::write(directory.path().join("payload/a.bin"), b"changed").unwrap();
        let notices = refresh_manifest_with_notices(&mut manifest, directory.path()).unwrap();
        assert!(manifest.extensions.is_empty());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("example.foreign") && notices[0].contains("content hashes to"));
    }

    #[test]
    fn missing_lod_identity_refuses_new_capability_publication() {
        let directory = output_with_tables(&[]);
        let entry = custom_entry(
            directory.path(),
            "example.foreign",
            "extra_payload",
            "payload",
        );
        let error = refused(directory.path(), &[entry]);
        assert!(error.contains("--no-lod") && error.contains("freshness anchor"));
    }

    #[test]
    fn finalized_entries_cannot_claim_core_files_or_mesh_folders() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "core");
        let mut entry = custom_entry(
            directory.path(),
            "example.foreign",
            "extra_payload",
            "payload",
        );
        for name in [
            "skyrim_world.db",
            "SKYRIM_WORLD.DB",
            "conversion-manifest.json",
            "cell_cache.rkyv",
            "meshes/",
            "MESHES/a.glb",
            "lod/",
            "terrain/a.glb",
        ] {
            entry.owns.files = vec![name.into()];
            // Policy rejection must precede any attempt to read or hash these paths.
            assert!(refused(directory.path(), std::slice::from_ref(&entry)).contains("core-owned"));
        }
    }

    #[test]
    fn core_gltf_names_cannot_be_claimed_but_additional_payload_names_can() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let mut entry = custom_entry(
            directory.path(),
            "example.foreign",
            "extra_payload",
            "payload",
        );
        entry.owns = ExtensionOwns::default();
        for name in [
            "OPEN_SKYRIM_material",
            "KHR_texture_transform",
            "KHR_materials_emissive_strength",
            "KHR_materials_specular",
        ] {
            entry.owns.gltf_extensions = vec![name.into()];
            assert!(refused(directory.path(), std::slice::from_ref(&entry)).contains("core-owned"));
        }
        entry.owns.gltf_extensions = vec!["OPEN_SKYRIM_material_animation".into()];
        fs::write(directory.path().join("example.gltf"), br#"{"extensionsUsed":["OPEN_SKYRIM_material_animation"],"extensions":{"OPEN_SKYRIM_material_animation":{"value":1}}}"#).unwrap();
        entry.sha256 = recompute_hash(directory.path(), &entry).unwrap();
        refresh_manifest_with_finalized(
            &mut ConversionManifest::default(),
            directory.path(),
            &[entry],
        )
        .unwrap();
    }

    #[test]
    fn glb_chunk_length_is_checked_before_allocating() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.glb");
        let mut header = Vec::new();
        header.extend_from_slice(GLB_MAGIC);
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&20u32.to_le_bytes());
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(GLB_JSON_CHUNK);
        fs::write(&path, header).unwrap();
        assert!(
            read_gltf_json(&path)
                .unwrap_err()
                .to_string()
                .contains("chunk length exceeds file size")
        );
    }

    #[test]
    fn gltf_only_descriptor_needs_payload_values_not_only_a_used_name() {
        let directory = output_with_tables(&[]);
        fs::write(
            directory.path().join("empty.gltf"),
            br#"{"extensionsUsed":["EXAMPLE_payload"]}"#,
        )
        .unwrap();
        assert!(
            build_entries_with_specs(directory.path(), vec![gltf_spec()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn gltf_bom_is_accepted_but_owned_float_overflow_is_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("example.gltf");
        let entry = ExtensionEntry {
            id: "example.payload".into(),
            version: 1,
            owns: gltf_spec().owns,
            sha256: String::new(),
            built_from: "core".into(),
            stats: Default::default(),
        };
        let document = br#"{"extensionsUsed":["EXAMPLE_payload"],"extensions":{"EXAMPLE_payload":{"value":1}}}"#;
        fs::write(&path, document).unwrap();
        let plain = recompute_hash(directory.path(), &entry).unwrap();
        let mut bom = b"\xef\xbb\xbf".to_vec();
        bom.extend_from_slice(document);
        fs::write(&path, bom).unwrap();
        assert_eq!(recompute_hash(directory.path(), &entry).unwrap(), plain);
        fs::write(&path, br#"{"extensionsUsed":["EXAMPLE_payload"],"extensions":{"EXAMPLE_payload":{"value":1e999}}}"#).unwrap();
        assert!(
            recompute_hash(directory.path(), &entry)
                .unwrap_err()
                .to_string()
                .contains("invalid glTF JSON chunk")
        );
    }

    fn with_identity(output: &Path, identity: &str) {
        let connection = Connection::open(output.join(DATABASE)).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO lod_build(id, build_identity) VALUES (1, ?1)",
                [identity],
            )
            .unwrap();
    }

    fn custom_entry(output: &Path, id: &str, table: &str, folder: &str) -> ExtensionEntry {
        let connection = Connection::open(output.join(DATABASE)).unwrap();
        connection
            .execute(
                &format!("CREATE TABLE IF NOT EXISTS {table}(a INTEGER)"),
                [],
            )
            .unwrap();
        connection
            .execute(&format!("INSERT INTO {table} VALUES (7)"), [])
            .unwrap();
        drop(connection);
        fs::create_dir_all(output.join(folder)).unwrap();
        fs::write(output.join(folder).join("a.bin"), b"payload").unwrap();
        let mut entry = ExtensionEntry {
            id: id.to_owned(),
            version: 1,
            owns: ExtensionOwns {
                tables: vec![table.to_owned()],
                files: vec![format!("{folder}/")],
                ..Default::default()
            },
            sha256: String::new(),
            built_from: "abc".to_owned(),
            stats: BTreeMap::new(),
        };
        entry.sha256 = recompute_hash(output, &entry).unwrap();
        entry
    }

    #[test]
    fn finalized_entries_join_the_converter_s_own() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let base = custom_entry(directory.path(), "example.geometry", "payload_x", "payload");
        let terrain = custom_entry(
            directory.path(),
            "example.terrain",
            "payload_t",
            "terrain_payload",
        );
        let mut manifest = ConversionManifest::default();
        refresh_manifest_with_finalized(
            &mut manifest,
            directory.path(),
            &[base.clone(), terrain.clone()],
        )
        .unwrap();
        let ids: Vec<&str> = manifest.extensions.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["example.geometry", "example.terrain"]);
        assert_eq!(
            manifest.extensions.iter().find(|entry| entry.id == base.id),
            Some(&base)
        );
        assert_eq!(
            manifest
                .extensions
                .iter()
                .find(|entry| entry.id == terrain.id),
            Some(&terrain)
        );
        // No finalized entries: the same list as refresh_manifest.
        let mut plain = ConversionManifest::default();
        refresh_manifest(&mut plain, directory.path()).unwrap();
        let mut empty = ConversionManifest::default();
        refresh_manifest_with_finalized(&mut empty, directory.path(), &[]).unwrap();
        assert_eq!(plain.extensions, empty.extensions);
    }

    fn refused(output: &Path, finalized: &[ExtensionEntry]) -> String {
        let mut manifest = ConversionManifest {
            extensions: vec![ExtensionEntry {
                id: "kept".to_owned(),
                version: 1,
                owns: ExtensionOwns::default(),
                sha256: "0".repeat(64),
                built_from: String::new(),
                stats: BTreeMap::new(),
            }],
            ..Default::default()
        };
        let before = manifest.clone();
        let error = refresh_manifest_with_finalized(&mut manifest, output, finalized)
            .expect_err("the call must be refused");
        assert_eq!(
            manifest, before,
            "a refused call must not change the manifest"
        );
        format!("{error:#}")
    }

    #[test]
    fn a_mismatched_core_identity_is_refused() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let entry = custom_entry(directory.path(), "example.geometry", "payload_x", "payload");
        with_identity(directory.path(), "def");
        assert!(refused(directory.path(), std::slice::from_ref(&entry)).contains("was built from"));
        // A data set without any identity cannot take a finalized entry either.
        let bare = output_with_tables(&[]);
        let mut orphan = entry;
        orphan.built_from = String::new();
        assert!(refused(bare.path(), &[orphan]).contains("no core build identity"));
    }

    #[test]
    fn duplicate_ids_and_conflicting_ownership_are_refused() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let one = custom_entry(directory.path(), "example.geometry", "payload_x", "payload");
        assert!(refused(directory.path(), &[one.clone(), one.clone()]).contains("listed twice"));
        let mut taken = one.clone();
        taken.id = "example.taken".into();
        taken.owns.files.clear();
        taken.sha256 = recompute_hash(directory.path(), &taken).unwrap();
        assert!(refused(directory.path(), &[one.clone(), taken]).contains("ownership conflicts"));
        let mut shared_folder = custom_entry(directory.path(), "mudcrab.b", "payload_b", "payload");
        shared_folder.sha256 = recompute_hash(directory.path(), &shared_folder).unwrap();
        assert!(refused(directory.path(), &[one, shared_folder]).contains("ownership conflicts"));
    }

    #[test]
    fn changed_owned_bytes_are_refused() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let entry = custom_entry(directory.path(), "example.geometry", "payload_x", "payload");
        fs::write(directory.path().join("payload/a.bin"), b"changed").unwrap();
        assert!(
            refused(directory.path(), std::slice::from_ref(&entry)).contains("content hashes to")
        );
        fs::write(directory.path().join("payload/a.bin"), b"payload").unwrap();
        let connection = Connection::open(directory.path().join(DATABASE)).unwrap();
        connection
            .execute("INSERT INTO payload_x VALUES (8)", [])
            .unwrap();
        drop(connection);
        assert!(refused(directory.path(), &[entry]).contains("content hashes to"));
    }

    #[test]
    fn nested_file_ownership_is_refused() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let base = custom_entry(directory.path(), "example.geometry", "payload_x", "payload");
        // Another extension owning a folder or an exact file below the base's folder.
        let mut below = custom_entry(
            directory.path(),
            "example.terrain",
            "payload_t",
            "payload/terrain",
        );
        // The nested files exist now, so the base hash covers them.
        let mut base = base;
        base.sha256 = recompute_hash(directory.path(), &base).unwrap();
        assert!(
            refused(directory.path(), &[base.clone(), below.clone()]).contains("overlaps"),
            "a folder below another extension's folder"
        );
        below.owns.files = vec!["payload/a.bin".to_owned()];
        below.sha256 = recompute_hash(directory.path(), &below).unwrap();
        assert!(
            refused(directory.path(), &[base.clone(), below.clone()]).contains("overlaps"),
            "an exact file below another extension's folder"
        );
        // Order does not matter, and the converter's own mesh folder counts too.
        assert!(refused(directory.path(), &[below, base.clone()]).contains("overlaps"));
        // Disjoint folders with a shared name prefix are fine.
        let sibling = custom_entry(directory.path(), "mudcrab.s", "payload_s", "payload_extra");
        let mut manifest = ConversionManifest::default();
        refresh_manifest_with_finalized(&mut manifest, directory.path(), &[base, sibling]).unwrap();
    }

    #[test]
    fn escaped_gltf_names_match_semantic_discovery_and_hashes() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "core");
        let path = directory.path().join("meshes/escaped.gltf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let plain = r#"{"extensionsUsed":["EXAMPLE_payload"],"materials":[{"extensions":{"EXAMPLE_payload":{"value":1}}}]}"#;
        fs::write(&path, plain).unwrap();
        let entries = build_entries_with_specs(directory.path(), vec![gltf_spec()]).unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.id == "example.payload")
            .unwrap();
        let hash = recompute_hash(directory.path(), entry).unwrap();
        fs::write(
            &path,
            plain.replace("EXAMPLE_payload", r"\u0045XAMPLE_payload"),
        )
        .unwrap();
        assert_eq!(
            build_entries_with_specs(directory.path(), vec![gltf_spec()]).unwrap(),
            entries
        );
        assert_eq!(recompute_hash(directory.path(), entry).unwrap(), hash);
        fs::write(
            &path,
            plain
                .replace("EXAMPLE_payload", r"\u0045XAMPLE_payload")
                .replace("value\":1", "value\":2"),
        )
        .unwrap();
        assert_ne!(recompute_hash(directory.path(), entry).unwrap(), hash);
    }

    #[test]
    fn finalized_entries_refuse_file_aliases_without_mutating_the_manifest() {
        let directory = output_with_tables(&[]);
        with_identity(directory.path(), "abc");
        let canonical = custom_entry(directory.path(), "example.a", "extra_table", "extra");
        let mut alias = canonical.clone();
        alias.id = "example.b".into();
        alias.owns = ExtensionOwns {
            files: vec!["extra/./a.bin".into()],
            ..Default::default()
        };
        let mut hasher = ExtensionHasher::new(&alias.id, alias.version);
        hasher.add_file("extra/./a.bin", b"payload");
        alias.sha256 = hasher.finish();
        assert!(refused(directory.path(), &[canonical, alias]).contains("canonical"));
    }

    #[test]
    fn owned_files_reject_aliases_and_paths_outside_the_dataset() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("extra")).unwrap();
        fs::write(directory.path().join("extra/a.bin"), b"payload").unwrap();
        for path in [
            "extra/./a.bin",
            "extra/../extra/a.bin",
            "extra//a.bin",
            "extra\\a.bin",
            "../outside.bin",
            "/absolute.bin",
            "Q:/absolute.bin",
            "extra//",
            "./",
            "",
        ] {
            assert!(
                owned_files(directory.path(), &[path.into()]).is_err(),
                "accepted {path:?}"
            );
        }
        assert_eq!(
            owned_files(directory.path(), &["extra/a.bin".into()])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            owned_files(directory.path(), &["extra/".into()])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_missing_exact_owned_file_is_an_error() {
        let directory = output_with_tables(&[]);
        let error = owned_files(directory.path(), &["payload/nothing.bin".to_owned()])
            .expect_err("an exact file must exist");
        assert!(format!("{error}").contains("is missing"));
        // A missing folder entry covers nothing.
        assert!(
            owned_files(directory.path(), &["payload/".to_owned()])
                .unwrap()
                .is_empty()
        );
    }

    proptest::proptest! {
        #[test]
        fn canonical_owned_files_reject_equivalent_dot_aliases(
            folder in "[a-z][a-z0-9]{0,7}",
            file in "[a-z][a-z0-9]{0,7}",
        ) {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join(&folder)).unwrap();
            let path = format!("{folder}/{file}.bin");
            fs::write(directory.path().join(&path), b"payload").unwrap();
            proptest::prop_assert_eq!(owned_files(directory.path(), &[path]).unwrap().len(), 1);
            for alias in [
                format!("{folder}/./{file}.bin"),
                format!("{folder}/../{folder}/{file}.bin"),
                format!("{folder}//{file}.bin"),
            ] {
                proptest::prop_assert!(owned_files(directory.path(), std::slice::from_ref(&alias)).is_err(), "accepted {}", alias);
            }
        }
    }
}
