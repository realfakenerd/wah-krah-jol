//! Adapt schema-decoded fields to the established runtime projections.
//! Source payloads remain separate from resolved canonical projection fields.

use crate::{
    config::RecordReader,
    esm::{
        RecordConversion,
        cell_cache::validate_inhouse_land,
        exporter::{
            RejectedLightFields, RejectedLightInputs, create_tables,
            export_inhouse_record_archive_to_db, export_inhouse_to_db, validate_inhouse_record,
        },
        load_order::LoadOrder,
        records::RawRecord,
    },
    records::{self, Value},
};
use color_eyre::Result;
use rusqlite::{Connection, params};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

/// Decoder/adapter behavior version for output changes outside the authored schema.
pub const ADAPTER_VERSION: u32 = 13;

/// Legacy metadata projection contract: preserve WRLD payloads and record-aware VMAD links.
/// Version 4 resolves localized string fields to the in-house reader's text (`legacy_text`).
/// This identity does not change the independent converted-asset producer/settings proofs.
pub const LEGACY_PROJECTION_VERSION: u32 = 5;

/// Where localized plugins' string tables are found, and in which language.
#[derive(Clone, Copy)]
pub(crate) struct StringSources<'a> {
    pub(crate) roots: StringRoots<'a>,
    /// As Skyrim's `sLanguage`: the tables are `Strings/<plugin>_<language>.<bank>`.
    pub(crate) language: &'a str,
}

impl<'a> StringSources<'a> {
    /// Tables in Skyrim's default language, for exports without a configuration.
    pub(crate) fn default_language(roots: StringRoots<'a>) -> Self {
        Self {
            roots,
            language: crate::config::DEFAULT_LANGUAGE,
        }
    }
}

/// Explicit loose-text precedence while keeping original plugin paths and provenance.
#[derive(Clone, Copy)]
pub(crate) enum StringRoots<'a> {
    /// Ordinary Data inputs search each plugin's loose directory before the archive VFS.
    PluginLooseFirst(&'a Path),
    /// MO2 inputs read only the effective loose winner, then the effective archive VFS.
    EffectiveData {
        archive_root: &'a Path,
        loose_files: &'a BTreeMap<String, PathBuf>,
    },
}

impl<'a> StringRoots<'a> {
    /// Archive extraction already resolves load-order precedence in this directory.
    fn archive_root(self) -> &'a Path {
        match self {
            Self::PluginLooseFirst(root) => root,
            Self::EffectiveData { archive_root, .. } => archive_root,
        }
    }
}

/// Explicit diagnostic export scope; existing publication APIs always select Full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordBundleMode {
    Full,
    RecordArchiveOnly,
}

/// SQL schemas remain present but these runtime views are deliberately unpopulated in archives.
const DEFERRED_RUNTIME_TABLES: &[&str] = &[
    "worldspaces",
    "cells",
    "references",
    "exterior_spatial",
    "land",
    "statics",
    "lights",
    "npcs",
    "actor_models",
    "actor_model_parts",
    "movement_types",
    "race_movement_links",
    "race_animation_sources",
    "movement_game_settings",
    "waters",
    "texture_sets",
    "landscape_textures",
    "grass_types",
    "landscape_texture_grasses",
    "scripts",
    "script_properties",
    "script_property_values",
    "room_bounds",
    "room_portals",
    "room_links",
    "reference_rooms",
    "room_graph_stats",
    "projection_omission_stats",
    "projection_omission_samples",
    "cell_environment",
    "cell_regions",
    "cell_lighting",
    "worldspace_environment",
    "lighting_templates",
    "weathers",
    "weather_colors",
    "weather_directional_ambient",
    "climates",
    "climate_weathers",
    "region_weathers",
    "image_spaces",
    "reference_linked_refs",
    "reference_activate_parents",
    "reference_details",
    "reference_light_data",
    "door_links",
    "emittance",
];

/// Diagnostic timings do not participate in producer identity or asset reuse.
pub(crate) const DATABASE_PROFILE_FILE: &str = "inhouse-database-profile.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct DatabaseProfile {
    schema_version: u32,
    reader: String,
    counts: BTreeMap<String, u64>,
    elapsed_seconds: BTreeMap<String, f64>,
}

impl DatabaseProfile {
    fn new() -> Self {
        Self {
            schema_version: 1,
            reader: "inhouse".to_owned(),
            counts: BTreeMap::new(),
            elapsed_seconds: BTreeMap::new(),
        }
    }

    fn elapsed(&mut self, phase: &str, started: Instant) {
        self.elapsed_seconds
            .insert(phase.to_owned(), started.elapsed().as_secs_f64());
    }

    fn write(&self, output: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let path = output.join(DATABASE_PROFILE_FILE);
        // Resumed staging can share files with the previous pack through hard links.
        if path.is_file() {
            fs::remove_file(&path)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

/// Identity binds decoder behavior and schema bytes so stale output cannot prove reuse.
/// Localized names are read in Skyrim's default language.
pub fn reader_identity(reader: RecordReader) -> serde_json::Value {
    reader_identity_in_language(reader, crate::config::DEFAULT_LANGUAGE)
}

/// As [`reader_identity`], for names read from `language`'s string tables. English tables are
/// Windows-1252 like plugin text; other tables are UTF-8 with Windows-1252 fallback.
pub fn reader_identity_in_language(reader: RecordReader, language: &str) -> serde_json::Value {
    match reader {
        RecordReader::Legacy => {
            let mut identity = serde_json::json!({
                "mode": "legacy", "projection_version": LEGACY_PROJECTION_VERSION,
                "language": language, "text_encoding": "windows-1252",
            });
            if !is_english(language) {
                identity["string_table_encoding"] = serde_json::json!("utf-8-or-windows-1252");
            }
            identity
        }
        RecordReader::Inhouse => {
            let mut identity = serde_json::json!({
                "mode": "inhouse", "adapter_version": ADAPTER_VERSION,
                "schema_sha256": crate::cache::hash_bytes(records::SCHEMA_BYTES),
                "diagnostics_version": records::diagnostics::SCHEMA_VERSION,
                "language": language,
                "text_encoding": "windows-1252", "vmad_text_encoding": "utf-8",
            });
            if !is_english(language) {
                identity["string_table_encoding"] = serde_json::json!("utf-8-or-windows-1252");
            }
            identity
        }
    }
}

/// Skyrim's English string tables, the language its own `sLanguage` defaults to.
pub(crate) fn is_english(language: &str) -> bool {
    language.eq_ignore_ascii_case(crate::config::DEFAULT_LANGUAGE)
}

/// Bind the ordered add-on bytes to metadata without changing asset settings proofs.
pub fn reader_identity_with_schemas(
    reader: RecordReader,
    schemas: &records::addons::RuntimeSchemas,
) -> serde_json::Value {
    reader_identity_with_schemas_in_language(reader, schemas, crate::config::DEFAULT_LANGUAGE)
}

/// The identity a configuration's database is produced with: its reader, schema add-ons and
/// string-table language.
pub(crate) fn configured_reader_identity(
    config: &crate::config::PipelineConfig,
) -> Result<serde_json::Value> {
    Ok(reader_identity_with_schemas_in_language(
        config.record_reader,
        config.schemas()?.as_ref(),
        &config.language,
    ))
}

/// As [`reader_identity_with_schemas`], for names read from `language`'s string tables.
pub fn reader_identity_with_schemas_in_language(
    reader: RecordReader,
    schemas: &records::addons::RuntimeSchemas,
    language: &str,
) -> serde_json::Value {
    let mut identity = reader_identity_in_language(reader, language);
    if reader == RecordReader::Inhouse && !schemas.identities().is_empty() {
        identity["schema_sha256"] = serde_json::json!(schemas.schema_sha256());
        identity["schema_addons"] = serde_json::json!(schemas.identities());
    }
    identity
}

/// Export a fresh diagnostic record bundle without converting meshes, textures or scripts.
/// Inputs stay read-only; retained STRINGS extraction is evidence workspace data.
pub fn export_record_bundle(data: &Path, plugins_file: &Path, output: &Path) -> Result<usize> {
    export_record_bundle_typed(data, plugins_file, output, &[])
}

/// Include ordered typed family fields and their localized lookup evidence in JSONL.
pub fn export_record_bundle_typed(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
) -> Result<usize> {
    export_record_bundle_typed_with_database_work_dir(data, plugins_file, output, signatures, None)
}

/// Optionally build SQLite on a separate local disk while retaining all working evidence.
/// Typed JSONL, extracted strings and final caches remain in the requested output directory.
pub fn export_record_bundle_typed_with_database_work_dir(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
    database_work_dir: Option<&Path>,
) -> Result<usize> {
    export_record_bundle_typed_with_options(
        data,
        plugins_file,
        output,
        signatures,
        database_work_dir,
        RecordBundleMode::Full,
    )
}

/// Fresh diagnostic-only archive option; source decoding, canonical archives and caches stay complete.
pub fn export_record_bundle_typed_with_options(
    data: &Path,
    plugins_file: &Path,
    output: &Path,
    signatures: &[[u8; 4]],
    database_work_dir: Option<&Path>,
    mode: RecordBundleMode,
) -> Result<usize> {
    let bundle_stage = records::profile::ProfileStage::start("complete_record_bundle");
    let config = crate::config::PipelineConfig::new(data, output);
    config.validate()?;
    let output_location = fresh_record_bundle_location(output)?;
    let work_location = database_work_dir
        .map(fresh_record_bundle_location)
        .transpose()?;
    let separate_work = work_location
        .as_deref()
        .filter(|work| *work != output_location.as_path());
    if let Some(work) = separate_work {
        crate::config::PipelineConfig::new(data, work).validate()?;
        color_eyre::eyre::ensure!(
            !work.starts_with(&output_location) && !output_location.starts_with(work),
            "database work directory must be separate from the record bundle output"
        );
    }
    let plugins = crate::esm::read_plugins_txt(plugins_file, data)?;
    fs::create_dir(output)?;
    if mode == RecordBundleMode::RecordArchiveOnly {
        write_record_archive_scope(output, false, signatures)?;
    }
    if let Some(work) = separate_work {
        // Exclusive creation: failures retain the new output; existing paths are never reused.
        fs::create_dir(work)?;
    }
    let database_root = separate_work.unwrap_or(output);
    let database_path = database_root.join("skyrim_world.db");
    let strings_root = output.join(".strings-input");
    fs::create_dir(&strings_root)?;
    let paths = localized_string_paths(&plugins, crate::config::DEFAULT_LANGUAGE);
    let mut archives = fs::read_dir(data)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("bsa") || extension.eq_ignore_ascii_case("ba2")
            })
        })
        .filter(|path| crate::pipeline::archive_load_order_priority(path, &plugins).is_some())
        .collect::<Vec<_>>();
    crate::pipeline::sort_archives_by_load_order(&mut archives, &plugins);
    let strings_stage = records::profile::ProfileStage::start("localized_archive_inputs");
    for archive in archives {
        if let Err(error) =
            crate::archive::ArchiveExtractor::extract_paths(&archive, &strings_root, &paths)
        {
            eprintln!(
                "warning: {}: unable to extract localized names: {error}",
                archive.display()
            );
        }
    }
    strings_stage.finish(plugins.len());
    drop(paths);
    let records = convert_plugins_with_dump(
        &plugins,
        &database_path,
        &strings_root,
        (!signatures.is_empty()).then_some((output, signatures)),
        mode,
    )?;
    drop(plugins);
    let terrain_stage = records::profile::ProfileStage::start("terrain_cache_pair");
    let count = write_terrain_caches(&records, &database_path, output)?;
    terrain_stage.finish(records.len());
    if let Some(work) = separate_work {
        let copy_stage = records::profile::ProfileStage::start("database_work_dir_publish_copy");
        // Conversion has returned and its SQLite connection is closed; caches read the same DB.
        copy_completed_record_database(work, output)?;
        copy_stage.finish(2);
    }
    write_reader_identity(output, RecordReader::Inhouse)?;
    fs::write(output.join("records-schema.json"), records::SCHEMA_BYTES)?;
    let record_count = records.len();
    let runtime_release_stage = records::profile::ProfileStage::start("runtime_master_release");
    drop(records);
    runtime_release_stage.finish(record_count);
    if mode == RecordBundleMode::RecordArchiveOnly {
        let mut identity = reader_identity(RecordReader::Inhouse);
        identity["bundle_mode"] = serde_json::json!("record_archive_only");
        identity["runtime_ready"] = serde_json::json!(false);
        fs::write(
            output.join("record-reader.json"),
            serde_json::to_vec_pretty(&identity)?,
        )?;
        write_record_archive_scope(output, true, signatures)?;
    }
    bundle_stage.finish(record_count);
    Ok(count)
}

/// Declare incomplete/completed diagnostic scope without claiming runtime readiness.
fn write_record_archive_scope(output: &Path, complete: bool, signatures: &[[u8; 4]]) -> Result<()> {
    // This manifest is owned by this invocation in its exclusively created fresh output.
    let typed_signatures = signatures
        .iter()
        .map(|tag| String::from_utf8_lossy(tag).into_owned())
        .collect::<Vec<_>>();
    fs::write(
        output.join("record-bundle-scope.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "inhouse-diagnostic-record-bundle-scope-v1",
            "mode": "record_archive_only",
            "status": if complete { "COMPLETE_DIAGNOSTIC_ARCHIVE_RUNTIME_VIEWS_DEFERRED" } else { "INCOMPLETE" },
            "runtime_ready": false,
            "runtime_views_materialized": false,
            "preserved_sql_tables": ["schema_info", "plugins", "records", "formid_map",
                "inhouse_source_records", "inhouse_localized_fields", "inhouse_terrain_source_order",
                "inhouse_terrain_layers", "inhouse_unresolved_terrain_textures", "record_overrides", "inhouse_plugin_masters", "inhouse_schema_addons", "record_diagnostics"],
        "deferred_runtime_tables": DEFERRED_RUNTIME_TABLES,
        "unchanged_auxiliary_tables": ["conversion_cache", "lod", "lod_chunks", "lod_build",
            "lod_chunks_spatial", "lod_chunks_spatial_node", "lod_chunks_spatial_parent", "lod_chunks_spatial_rowid"],
            "preserved_cache_files": ["cell_cache.rkyv", "cell_cache_preserved.rkyv"],
            "typed_signatures": typed_signatures,
            "typed_selection_changed": false,
            "asset_conversion": false,
            "no_lod": true,
            "whole_runtime_SQL_equivalence_claim": false,
        }))?,
    )?;
    Ok(())
}

/// Resolve a fresh leaf through its existing parent, rejecting files, directories and symlinks.
/// Like the existing create_dir output contract, the parent must already exist.
fn fresh_record_bundle_location(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => color_eyre::eyre::bail!("record bundle output/work directory must be new"),
    }
    let name = path.file_name().ok_or_else(|| {
        color_eyre::eyre::eyre!("output/work directory must have a new leaf name")
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Exclusive, bounded copy: failures retain both work artifacts and any partial destination.
/// No replacement, delete, rename or journal-policy change is performed.
fn copy_completed_record_database(work: &Path, output: &Path) -> Result<()> {
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let source = work.join(name);
        color_eyre::eyre::ensure!(
            fs::symlink_metadata(&source)?.file_type().is_file(),
            "completed database artifact must be a regular file"
        );
    }
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let mut source = fs::File::open(work.join(name))?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(name))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let size = std::io::Read::read(&mut source, &mut buffer)?;
            if size == 0 {
                break;
            }
            destination.write_all(&buffer[..size])?;
        }
        destination.flush()?;
        destination.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "database_work_dir_tests.rs"]
mod database_work_dir_tests;

#[cfg(test)]
#[path = "inhouse_review_tests.rs"]
mod inhouse_review_tests;

#[cfg(test)]
#[path = "inhouse_determinism_tests.rs"]
mod inhouse_determinism_tests;

/// Stamp generated pack metadata after database/cache regeneration.
pub(crate) fn write_reader_identity(staging: &Path, reader: RecordReader) -> Result<()> {
    let schemas = records::addons::RuntimeSchemas::from_addon_bytes(&[])
        .map_err(color_eyre::eyre::Error::msg)?;
    write_reader_identity_value(staging, &reader_identity_with_schemas(reader, &schemas))
}

/// Publish the same immutable schema provenance and string-table language used by the decoder
/// and history.
pub(crate) fn write_configured_reader_identity(
    staging: &Path,
    config: &crate::config::PipelineConfig,
) -> Result<()> {
    write_reader_identity_value(staging, &configured_reader_identity(config)?)
}

/// Replace `record-reader.json` with `identity`.
fn write_reader_identity_value(staging: &Path, identity: &serde_json::Value) -> Result<()> {
    let path = staging.join("record-reader.json");
    if path.is_file() {
        fs::remove_file(&path)?;
    }
    fs::write(path, serde_json::to_vec_pretty(identity)?)?;
    Ok(())
}

/// The localized-name tables in `language` needed by the selected plugins.
pub(crate) fn localized_string_paths(plugins: &[PathBuf], language: &str) -> BTreeSet<PathBuf> {
    plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .flat_map(|stem| {
            ["strings", "dlstrings", "ilstrings"].map(|bank| {
                PathBuf::from(format!(
                    "strings/{}_{}.{bank}",
                    super::load_order::normalize_plugin_name(&stem.to_string_lossy()),
                    super::load_order::normalize_plugin_name(language)
                ))
            })
        })
        .collect()
}

/// Bounded summaries accompany disk-backed details for every adapter observation.
#[derive(Default)]
struct Warnings {
    summary: BTreeMap<String, (u64, String)>,
    summary_keys: BTreeMap<String, records::DiagnosticSummaryKey>,
    notices: BTreeMap<String, (u64, String)>,
    notice_keys: BTreeMap<String, records::DiagnosticSummaryKey>,
    logs: BTreeMap<String, records::diagnostics::EventLog>,
}

impl Warnings {
    /// Journal each issue and retain the smallest bounded provenance example per plugin.
    fn skipped(
        &mut self,
        plugin: &str,
        context: records::diagnostics::EventContext,
        message: String,
        readable_table: bool,
    ) {
        self.logs
            .entry(plugin.to_owned())
            .or_default()
            .append(&context, "adapter", &message);
        let is_notice = readable_table
            && context.stage == "localization"
            && context.action == Some("unresolved");
        let key = records::DiagnosticSummaryKey::new(&context, "adapter", &message);
        if is_notice {
            let entry = self.notices.entry(plugin.to_owned()).or_default();
            entry.0 += 1;
            if self
                .notice_keys
                .get(plugin)
                .is_none_or(|chosen| key < *chosen)
            {
                self.notice_keys.insert(plugin.to_owned(), key);
                entry.1 = records::diagnostics::bounded_summary(message.clone());
            }
        } else {
            let entry = self.summary.entry(plugin.to_owned()).or_default();
            entry.0 += 1;
            if self
                .summary_keys
                .get(plugin)
                .is_none_or(|chosen| key < *chosen)
            {
                self.summary_keys.insert(plugin.to_owned(), key);
                entry.1 = records::diagnostics::bounded_summary(message.clone());
            }
        }
    }

    /// Print and return deterministic plugin summaries after usable neighbors are exported.
    fn report(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.summary.len());
        for (plugin, (count, first)) in &self.summary {
            let line = format!(
                "warning: {plugin}: inhouse adapter reported {count} local decode/lookup issues (first: {first})"
            );
            eprintln!("{line}");
            lines.push(line);
        }
        lines
    }

    /// Print faithful missing-ID outcomes without affecting completeness.
    fn report_notices(&self) -> Vec<String> {
        self.notices
            .iter()
            .map(|(plugin, (count, first))| {
                let line = format!("notice: {plugin}: {count} localized IDs missing from readable string tables (first: {first})");
                eprintln!("{line}");
                line
            })
            .collect()
    }
}

/// Identify an adapter observation without retaining another payload or typed record.
fn adapter_context(
    record: &records::DecodedRecord,
    stage: &'static str,
    field: Option<String>,
    action: &'static str,
) -> records::diagnostics::EventContext {
    records::diagnostics::EventContext {
        source_record_offset: Some(record.source_record_offset),
        source_form_id: Some(record.source_form_id),
        form_id: Some(record.form_id),
        record_type: Some(record.record_type),
        stage,
        field,
        action: Some(action),
        ..Default::default()
    }
}

/// A localized occurrence keeps its original key separately from safe runtime text.
struct LocalizedField {
    form_id: u32,
    field_index: usize,
    signature: [u8; 4],
    field_name: String,
    string_table: &'static str,
    string_id: u32,
    load_order: u32,
    resolved_text: Option<String>,
    status: &'static str,
}

/// Recover winning LAND physical order from this reader's published provenance.
pub(crate) fn terrain_source_offsets(db_path: &Path) -> Result<HashMap<u32, u64>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement =
        conn.prepare("SELECT form_id,source_record_offset FROM inhouse_terrain_source_order")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// Read every normalized native layer and remapped source key, including excess runtime layers.
pub(crate) fn terrain_preserved_layers(
    db_path: &Path,
) -> Result<HashMap<u32, Vec<shared::TerrainLayer>>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare("SELECT form_id,layer_data FROM inhouse_terrain_layers")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut output = HashMap::new();
    for row in rows {
        let (id, bytes) = row?;
        output.insert(
            id,
            rkyv::from_bytes::<Vec<shared::TerrainLayer>, rkyv::rancor::Error>(&bytes)?,
        );
    }
    Ok(output)
}

/// Clear unresolved texture keys without dropping their layers or blend weights.
/// The runtime cache writer applies its capacity after receiving this complete projection.
pub(crate) fn terrain_runtime_layers(
    db_path: &Path,
    preserved: &HashMap<u32, Vec<shared::TerrainLayer>>,
) -> Result<HashMap<u32, Vec<shared::TerrainLayer>>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare("SELECT form_id FROM records WHERE record_type='LTEX'")?;
    let textures = statement
        .query_map([], |row| row.get::<_, u32>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let mut output = preserved.clone();
    for layers in output.values_mut() {
        for layer in layers {
            if layer.texture_form_id != 0 && !textures.contains(&layer.texture_form_id) {
                layer.texture_form_id = 0;
            }
        }
    }
    Ok(output)
}

/// Write the counters the database itself proves and the reader's phase timings. Terrain writers
/// then merge their own timings into the same file, so restores keep one producer profile.
pub(crate) fn write_database_profile(
    output: &Path,
    db_path: &Path,
    phases: &BTreeMap<String, f64>,
) -> Result<()> {
    let conn = Connection::open(db_path)?;
    let count = |sql: &str| -> Result<u64> { Ok(conn.query_row(sql, [], |row| row.get(0))?) };
    let mut profile = DatabaseProfile::new();
    profile.counts.insert(
        "decoded_winners".into(),
        count("SELECT count(*) FROM inhouse_source_records")?,
    );
    profile.counts.insert(
        "runtime_winners".into(),
        count("SELECT count(*) FROM records")?,
    );
    profile.counts.insert(
        "localized_fields".into(),
        count("SELECT count(*) FROM inhouse_localized_fields")?,
    );
    profile.counts.insert(
        "source_payload_bytes".into(),
        count("SELECT coalesce(sum(length(payload)),0) FROM inhouse_source_records")?,
    );
    profile.elapsed_seconds = phases.clone();
    profile.write(output)
}

/// Share source-order and layer preservation across conversion, diagnostics and metadata rebuild.
pub(crate) fn write_terrain_caches(
    records: &HashMap<u32, RawRecord>,
    db_path: &Path,
    output: &Path,
) -> Result<usize> {
    let started = Instant::now();
    let profile_path = output.join(DATABASE_PROFILE_FILE);
    let mut profile = if profile_path.is_file() {
        serde_json::from_slice::<DatabaseProfile>(&fs::read(profile_path)?)?
    } else {
        DatabaseProfile::new()
    };
    let auxiliary_started = Instant::now();
    let offsets = terrain_source_offsets(db_path)?;
    let preserved = terrain_preserved_layers(db_path)?;
    let runtime = terrain_runtime_layers(db_path, &preserved)?;
    profile.elapsed("terrain_auxiliary_reads", auxiliary_started);
    let runtime_started = Instant::now();
    let count = crate::esm::cell_cache::write_cell_cache_with_preserved_layers(
        records,
        &offsets,
        &runtime,
        &output.join("cell_cache.rkyv"),
    )?;
    profile.elapsed("terrain_runtime_cache", runtime_started);
    let preserved_started = Instant::now();
    crate::esm::cell_cache::write_preserved_cell_cache_with_source_layers(
        records,
        &offsets,
        &preserved,
        &output.join("cell_cache_preserved.rkyv"),
    )?;
    profile.elapsed("terrain_preserved_cache", preserved_started);
    profile
        .counts
        .insert("terrain_cells".to_owned(), count as u64);
    profile.elapsed("total_terrain_caches", started);
    profile.write(output)?;
    Ok(count)
}

/// Decode and export with one shared load-order mapping; plugin data failures stay local.
#[cfg(test)]
pub(crate) fn convert_plugins(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_dump(
        plugin_paths,
        db_path,
        strings_root,
        None,
        RecordBundleMode::Full,
    )
}

/// Publish usable records with bounded recovery warnings and compatibility notices.
/// One authoritative text-source policy applies to candidate admission and publication.
pub(crate) fn convert_plugins_with_string_sources_report(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    string_sources: StringSources<'_>,
    schemas: Option<&records::addons::RuntimeSchemas>,
) -> Result<RecordConversion> {
    convert_plugins_with_text_sources_report_impl(
        plugin_paths,
        db_path,
        string_sources,
        None,
        RecordBundleMode::Full,
        schemas,
        #[cfg(test)]
        None,
    )
}

/// Diagnostic export shares the exact decoder, winner and text lookup paths with publication.
fn convert_plugins_with_dump(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_dump_and_schemas(plugin_paths, db_path, strings_root, dump, mode, None)
}

/// Preserve legacy entry points while threading one schema snapshot through all passes.
fn convert_plugins_with_dump_and_schemas(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
    schemas: Option<&records::addons::RuntimeSchemas>,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_dump_impl(
        plugin_paths,
        db_path,
        strings_root,
        dump,
        mode,
        schemas,
        #[cfg(test)]
        None,
    )
}

/// Keep synthetic schema selection private to tests of the real publication path.
fn convert_plugins_with_dump_impl(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    strings_root: &Path,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
    schemas: Option<&records::addons::RuntimeSchemas>,
    #[cfg(test)] test_schema: Option<&records::schema_format::Schema>,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_text_sources_impl(
        plugin_paths,
        db_path,
        StringSources::default_language(StringRoots::PluginLooseFirst(strings_root)),
        dump,
        mode,
        schemas,
        #[cfg(test)]
        test_schema,
    )
}

/// Preserve map-returning diagnostic/test entry points while sharing warning collection.
fn convert_plugins_with_text_sources_impl(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    string_sources: StringSources<'_>,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
    schemas: Option<&records::addons::RuntimeSchemas>,
    #[cfg(test)] test_schema: Option<&records::schema_format::Schema>,
) -> Result<HashMap<u32, RawRecord>> {
    convert_plugins_with_text_sources_report_impl(
        plugin_paths,
        db_path,
        string_sources,
        dump,
        mode,
        schemas,
        #[cfg(test)]
        test_schema,
    )
    .map(|conversion| conversion.records)
}

/// Return published records and summaries by severity without changing recovery semantics.
fn convert_plugins_with_text_sources_report_impl(
    plugin_paths: &[PathBuf],
    db_path: &Path,
    string_sources: StringSources<'_>,
    dump: Option<(&Path, &[[u8; 4]])>,
    mode: RecordBundleMode,
    schemas: Option<&records::addons::RuntimeSchemas>,
    #[cfg(test)] test_schema: Option<&records::schema_format::Schema>,
) -> Result<RecordConversion> {
    let builtin;
    let schemas = match schemas {
        Some(schemas) => schemas,
        None => {
            builtin = records::addons::RuntimeSchemas::from_addon_bytes(&[])
                .map_err(color_eyre::eyre::Error::msg)?;
            &builtin
        }
    };
    let total_started = Instant::now();
    let mut phases = BTreeMap::<String, f64>::new();
    let order_stage = records::profile::ProfileStage::start("read_load_order");
    let order = LoadOrder::read(plugin_paths)?;
    order_stage.finish(plugin_paths.len());
    let mut warnings = Warnings::default();
    let prepared = records::prepare_runtime_schema(schemas);
    let mut tables = HashMap::new();
    let validate = |record: &records::DecodedRecord| {
        validate_candidate_with_string_sources(
            record,
            &prepared,
            plugin_paths,
            string_sources,
            &mut tables,
        )
    };
    let mut preserved_layers = HashMap::new();
    let preserve = |accepted: &records::ReadResult| {
        for record in accepted
            .records
            .values()
            .filter(|record| record.record_type == *b"LAND")
        {
            match crate::esm::cell_cache::preserved_texture_layers(&record.to_raw_record()) {
                Ok(layers) => {
                    preserved_layers.insert(record.form_id, layers);
                }
                Err(error) => warnings.skipped(
                    &order.names[record.load_order as usize],
                    adapter_context(
                        record,
                        "terrain_provenance",
                        Some("texture_layers".into()),
                        "skipped",
                    ),
                    format!("{:08X} terrain layer provenance: {error}", record.form_id),
                    false,
                ),
            }
        }
    };
    #[cfg(test)]
    let mut decoded = if let Some(schema) = test_schema {
        records::read_plugins_with_validation_and_observer_with_schema(
            plugin_paths,
            &order,
            schema,
            validate,
            preserve,
        )?
    } else {
        records::read_plugins_with_runtime_schemas(
            plugin_paths,
            &order,
            schemas,
            validate,
            preserve,
        )?
    };
    #[cfg(not(test))]
    let mut decoded = records::read_plugins_with_runtime_schemas(
        plugin_paths,
        &order,
        schemas,
        validate,
        preserve,
    )?;
    // Winner validation and the observer have finished; physical history and
    // winning source payloads stay alive for the established provenance SQL pass.
    release_reader_metadata(&mut decoded);
    phases.insert(
        "reader_decode_merge_deferred".into(),
        total_started.elapsed().as_secs_f64(),
    );
    let mut localized_fields = Vec::new();
    let mut unresolved_fields = HashMap::new();
    let mut typed_output = dump
        .map(|(output, _)| {
            fs::File::create(output.join("typed-records.jsonl"))
                .map(|file| BufWriter::with_capacity(1024 * 1024, file))
        })
        .transpose()?;
    let projection_stage = records::profile::ProfileStage::start("typed_export_runtime_projection");
    let mut master = HashMap::with_capacity(decoded.records.len());
    let mut opaque_fields = HashMap::new();
    // Malformed FNAM/XRDS are rejected before export; remember them so the light status columns
    // match the legacy reader's "malformed" instead of reading "absent".
    let mut rejected_light_fields = RejectedLightFields::new();
    // Sort compact keys, leaving record payloads in place and streaming one JSON line at a time.
    // The same traversal fixes localized provenance rowids when no typed dump is requested.
    let mut record_keys = decoded
        .records
        .values()
        .map(|record| (record.record_type, record.form_id))
        .collect::<Vec<_>>();
    record_keys.sort_unstable();
    for (_, id) in record_keys {
        let record = decoded
            .records
            .get_mut(&id)
            .expect("sorted winning record exists");
        let priority = record.load_order as usize;
        let plugin = &order.names[priority];
        let mut runtime_text_updates = decoded_text_updates(record);
        let mut unresolved = BTreeSet::new();
        let mut localized = Vec::new();
        let mut localization = BTreeMap::new();
        for (index, field) in record.fields.iter().enumerate() {
            // Unsupported records retain their pre-existing runtime compatibility path.
            if record.supported && field.opaque {
                opaque_fields
                    .entry(record.form_id)
                    .or_insert_with(BTreeSet::new)
                    .insert(index);
                unresolved.insert(index);
            }
            if let Value::LocalizedString(id) = &field.value {
                let bank = localized_bank(&prepared, record.record_type, field);
                let mut table = (*id != 0).then(|| {
                    runtime_text_table(
                        &mut tables,
                        priority,
                        &plugin_paths[priority],
                        string_sources,
                        bank,
                    )
                });
                if let Some(table) = table.as_deref_mut()
                    && !table.reported
                {
                    if let Some(error) = &table.error {
                        warnings.skipped(
                            plugin,
                            records::diagnostics::EventContext {
                                stage: "localization_bank",
                                field: Some(bank.to_owned()),
                                action: Some("unresolved"),
                                ..Default::default()
                            },
                            format!("localized table: {error}"),
                            false,
                        );
                    }
                    for malformed_id in &table.malformed_ids {
                        warnings.skipped(
                            plugin,
                            records::diagnostics::EventContext {
                                stage: "localization_bank_entry",
                                field: Some(bank.to_owned()),
                                action: Some("malformed"),
                                ..Default::default()
                            },
                            format!("malformed {bank} entry for localized ID {malformed_id}"),
                            false,
                        );
                    }
                    table.reported = true;
                }
                // Zero is a null key even if a malformed table gives it text.
                let resolved = table
                    .as_ref()
                    .and_then(|table| localized_text(*id, &table.values));
                let status = if *id == 0 {
                    "null_id"
                } else if resolved.is_some() {
                    "resolved"
                } else {
                    "missing"
                };
                localization.insert(
                    index,
                    serde_json::json!({"bank": bank, "id": id, "text": resolved, "status": status}),
                );
                localized.push(LocalizedField {
                    form_id: record.form_id,
                    field_index: index,
                    signature: field.signature,
                    field_name: field.name.clone(),
                    string_table: bank,
                    string_id: *id,
                    load_order: record.load_order,
                    resolved_text: resolved.map(str::to_owned),
                    status,
                });
                // Keep the occurrence and key; unresolved text is never invented.
                if let Some(text) = resolved {
                    runtime_text_updates.push((index, runtime_string(text)));
                } else {
                    runtime_text_updates.push((index, runtime_string("")));
                    unresolved.insert(index);
                    if *id != 0 {
                        let malformed = table
                            .as_ref()
                            .is_some_and(|table| table.malformed_ids.contains(id));
                        let readable = table
                            .as_ref()
                            .is_some_and(|table| table.error.is_none() && !malformed);
                        let message = if malformed {
                            format!(
                                "{:08X} {} references malformed {bank} entry for localized ID {id}",
                                record.form_id, field.name
                            )
                        } else {
                            format!(
                                "{:08X} {} missing localized ID {id}",
                                record.form_id, field.name
                            )
                        };
                        warnings.skipped(
                            plugin,
                            adapter_context(
                                record,
                                "localization",
                                Some(format!(
                                    "{}[canonical:{index}].{}",
                                    String::from_utf8_lossy(&field.signature),
                                    field.name
                                )),
                                "unresolved",
                            ),
                            message,
                            readable,
                        );
                    }
                }
            }
        }
        if let Some(writer) = &mut typed_output
            && dump.is_some_and(|(_, signatures)| signatures.contains(&record.record_type))
        {
            records::typed_export::write_record(writer, record, &localization)?;
            writer.write_all(b"\n")?;
        }
        let rejected_light = RejectedLightInputs {
            fnam: record.rejected_fields.contains(b"FNAM"),
            xrds: record.rejected_fields.contains(b"XRDS"),
        };
        if rejected_light != RejectedLightInputs::default() {
            rejected_light_fields.insert(record.form_id, rejected_light);
        }
        // Typed fields and the source digest have been written before ownership moves.
        let mut raw = take_canonical_record(record);
        for (index, text) in runtime_text_updates {
            raw.subrecords[index].1 = text;
        }
        let omitted = opaque_fields.get(&raw.form_id);
        // Admission already validated these shared text/opaque transformations.
        // This defensive guard is exceptional under the current validators; future
        // catalogue-dependent projection rules need their own admission/recovery policy.
        let invalid = with_filtered_record(&mut raw, omitted, validate_runtime_record);
        if let Err(error) = invalid {
            warnings.skipped(
                plugin,
                adapter_context(record, "runtime_projection", None, "skipped"),
                format!(
                    "{:08X} {}: {error}",
                    raw.form_id,
                    String::from_utf8_lossy(&raw.record_type)
                ),
                false,
            );
            continue;
        }
        if !unresolved.is_empty() {
            unresolved_fields.insert(raw.form_id, unresolved);
        }
        localized_fields.extend(localized);
        master.insert(raw.form_id, raw);
    }
    if let Some(writer) = &mut typed_output {
        writer.flush()?;
    }
    projection_stage.finish(master.len());
    // Runtime replacements and localized provenance now own their text. The
    // completed typed writer and string-table cache have no remaining consumers.
    drop(tables);
    drop(typed_output);
    let setup_stage = records::profile::ProfileStage::start("database_setup_plugin_hashes");
    let conn = Connection::open(db_path)?;
    // Keep a modest page-cache target for this conversion's private connection.
    // Negative cache_size is in KiB; journal, sync and spill policies stay intact.
    conn.pragma_update(Some("main"), "cache_size", -65_536)?;
    conn.set_prepared_statement_cache_capacity(64);
    create_tables(&conn)?;
    let terrain_offsets = decoded
        .records
        .values()
        .filter(|record| record.record_type == *b"LAND" && master.contains_key(&record.form_id))
        .map(|record| (record.form_id, record.source_record_offset))
        .collect::<HashMap<_, _>>();
    setup_stage.finish(plugin_paths.len());
    let runtime_started = Instant::now();
    let runtime_stage = records::profile::ProfileStage::start(if mode == RecordBundleMode::Full {
        "runtime_database_export"
    } else {
        "canonical_record_database_export"
    });
    if mode == RecordBundleMode::Full {
        export_inhouse_to_db(
            &conn,
            &master,
            &order,
            plugin_paths,
            &unresolved_fields,
            &terrain_offsets,
            Some(&rejected_light_fields),
        )?;
    } else {
        export_inhouse_record_archive_to_db(&conn, &master, &order, plugin_paths)?;
    }
    runtime_stage.finish(master.len());
    phases.insert(
        "runtime_database_export".into(),
        runtime_started.elapsed().as_secs_f64(),
    );
    drop(unresolved_fields);
    // Canonical SQL retains every opaque occurrence; caches see only runtime fields.
    for (id, omitted) in &opaque_fields {
        if let Some(raw) = master.get_mut(id) {
            let mut index = 0;
            raw.subrecords.retain(|_| {
                let keep = !omitted.contains(&index);
                index += 1;
                keep
            });
        }
    }
    // Keep the established rkyv records.data contract for runtime tools.
    // Auxiliary provenance holds verbatim source framing and file-relative IDs.
    let provenance_stage = records::profile::ProfileStage::start("source_database_export");
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS inhouse_source_records(form_id INTEGER PRIMARY KEY, source_form_id INTEGER NOT NULL, load_order INTEGER NOT NULL, record_type TEXT NOT NULL, flags INTEGER NOT NULL, payload BLOB NOT NULL); DELETE FROM inhouse_source_records;")?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS inhouse_localized_fields(form_id INTEGER NOT NULL, field_index INTEGER NOT NULL, signature TEXT NOT NULL, field_name TEXT NOT NULL, string_table TEXT NOT NULL, string_id INTEGER NOT NULL, load_order INTEGER NOT NULL, resolved_text TEXT, status TEXT NOT NULL CHECK(status IN ('resolved','null_id','missing')), PRIMARY KEY(form_id,field_index)); DELETE FROM inhouse_localized_fields; CREATE TABLE IF NOT EXISTS inhouse_terrain_source_order(form_id INTEGER PRIMARY KEY, source_record_offset INTEGER NOT NULL); DELETE FROM inhouse_terrain_source_order;")?;
    super::inhouse_terrain_diagnostics::replace_snapshot_tables(&tx)?;
    for (id, layers) in preserved_layers {
        let Some(record) = master.get(&id) else {
            continue;
        };
        let unresolved = layers
            .iter()
            .map(|layer| layer.texture_form_id)
            .filter(|id| {
                *id != 0
                    && !master
                        .get(id)
                        .is_some_and(|record| record.record_type == *b"LTEX")
            })
            .collect::<BTreeSet<_>>();
        tx.prepare_cached(
            "INSERT INTO inhouse_terrain_layers(form_id,cell_id,load_order,layer_data) VALUES (?1,?2,?3,?4)")?.execute(
            params![id, record.cell_form_id.unwrap_or(id), record.load_order, rkyv::to_bytes::<rkyv::rancor::Error>(&layers)?.as_slice()],
        )?;
        super::inhouse_terrain_diagnostics::write_unresolved_texture_ids(&tx, id, &unresolved)?;
    }
    for field in localized_fields {
        tx.prepare_cached(
            "INSERT INTO inhouse_localized_fields(form_id,field_index,signature,field_name,string_table,string_id,load_order,resolved_text,status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)")?.execute(
            params![field.form_id, field.field_index as u64, String::from_utf8_lossy(&field.signature), field.field_name, field.string_table, field.string_id, field.load_order, field.resolved_text, field.status],
        )?;
    }
    for (id, offset) in terrain_offsets {
        tx.prepare_cached(
            "INSERT INTO inhouse_terrain_source_order(form_id,source_record_offset) VALUES (?1,?2)",
        )?
        .execute(params![id, offset])?;
    }
    let source_count = decoded.records.len();
    let source_records_started = Instant::now();
    write_source_records(&tx, &mut decoded.records)?;
    phases.insert(
        "provenance_source_records".into(),
        source_records_started.elapsed().as_secs_f64(),
    );
    records::history::write_tables_with_schemas(
        &tx,
        &decoded.history,
        &order,
        plugin_paths,
        schemas,
    )?;
    records::diagnostics::write_tables(
        &tx,
        order
            .names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                decoded
                    .diagnostics
                    .get(name)
                    .map(|diagnostics| (index, &diagnostics.event_log))
            })
            .chain(
                order
                    .names
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| warnings.logs.get(name).map(|log| (index, log))),
            ),
    )?;
    tx.commit()?;
    provenance_stage.finish(source_count);
    let diagnostics_started = Instant::now();
    if let Some(staging) = db_path.parent() {
        let path = staging.join("inhouse-reader-diagnostics.json");
        if path.is_file() {
            fs::remove_file(&path)?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "decoder": decoded.diagnostics,
                "adapter": warnings.summary,
                "adapter_notices": warnings.notices,
            }))?,
        )?;
    }
    let mut report_warnings = decoded
        .diagnostics
        .iter()
        .flat_map(|(name, diagnostics)| diagnostics.warning_summary_lines(name))
        .collect::<Vec<_>>();
    let mut report_notices = decoded
        .diagnostics
        .iter()
        .flat_map(|(name, diagnostics)| diagnostics.notice_summary_lines(name))
        .collect::<Vec<_>>();
    let release_stage = records::profile::ProfileStage::start("decoded_release");
    drop(decoded);
    release_stage.finish(master.len());
    report_warnings.extend(warnings.report());
    report_notices.extend(warnings.report_notices());
    phases.insert(
        "reader_target_validation_and_diagnostics".into(),
        diagnostics_started.elapsed().as_secs_f64(),
    );
    phases.insert("total_reader".into(), total_started.elapsed().as_secs_f64());
    Ok(RecordConversion {
        phases,
        records: master,
        warnings: report_warnings,
        notices: report_notices,
    })
}

/// Free reader-only header/override lookups after winners and observation are finalized.
/// Physical history, raw winning payloads and diagnostics remain for their SQL pass.
fn release_reader_metadata(decoded: &mut records::ReadResult) {
    drop(std::mem::take(&mut decoded.headers));
    drop(std::mem::take(&mut decoded.overrides));
}

/// Existing publication regressions retain the ordinary loose-directory lookup policy.
#[cfg(test)]
fn validate_candidate(
    record: &records::DecodedRecord,
    prepared: &records::PreparedSchemas,
    plugin_paths: &[PathBuf],
    strings_root: &Path,
    tables: &mut HashMap<(usize, String), RuntimeTextTable>,
) -> std::result::Result<(), String> {
    validate_candidate_with_string_sources(
        record,
        prepared,
        plugin_paths,
        StringSources::default_language(StringRoots::PluginLooseFirst(strings_root)),
        tables,
    )
}

/// Validate the same runtime text and opaque selection before merge replaces a predecessor.
/// Localization failures retain the established empty-text policy and remain record-local.
fn validate_candidate_with_string_sources(
    record: &records::DecodedRecord,
    prepared: &records::PreparedSchemas,
    plugin_paths: &[PathBuf],
    string_sources: StringSources<'_>,
    tables: &mut HashMap<(usize, String), RuntimeTextTable>,
) -> std::result::Result<(), String> {
    if !matches!(&record.record_type, b"LAND" | b"MOVT" | b"GMST" | b"RACE") {
        return Ok(());
    }
    if record.record_type == *b"LAND" {
        if !record.payload_complete {
            return Err("incomplete terrain payload".into());
        }
        if let Some(signature) = record
            .rejected_fields
            .iter()
            .copied()
            .find(|signature| matches!(signature, b"VHGT" | b"BTXT" | b"ATXT" | b"VTXT"))
        {
            return Err(format!(
                "rejected authored terrain field {}",
                String::from_utf8_lossy(&signature)
            ));
        }
    }
    let mut raw = record.to_raw_record();
    for (index, text) in decoded_text_updates(record) {
        raw.subrecords[index].1 = text;
    }
    for (index, field) in record.fields.iter().enumerate() {
        if let Value::LocalizedString(id) = &field.value {
            let priority = record.load_order as usize;
            let bank = localized_bank(prepared, record.record_type, field);
            let table = if *id == 0 {
                None
            } else {
                Some(runtime_text_table(
                    tables,
                    priority,
                    &plugin_paths[priority],
                    string_sources,
                    bank,
                ))
            };
            raw.subrecords[index].1 = runtime_string(
                table
                    .as_ref()
                    .and_then(|table| localized_text(*id, &table.values))
                    .unwrap_or(""),
            );
        }
    }
    if record.supported {
        let mut index = 0;
        raw.subrecords.retain(|_| {
            let keep = !record.fields[index].opaque;
            index += 1;
            keep
        });
    }
    validate_runtime_record(&raw)
}

/// Keep candidate admission and the final defensive check on one validator contract.
fn validate_runtime_record(record: &RawRecord) -> std::result::Result<(), String> {
    validate_inhouse_record(record).map_err(|error| error.to_string())?;
    if record.record_type == *b"LAND" {
        validate_inhouse_land(record).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Publish every accepted reader source, including any exceptional final projection rejection.
/// History uses these rows for its winner catalogue; each payload can then be dropped.
fn write_source_records(
    conn: &Connection,
    records: &mut HashMap<u32, records::DecodedRecord>,
) -> Result<()> {
    let mut source_ids = records.keys().copied().collect::<Vec<_>>();
    source_ids.sort_unstable();
    for id in source_ids {
        let record = records.remove(&id).expect("sorted source record exists");
        conn.prepare_cached(
            "INSERT INTO inhouse_source_records(form_id,source_form_id,load_order,record_type,flags,payload) VALUES (?1,?2,?3,?4,?5,?6)")?.execute(
            params![id, record.source_form_id, record.load_order, String::from_utf8_lossy(&record.record_type), record.flags, record.raw_payload],
        )?;
    }
    Ok(())
}

/// One temporarily hidden occurrence retains its original position and exact owned buffers.
struct OmittedSubrecord {
    index: usize,
    signature: Vec<u8>,
    bytes: Vec<u8>,
}

/// Move only tuple ownership for validation; restoration also runs during unwinding.
struct RuntimeFieldSelection<'a> {
    record: &'a mut RawRecord,
    omitted: Vec<OmittedSubrecord>,
    original_len: usize,
}

impl<'a> RuntimeFieldSelection<'a> {
    /// Preserve selected byte allocations while temporarily hiding opaque occurrences.
    fn new(record: &'a mut RawRecord, omitted: &BTreeSet<usize>) -> Self {
        let source = std::mem::take(&mut record.subrecords);
        let original_len = source.len();
        let mut hidden = Vec::new();
        record.subrecords = Vec::with_capacity(original_len);
        for (index, (signature, bytes)) in source.into_iter().enumerate() {
            if omitted.contains(&index) {
                hidden.push(OmittedSubrecord {
                    index,
                    signature,
                    bytes,
                });
            } else {
                record.subrecords.push((signature, bytes));
            }
        }
        Self {
            record,
            omitted: hidden,
            original_len,
        }
    }
}

impl Drop for RuntimeFieldSelection<'_> {
    /// Restore the canonical occurrence order without copying any signature or payload.
    fn drop(&mut self) {
        let mut selected = std::mem::take(&mut self.record.subrecords).into_iter();
        let mut hidden = std::mem::take(&mut self.omitted).into_iter().peekable();
        let mut restored = Vec::with_capacity(self.original_len);
        for index in 0..self.original_len {
            if hidden.peek().is_some_and(|field| field.index == index) {
                let field = hidden.next().expect("selected hidden occurrence");
                restored.push((field.signature, field.bytes));
            } else {
                restored.push(selected.next().expect("selected runtime occurrence"));
            }
        }
        self.record.subrecords = restored;
    }
}

/// Validate a runtime selection and restore canonical ownership before any result escapes.
fn with_filtered_record<T>(
    record: &mut RawRecord,
    omitted: Option<&BTreeSet<usize>>,
    validate: impl FnOnce(&RawRecord) -> T,
) -> T {
    let Some(omitted) = omitted.filter(|omitted| !omitted.is_empty()) else {
        return validate(record);
    };
    let selection = RuntimeFieldSelection::new(record, omitted);
    let result = validate(&*selection.record);
    drop(selection);
    result
}

/// Move canonical buffers only after selected typed serialization has completed.
/// Source payload, identity, provenance and rejection metadata survive for the existing SQL pass.
fn take_canonical_record(record: &mut records::DecodedRecord) -> RawRecord {
    let fields = std::mem::take(&mut record.fields);
    RawRecord {
        form_id: record.form_id,
        record_type: record.record_type,
        flags: record.flags,
        subrecords: fields
            .into_iter()
            .map(|field| (field.signature.to_vec(), field.canonical_bytes))
            .collect(),
        cell_form_id: record.cell_form_id,
        worldspace_form_id: record.worldspace_form_id,
        load_order: record.load_order,
    }
}

/// One lazily loaded string bank shared by candidate admission and final publication.
/// Deferred warning publication avoids reporting banks from discarded candidates twice.
struct RuntimeTextTable {
    values: HashMap<u32, String>,
    error: Option<String>,
    malformed_ids: BTreeSet<u32>,
    reported: bool,
}

#[derive(Debug)]
pub(crate) struct ParsedTextTable {
    pub(crate) values: HashMap<u32, String>,
    pub(crate) malformed_ids: BTreeSet<u32>,
}

/// Load the same authoritative loose/VFS snapshot once; an unreadable bank remains unresolved.
fn runtime_text_table<'a>(
    tables: &'a mut HashMap<(usize, String), RuntimeTextTable>,
    priority: usize,
    plugin: &Path,
    string_sources: StringSources<'_>,
    bank: &str,
) -> &'a mut RuntimeTextTable {
    tables
        .entry((priority, bank.to_owned()))
        .or_insert_with(|| {
            let (values, malformed_ids, error) =
                match read_plugin_strings(plugin, string_sources, bank) {
                    Ok(Some(table)) => (table.values, table.malformed_ids, None),
                    Ok(None) => (
                        HashMap::new(),
                        BTreeSet::new(),
                        Some(format!("missing {bank} string table")),
                    ),
                    Err(error) => (HashMap::new(), BTreeSet::new(), Some(error.to_string())),
                };
            RuntimeTextTable {
                values,
                error,
                malformed_ids,
                reported: false,
            }
        })
}

/// Share authoritative localized-bank selection across admission and publication.
fn localized_bank(
    prepared: &records::PreparedSchemas,
    record_type: [u8; 4],
    field: &records::DecodedField,
) -> &'static str {
    match field.localized_bank.as_deref() {
        Some("dlstrings") => "dlstrings",
        Some("ilstrings") => "ilstrings",
        Some("strings") => "strings",
        _ => records::string_table_with_prepared(
            prepared,
            &record_type,
            &field.signature,
            &field.name,
        ),
    }
}

/// A zero localized key stays null even when an invalid table gives it a value.
pub(crate) fn localized_text(id: u32, table: &HashMap<u32, String>) -> Option<&str> {
    if id == 0 {
        None
    } else {
        table.get(&id).map(String::as_str)
    }
}

/// Use the same decoded inline text in candidate validation and final publication.
/// Field indices follow both raw projections; localized keys use the shared bank lookup.
fn decoded_text_updates(record: &records::DecodedRecord) -> Vec<(usize, Vec<u8>)> {
    record
        .fields
        .iter()
        .enumerate()
        .filter_map(|(index, field)| {
            let Value::String(text) = &field.value else {
                return None;
            };
            runtime_text(text, &field.canonical_bytes).map(|bytes| (index, bytes))
        })
        .collect()
}

/// Encode decoded text for the shared runtime extractor without changing original source bytes.
/// Typed Windows-1252 decoding owns ordinary text; runtime bytes are UTF-8 with one NUL.
/// A fixed-width ASCII code stored without a terminator keeps its exact source width.
fn runtime_text(text: &str, source: &[u8]) -> Option<Vec<u8>> {
    (!(text.is_ascii() && source == text.as_bytes())).then(|| runtime_string(text))
}

/// Encode a resolved or normalized runtime string as UTF-8 followed by one NUL.
pub(crate) fn runtime_string(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// The localized bank one reader uses for a plugin: the loose winner first, then the archive VFS.
/// The database cache identity calls the same resolver, so both cannot choose different banks.
#[derive(Debug)]
pub(crate) struct LocalizedBank {
    pub(crate) origin: &'static str,
    pub(crate) path: PathBuf,
}

/// Resolve the authoritative bank for one plugin. A broken winning loose source remains
/// unresolved instead of falling back to losing text; ambiguous names fail closed.
pub(crate) fn resolve_localized_bank(
    string_sources: StringSources<'_>,
    plugin: &Path,
    bank: &str,
) -> Result<Option<LocalizedBank>> {
    let file_name = format!(
        "{}_{}.{bank}",
        plugin.file_stem().unwrap_or_default().to_string_lossy(),
        string_sources.language
    );
    match string_sources.roots {
        StringRoots::PluginLooseFirst(_) => {
            if let Some(folder) =
                unique_child(plugin.parent().unwrap_or(Path::new(".")), "strings")?
                && let Some(path) = unique_child(&folder, &file_name)?
            {
                return Ok(Some(LocalizedBank {
                    origin: "loose",
                    path,
                }));
            }
        }
        StringRoots::EffectiveData { loose_files, .. } => {
            // MO2 indexes direct Strings banks with the same plugin-derived Unicode key.
            // A single key retains layer precedence; scanning equivalent keys could select a loser.
            let key = super::load_order::normalize_plugin_name(&format!("strings/{file_name}"));
            if let Some(path) = loose_files.get(&key) {
                return Ok(Some(LocalizedBank {
                    origin: "loose",
                    path: path.clone(),
                }));
            }
        }
    }
    if let Some(folder) = unique_child(string_sources.roots.archive_root(), "strings")?
        && let Some(path) = unique_child(&folder, &file_name)?
    {
        return Ok(Some(LocalizedBank {
            origin: "archive",
            path,
        }));
    }
    Ok(None)
}

/// Read the resolved bank, or an empty table when no bank exists for this plugin.
pub(crate) fn read_plugin_strings(
    plugin: &Path,
    string_sources: StringSources<'_>,
    bank: &str,
) -> Result<Option<ParsedTextTable>> {
    match resolve_localized_bank(string_sources, plugin, bank)? {
        Some(source) => parse_text_table(
            &fs::read(source.path)?,
            bank != "strings",
            string_sources.language,
        )
        .map(Some),
        None => Ok(None),
    }
}

/// Resolve a single path component on both case-sensitive and Windows filesystems.
/// An exact child wins; otherwise one case-insensitive Unicode match is required.
fn unique_child(parent: &Path, name: &str) -> Result<Option<PathBuf>> {
    let direct = parent.join(name);
    if direct.exists() {
        return Ok(Some(direct));
    }
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let wanted = super::load_order::normalize_plugin_name(name);
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry?;
        if super::load_order::normalize_plugin_name(&entry.file_name().to_string_lossy()) == wanted
        {
            matches.push(entry.path());
        }
    }
    color_eyre::eyre::ensure!(matches.len() <= 1, "ambiguous localized string path {name}");
    Ok(matches.pop())
}

/// English string tables are Windows-1252, like plugin text, even where the bytes also form
/// valid UTF-8. Skyrim SE's tables in other languages are UTF-8 (Cyrillic, Polish); a string in
/// them that is not valid UTF-8 is read as Windows-1252.
fn decode_table_text(bytes: &[u8], language: &str) -> String {
    if is_english(language) {
        return records::decode_text(bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => records::decode_text(bytes),
    }
}

/// Read STRINGS offsets within their declared data region; one bad entry is skipped.
/// DLSTRINGS/ILSTRINGS directory offsets point to a checked length prefix followed by text.
fn parse_text_table(
    bytes: &[u8],
    length_prefixed: bool,
    language: &str,
) -> Result<ParsedTextTable> {
    let word = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .and_then(|data| data.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let count =
        word(0).ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS header"))? as usize;
    let size = word(4).ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS header"))? as usize;
    let data_start = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(8))
        .ok_or_else(|| color_eyre::eyre::eyre!("STRINGS directory overflow"))?;
    let data_end = data_start
        .checked_add(size)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| color_eyre::eyre::eyre!("truncated STRINGS directory/data"))?;
    let data = &bytes[data_start..data_end];
    // The declared count is bounded by the table's own directory, but every
    // entry costs several times its eight directory bytes, so the map grows
    // from validated entries instead of reserving for the declared count.
    let mut strings = HashMap::new();
    let mut malformed_ids = BTreeSet::new();
    for index in 0..count {
        let id = word(8 + index * 8).expect("bounded directory");
        let offset = word(12 + index * 8).expect("bounded directory") as usize;
        let tail = data.get(offset..);
        let tail = if length_prefixed {
            tail.and_then(|tail| {
                let size = u32::from_le_bytes(tail.get(..4)?.try_into().ok()?) as usize;
                tail.get(4..4usize.checked_add(size)?)
            })
        } else {
            tail
        };
        if let Some(tail) = tail
            && let Some(end) = tail.iter().position(|byte| *byte == 0)
        {
            strings.insert(id, decode_table_text(&tail[..end], language));
        } else {
            malformed_ids.insert(id);
        }
    }
    Ok(ParsedTextTable {
        values: strings,
        malformed_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A four-byte event code without a terminator keeps its width; padded and accented
    /// text become canonical UTF-8 with one NUL.
    #[test]
    fn runtime_text_keeps_unterminated_codes_and_canonicalizes_other_text() {
        assert_eq!(runtime_text("ADIA", b"ADIA"), None);
        assert_eq!(runtime_text("Bool", b"Bool\0\0"), Some(b"Bool\0".to_vec()));
        assert_eq!(
            runtime_text("Caf\u{e9}", b"Caf\xe9\0"),
            Some("Caf\u{e9}\0".as_bytes().to_vec())
        );
    }

    /// A broken length-prefixed entry cannot consume its valid table neighbor.
    #[test]
    fn dlstrings_and_ilstrings_bound_each_entry() {
        let mut table = Vec::new();
        table.extend(2u32.to_le_bytes());
        table.extend(12u32.to_le_bytes());
        table.extend(41u32.to_le_bytes());
        table.extend(0u32.to_le_bytes());
        table.extend(42u32.to_le_bytes());
        table.extend(4u32.to_le_bytes());
        table.extend(u32::MAX.to_le_bytes());
        table.extend(4u32.to_le_bytes());
        table.extend(b"yes\0");
        let decoded = parse_text_table(&table, true, crate::config::DEFAULT_LANGUAGE).unwrap();
        assert_eq!(decoded.values.len(), 1);
        assert_eq!(decoded.values[&42], "yes");
        assert!(!decoded.values.contains_key(&41));
        assert_eq!(decoded.malformed_ids, BTreeSet::from([41]));
        table[4..8].copy_from_slice(&13u32.to_le_bytes());
        assert!(parse_text_table(&table, true, crate::config::DEFAULT_LANGUAGE).is_err());
    }

    /// The declared count sizes no reservation: a table whose entries are all
    /// malformed returns a map that never allocated for them.
    #[test]
    fn text_table_capacity_grows_only_from_validated_entries() {
        let count = 4096u32;
        let mut table = Vec::new();
        table.extend(count.to_le_bytes());
        table.extend(0u32.to_le_bytes());
        for index in 0..count {
            table.extend(index.to_le_bytes());
            table.extend(u32::MAX.to_le_bytes());
        }
        let decoded = parse_text_table(&table, false, crate::config::DEFAULT_LANGUAGE).unwrap();
        assert!(decoded.values.is_empty());
        assert_eq!(decoded.malformed_ids.len() as u32, count);
        assert!(
            decoded.values.capacity() < count as usize,
            "an empty table reserved {} slots for {count} malformed entries",
            decoded.values.capacity()
        );
    }

    proptest! {
        #![proptest_config(crate::test_strategies::config(256))]

        #[test]
        fn arbitrary_text_table_bytes_never_panic(
            bytes in crate::test_strategies::arbitrary_bytes(512),
        ) {
            for length_prefixed in [false, true] {
                let _ = parse_text_table(&bytes, length_prefixed, crate::config::DEFAULT_LANGUAGE);
            }
        }
    }
}

#[cfg(test)]
mod runtime_move_tests {
    use super::*;
    use crate::records::{DecodedField, DecodedRecord};

    /// Keep independent identity, payload and nested-value sentinels for ownership assertions.
    fn record() -> DecodedRecord {
        DecodedRecord {
            form_id: 0x0100_1234,
            source_form_id: 0x0200_5678,
            record_type: *b"INFO",
            flags: 0x4000,
            version_control: 0x1234_5678,
            form_version: 44,
            header_unknown: 19,
            load_order: 7,
            cell_form_id: Some(91),
            worldspace_form_id: Some(92),
            topic_form_id: Some(93),
            source_topic_form_id: Some(0x0200_0001),
            topic_group_offset: Some(129),
            source_record_offset: 257,
            fields: vec![
                DecodedField {
                    opaque: false,
                    localized_bank: None,
                    signature: *b"DATA",
                    name: "first".into(),
                    value: Value::Bytes(vec![9, 9]),
                    canonical_bytes: vec![1, 2, 3],
                },
                DecodedField {
                    opaque: false,
                    localized_bank: None,
                    signature: *b"DATA",
                    name: "second".into(),
                    value: Value::Struct(vec![(
                        "nested".into(),
                        Value::Array(vec![Value::Float(-0.0), Value::Unsigned(u64::MAX)]),
                    )]),
                    canonical_bytes: vec![4, 5],
                },
                DecodedField {
                    opaque: false,
                    localized_bank: None,
                    signature: *b"FULL",
                    name: "localized".into(),
                    value: Value::LocalizedString(100),
                    canonical_bytes: 100u32.to_le_bytes().to_vec(),
                },
            ],
            rejected_fields: vec![*b"VMAD"],
            payload_complete: false,
            raw_payload: b"source".to_vec(),
            supported: false,
            deferred_fields: Vec::new(),
        }
    }

    /// Hide invalid opaque links around valid repeated signatures and asymmetric payloads.
    fn opaque_race() -> RawRecord {
        RawRecord {
            form_id: 0x0100_1234,
            record_type: *b"RACE",
            flags: 0x4000,
            subrecords: vec![
                (b"WKMV".to_vec(), vec![0x19, 0x73, 0xa5]),
                (b"EDID".to_vec(), b"RetainedRace\0".to_vec()),
                (b"RNMV".to_vec(), vec![0x27, 0x91]),
                (b"WKMV".to_vec(), 0x0003_580du32.to_le_bytes().to_vec()),
                (b"RNMV".to_vec(), 0u32.to_le_bytes().to_vec()),
                (b"ZZZZ".to_vec(), vec![0x35, 0xb7, 0x59, 0xd1, 0x43]),
            ],
            cell_form_id: Some(91),
            worldspace_form_id: Some(92),
            load_order: 7,
        }
    }

    /// Runtime selection borrows retained allocations and restores all canonical occurrences.
    #[test]
    fn runtime_selection_restores_exact_buffers_after_success_and_rejection() {
        let mut raw = opaque_race();
        let expected = raw.clone();
        let canonical = crate::esm::extractors::serialize_subrecords(&raw.subrecords);
        let pointers = raw
            .subrecords
            .iter()
            .map(|(signature, bytes)| (signature.as_ptr(), bytes.as_ptr()))
            .collect::<Vec<_>>();
        assert!(validate_inhouse_record(&raw).is_err());
        let omitted = BTreeSet::from([0usize, 2, 5, 99]);
        let result = with_filtered_record(&mut raw, Some(&omitted), |runtime| {
            assert_eq!(runtime.subrecords.len(), 3);
            for (selected, original) in [1usize, 3, 4].into_iter().enumerate() {
                assert_eq!(runtime.subrecords[selected], expected.subrecords[original]);
                assert_eq!(
                    runtime.subrecords[selected].0.as_ptr(),
                    pointers[original].0
                );
                assert_eq!(
                    runtime.subrecords[selected].1.as_ptr(),
                    pointers[original].1
                );
            }
            validate_inhouse_record(runtime)
        });
        result.unwrap();
        assert_eq!(format!("{raw:?}"), format!("{expected:?}"));
        assert_eq!(raw.subrecords, expected.subrecords);
        assert_eq!(
            crate::esm::extractors::serialize_subrecords(&raw.subrecords),
            canonical
        );
        for (index, (signature, bytes)) in raw.subrecords.iter().enumerate() {
            assert_eq!((signature.as_ptr(), bytes.as_ptr()), pointers[index]);
        }
        let rejected = with_filtered_record(
            &mut raw,
            Some(&BTreeSet::from([5usize])),
            validate_inhouse_record,
        );
        assert!(rejected.unwrap_err().to_string().contains("WKMV"));
        assert_eq!(format!("{raw:?}"), format!("{expected:?}"));
        assert_eq!(raw.subrecords, expected.subrecords);
        assert_eq!(
            crate::esm::extractors::serialize_subrecords(&raw.subrecords),
            canonical
        );
        for (index, (signature, bytes)) in raw.subrecords.iter().enumerate() {
            assert_eq!((signature.as_ptr(), bytes.as_ptr()), pointers[index]);
        }
        let empty = BTreeSet::new();
        let tuple_pointer = raw.subrecords.as_ptr();
        with_filtered_record(&mut raw, Some(&empty), |runtime| {
            assert_eq!(runtime.subrecords.as_ptr(), tuple_pointer);
        });
        assert_eq!(raw.subrecords.as_ptr(), tuple_pointer);
    }

    /// The ownership guard restores bytes if validation unwinds before returning a result.
    #[test]
    fn runtime_selection_restores_exact_buffers_during_unwind() {
        let mut raw = opaque_race();
        let expected = raw.clone();
        let canonical = crate::esm::extractors::serialize_subrecords(&raw.subrecords);
        let pointers = raw
            .subrecords
            .iter()
            .map(|(_, bytes)| bytes.as_ptr())
            .collect::<Vec<_>>();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_filtered_record(&mut raw, Some(&BTreeSet::from([0usize, 2, 5])), |_| {
                panic!("authored validation unwind");
            });
        }));
        assert!(panicked.is_err());
        assert_eq!(format!("{raw:?}"), format!("{expected:?}"));
        assert_eq!(raw.subrecords, expected.subrecords);
        assert_eq!(
            crate::esm::extractors::serialize_subrecords(&raw.subrecords),
            canonical
        );
        for (index, (_, bytes)) in raw.subrecords.iter().enumerate() {
            assert_eq!(bytes.as_ptr(), pointers[index]);
        }
    }

    /// Source insertion drains payload ownership while publishing every retained reader row.
    #[test]
    fn source_sql_drains_payloads_without_changing_retained_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE inhouse_source_records(form_id INTEGER PRIMARY KEY, source_form_id INTEGER NOT NULL, load_order INTEGER NOT NULL, record_type TEXT NOT NULL, flags INTEGER NOT NULL, payload BLOB NOT NULL);").unwrap();
        let mut later = record();
        let earlier_id = later.form_id;
        let mut earlier = later.clone();
        later.form_id += 17;
        later.source_form_id += 31;
        later.load_order = 11;
        later.raw_payload = vec![0x27, 0x91, 0x43];
        earlier.raw_payload = vec![0x19, 0x73, 0xb5, 0x57];
        let expected = [earlier.clone(), later.clone()];
        let mut decoded = records::ReadResult {
            records: HashMap::from([(later.form_id, later), (earlier.form_id, earlier)]),
            ..Default::default()
        };
        write_source_records(&conn, &mut decoded.records).unwrap();
        assert!(decoded.records.is_empty());
        for original in expected {
            let (source, priority, tag, flags, payload): (u32, u32, String, u32, Vec<u8>) = conn.query_row(
                "SELECT source_form_id,load_order,record_type,flags,payload FROM inhouse_source_records WHERE form_id=?1",
                [original.form_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).unwrap();
            assert_eq!(
                (source, priority, tag, flags, payload),
                (
                    original.source_form_id,
                    original.load_order,
                    "INFO".into(),
                    original.flags,
                    original.raw_payload
                )
            );
        }
        let ids = conn
            .prepare("SELECT form_id FROM inhouse_source_records ORDER BY form_id")
            .unwrap()
            .query_map([], |row| row.get::<_, u32>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, [earlier_id, earlier_id + 17]);
    }

    /// Frame own subrecords without using production encoding or schema helpers.
    fn source_subrecord(tag: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
        [
            tag.as_slice(),
            &u16::try_from(bytes.len()).unwrap().to_le_bytes(),
            bytes,
        ]
        .concat()
    }

    /// Preserve independent header sentinels for exact source-offset/raw-history assertions.
    fn source_frame(tag: &[u8; 4], id: u32, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = tag.to_vec();
        bytes.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
        bytes.extend(flags.to_le_bytes());
        bytes.extend(id.to_le_bytes());
        bytes.extend(0x1973_b557u32.to_le_bytes());
        bytes.extend(44u16.to_le_bytes());
        bytes.extend(0x2791u16.to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    /// Exercise projection policy with a private non-native localized WKMV descriptor.
    /// Current public add-ons cannot replace RACE.WKMV's built-in FormID descriptor.
    fn projection_admission_fallback_case(deletion_barrier: bool, extra_rejection: bool) {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("output");
        let strings = data.join("Strings");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        fs::create_dir(&strings).unwrap();
        let schema = records::schema_format::Schema::parse(
            br#"{
            "version":1,"records":[
                {"signature":"RACE","fields":[
                    {"signature":"EDID","name":"editor_id","kind":"zstring"},
                    {"signature":"WKMV","name":"synthetic_localized_link","kind":"lstring"}
                ]},
                {"signature":"STAT","fields":[
                    {"signature":"EDID","name":"editor_id","kind":"zstring"}
                ]}
            ]}"#,
        )
        .unwrap();
        let header = |master: bool, localized: bool| {
            let mut payload = source_subrecord(b"HEDR", &[0; 12]);
            if master {
                payload.extend(source_subrecord(b"MAST", b"Base.esm\0"));
                payload.extend(source_subrecord(b"DATA", &[0; 8]));
            }
            source_frame(b"TES4", 0, if localized { 0x80 } else { 0 }, &payload)
        };
        let source_id = 0x1600u32;
        let runtime_id = 0x0100_1600u32;
        let prior_payload = [
            source_subrecord(b"EDID", b"UsableLocalizedPredecessor\0"),
            source_subrecord(b"WKMV", &8u32.to_le_bytes()),
        ]
        .concat();
        let rejected_payload = [
            source_subrecord(b"EDID", b"RejectedLocalizedOverride\0"),
            source_subrecord(b"WKMV", &17u32.to_le_bytes()),
        ]
        .concat();
        let prior_header = header(false, true);
        let prior_offset = prior_header.len() as u64;
        let prior_frame = source_frame(b"RACE", source_id, 0, &prior_payload);
        let rejected_header = header(true, true);
        let rejected_offset = rejected_header.len() as u64;
        let rejected_frame = source_frame(b"RACE", source_id, 0, &rejected_payload);
        let mut originals = vec![
            (data.join("Bootstrap.esm"), header(false, false)),
            (
                data.join("Base.esm"),
                [prior_header, prior_frame.clone()].concat(),
            ),
        ];
        if deletion_barrier {
            originals.push((
                data.join("Delete.esp"),
                [
                    header(true, false),
                    source_frame(b"RACE", source_id, 0x20, &[]),
                ]
                .concat(),
            ));
        }
        let intermediate = extra_rejection.then(|| {
            let payload = [
                source_subrecord(b"EDID", b"AnotherRejectedLocalizedOverride\0"),
                source_subrecord(b"WKMV", &31u32.to_le_bytes()),
            ]
            .concat();
            let header = header(true, true);
            let offset = header.len() as u64;
            let frame = source_frame(b"RACE", source_id, 0, &payload);
            let priority = originals.len() as u32;
            originals.push((
                data.join("Intermediate.esp"),
                [header, frame.clone()].concat(),
            ));
            (priority, offset, frame, payload)
        });
        let rejected_priority = originals.len() as u32;
        let neighbor_id = (rejected_priority << 24) | 0x1700;
        originals.push((
            data.join("Override.esp"),
            [
                rejected_header,
                rejected_frame.clone(),
                source_frame(
                    b"STAT",
                    0x0100_1700,
                    0,
                    &source_subrecord(b"EDID", b"UsableAfterRejectedOverride\0"),
                ),
            ]
            .concat(),
        ));
        for (path, bytes) in &originals {
            fs::write(path, bytes).unwrap();
        }
        // Independent STRINGS framing makes the earlier key exactly four runtime bytes.
        // The later key is absent and projects to one NUL rather than a valid link width.
        let table = [
            1u32.to_le_bytes().as_slice(),
            4u32.to_le_bytes().as_slice(),
            8u32.to_le_bytes().as_slice(),
            0u32.to_le_bytes().as_slice(),
            b"Q7z\0",
        ]
        .concat();
        let table_path = strings.join("Base_english.strings");
        fs::write(&table_path, &table).unwrap();
        let paths = originals
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let database = output.join("skyrim_world.db");
        let master = convert_plugins_with_dump_impl(
            &paths,
            &database,
            &data,
            Some((&output, &[*b"RACE", *b"STAT"])),
            RecordBundleMode::Full,
            None,
            Some(&schema),
        )
        .unwrap();
        assert!(master.contains_key(&neighbor_id));
        assert_eq!(master.len(), if deletion_barrier { 1 } else { 2 });
        assert_eq!(master.contains_key(&runtime_id), !deletion_barrier);
        let conn = Connection::open(&database).unwrap();
        let source_rows: u32 = conn
            .query_row(
                "SELECT count(*) FROM inhouse_source_records WHERE form_id=?1",
                [runtime_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_rows, u32::from(!deletion_barrier));
        if !deletion_barrier {
            let race = &master[&runtime_id];
            assert_eq!(race.load_order, 1);
            assert_eq!(
                race.subrecords
                    .iter()
                    .find(|(tag, _)| tag == b"WKMV")
                    .map(|(_, bytes)| bytes.as_slice()),
                Some(b"Q7z\0".as_slice())
            );
            let source: (u32, u32, Vec<u8>) = conn
                .query_row(
                    "SELECT source_form_id,load_order,payload FROM inhouse_source_records WHERE form_id=?1",
                    [runtime_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(source, (source_id, 1, prior_payload.clone()));
            let localized: (u32, u32, String) = conn
                .query_row(
                    "SELECT string_id,load_order,resolved_text FROM inhouse_localized_fields WHERE form_id=?1",
                    [runtime_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(localized, (8, 1, "Q7z".into()));
        }
        let versions = conn
            .prepare("SELECT plugin_id,status,reason FROM record_overrides WHERE form_id=?1 ORDER BY plugin_id")
            .unwrap()
            .query_map([runtime_id], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let expected_versions = 2 + usize::from(deletion_barrier) + usize::from(extra_rejection);
        assert_eq!(versions.len(), expected_versions);
        assert_eq!(versions[0], (1, "accepted".into(), None));
        if deletion_barrier {
            assert_eq!(versions[1], (2, "accepted".into(), None));
        }
        if let Some((priority, _, _, _)) = &intermediate {
            let row = versions
                .iter()
                .find(|(plugin, _, _)| plugin == priority)
                .unwrap();
            assert_eq!(row.1, "skipped");
            assert!(row.2.as_deref().unwrap().contains("RACE WKMV length"));
        }
        let rejected = versions.last().unwrap();
        assert_eq!(rejected.0, rejected_priority);
        assert_eq!(rejected.1, "skipped");
        let reason = rejected.2.as_deref().unwrap();
        assert!(reason.contains("unusable runtime candidate"));
        assert!(reason.contains("RACE WKMV length"));
        drop(conn);
        let typed = fs::read_to_string(output.join("typed-records.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(typed.len(), if deletion_barrier { 1 } else { 2 });
        if !deletion_barrier {
            let row = typed
                .iter()
                .find(|row| row["form_id"] == runtime_id)
                .unwrap();
            assert_eq!(row["load_order"], 1);
            assert_eq!(
                row["source_payload_sha256"],
                crate::cache::hash_bytes(&prior_payload)
            );
            assert_eq!(
                row["fields"][1]["value"],
                serde_json::json!({"LocalizedString":8})
            );
        }
        let pin = crate::cache::hash_file(&database).unwrap();
        let mut visited = 0;
        let summary = records::history_source::visit_source_history(
            &output,
            None,
            records::history_source::SourceQuery::Source {
                original_owner: "Base.esm".into(),
                full_local: source_id,
            },
            |version| {
                assert_eq!(version.runtime_form_id, Some(runtime_id));
                let raw = version.raw.unwrap();
                if version.plugin_id == 1 {
                    assert_eq!(version.status, "accepted");
                    assert_eq!(version.source_record_offset, prior_offset);
                    assert_eq!(version.accepted_source_winner, !deletion_barrier);
                    assert_eq!(version.runtime_record_present, !deletion_barrier);
                    assert_eq!(
                        version.runtime_fallback_before_latest_source,
                        !deletion_barrier
                    );
                    assert_eq!(raw.header.as_slice(), &prior_frame[..24]);
                    assert_eq!(
                        raw.stored_payload.as_deref(),
                        Some(prior_payload.as_slice())
                    );
                } else if version.plugin_id == rejected_priority {
                    assert_eq!(version.status, "skipped");
                    assert_eq!(version.source_record_offset, rejected_offset);
                    assert!(!version.accepted_source_winner);
                    assert!(!version.runtime_record_present);
                    assert_eq!(raw.header.as_slice(), &rejected_frame[..24]);
                    assert_eq!(
                        raw.stored_payload.as_deref(),
                        Some(rejected_payload.as_slice())
                    );
                } else if let Some((priority, offset, frame, payload)) = &intermediate
                    && version.plugin_id == *priority
                {
                    assert_eq!(version.status, "skipped");
                    assert_eq!(version.source_record_offset, *offset);
                    assert!(!version.accepted_source_winner);
                    assert!(!version.runtime_record_present);
                    assert_eq!(raw.header.as_slice(), &frame[..24]);
                    assert_eq!(raw.stored_payload.as_deref(), Some(payload.as_slice()));
                } else {
                    assert!(deletion_barrier);
                    assert_eq!(version.flags & 0x20, 0x20);
                    assert!(!version.accepted_source_winner);
                    assert!(!version.runtime_record_present);
                }
                visited += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(visited, expected_versions);
        assert!(summary.fully_raw_available);
        assert_eq!(crate::cache::hash_file(&database).unwrap(), pin);
        for (path, bytes) in originals {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(fs::read(table_path).unwrap(), table);
    }

    /// A projection-unusable override keeps the exact prior source, typed value and neighbor.
    #[test]
    fn projection_admission_restores_predecessor_source_typed_history_and_neighbor() {
        projection_admission_fallback_case(false, false);
    }

    /// A valid deletion prevents projection recovery from resurrecting an earlier definition.
    #[test]
    fn projection_admission_respects_deletion_barrier_and_keeps_neighbor() {
        projection_admission_fallback_case(true, false);
    }

    /// Consecutive projection rejections keep the same usable source and every raw version.
    #[test]
    fn projection_admission_skips_multiple_rejected_overrides_without_losing_predecessor() {
        projection_admission_fallback_case(false, true);
    }

    /// A projection-unusable first definition has no winner rows but retains exact raw history.
    /// This private schema is a projection-policy witness, not a native RACE layout claim.
    #[test]
    fn projection_rejection_omits_winner_rows_but_keeps_raw_history_and_neighbor() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("output");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        let schema = records::schema_format::Schema::parse(
            br#"{
            "version":1,"records":[
                {"signature":"RACE","fields":[
                    {"signature":"EDID","name":"editor_id","kind":"zstring"},
                    {"signature":"WKMV","name":"synthetic_localized_link","kind":"lstring"}
                ]},
                {"signature":"STAT","fields":[
                    {"signature":"EDID","name":"editor_id","kind":"zstring"}
                ]}
            ]}"#,
        )
        .unwrap();
        let race_id = 0x1600u32;
        let neighbor_id = 0x1700u32;
        let race_payload = [
            source_subrecord(b"EDID", b"RetainedRejectedRace\0"),
            source_subrecord(b"WKMV", &17u32.to_le_bytes()),
        ]
        .concat();
        let header = source_frame(b"TES4", 0, 0x80, &source_subrecord(b"HEDR", &[0; 12]));
        let race_offset = header.len() as u64;
        let race_frame = source_frame(b"RACE", race_id, 0, &race_payload);
        let neighbor_payload = source_subrecord(b"EDID", b"UsableAfterRejectedRace\0");
        let original = [
            header,
            race_frame.clone(),
            source_frame(b"STAT", neighbor_id, 0, &neighbor_payload),
        ]
        .concat();
        let plugin = data.join("Synthetic.esm");
        fs::write(&plugin, &original).unwrap();
        let database = output.join("skyrim_world.db");
        let master = convert_plugins_with_dump_impl(
            std::slice::from_ref(&plugin),
            &database,
            &data,
            Some((&output, &[*b"RACE", *b"STAT"])),
            RecordBundleMode::Full,
            None,
            Some(&schema),
        )
        .unwrap();
        assert_eq!(master.len(), 1);
        assert!(master.contains_key(&neighbor_id));
        assert!(!master.contains_key(&race_id));
        let conn = Connection::open(&database).unwrap();
        let source_rows: u32 = conn
            .query_row(
                "SELECT count(*) FROM inhouse_source_records WHERE form_id=?1",
                [race_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_rows, 0);
        let present: u32 = conn
            .query_row(
                "SELECT count(*) FROM records WHERE form_id=?1",
                [race_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 0);
        let rows: u32 = conn
            .query_row("SELECT count(*) FROM inhouse_source_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
        let accepted: (String, u64) = conn
            .query_row(
                "SELECT status,source_record_offset FROM record_overrides WHERE form_id=?1",
                [race_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(accepted, ("skipped".into(), race_offset));
        drop(conn);
        let typed = fs::read_to_string(output.join("typed-records.jsonl")).unwrap();
        let typed = typed
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0]["form_id"], neighbor_id);
        assert_eq!(
            typed[0]["source_payload_sha256"],
            crate::cache::hash_bytes(&neighbor_payload)
        );
        let database_pin = crate::cache::hash_file(&database).unwrap();
        let mut seen = 0;
        let summary = records::history_source::visit_source_history(
            &output,
            None,
            records::history_source::SourceQuery::Physical {
                plugin_id: 0,
                source_record_offset: race_offset,
            },
            |version| {
                assert_eq!(version.runtime_form_id, Some(race_id));
                assert_eq!(version.status, "skipped");
                assert!(!version.accepted_source_winner);
                assert!(!version.runtime_record_present);
                assert!(
                    version
                        .rejection_reason
                        .as_deref()
                        .unwrap()
                        .contains("RACE WKMV length")
                );
                let raw = version.raw.unwrap();
                assert_eq!(raw.header.as_slice(), &race_frame[..24]);
                assert_eq!(raw.stored_payload.as_deref(), Some(race_payload.as_slice()));
                assert_eq!(
                    raw.stored_payload_sha256,
                    Some(crate::cache::hash_bytes(&race_payload))
                );
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, 1);
        assert!(summary.fully_raw_available);
        assert_eq!(crate::cache::hash_file(&database).unwrap(), database_pin);
        assert_eq!(fs::read(&plugin).unwrap(), original);
    }

    /// Releasing lookup capacity retains raw winners, physical rejection history and diagnostics.
    #[test]
    fn reader_metadata_release_preserves_raw_history_and_diagnostics() {
        let winner = record();
        let id = winner.form_id;
        let history = records::history::HistoryEntry {
            form_id: Some(id),
            plugin_index: 7,
            source_form_id: 0x0200_5678,
            record_type: *b"INFO",
            flags: 0x4000,
            source_record_offset: 257,
            source_record_size: 6,
            source_group_end: 1025,
            frame_complete: true,
            accepted: false,
            reason: Some("authored rejected predecessor".into()),
            source_cell_form_id: Some(91),
            source_worldspace_form_id: Some(92),
            source_topic_form_id: Some(0x0200_0001),
            topic_group_offset: Some(129),
            editor_id: Some("authored identity".into()),
        };
        let mut decoded = records::ReadResult {
            records: HashMap::from([(id, winner.clone())]),
            headers: vec![winner],
            overrides: HashMap::from([(
                id,
                vec![records::RecordSource {
                    plugin_index: 3,
                    source_form_id: 0x0100_1234,
                    deleted: true,
                }],
            )]),
            history: vec![history],
            diagnostics: BTreeMap::from([(
                "authored plugin".into(),
                records::PluginDiagnostics {
                    skipped_records: 13,
                    skipped_fields: 17,
                    compatibility_headers: 19,
                    first_example: Some("authored diagnostic".into()),
                    ..Default::default()
                },
            )]),
        };
        let raw_pointer = decoded.records[&id].raw_payload.as_ptr();
        let history_pointer = decoded.history.as_ptr();
        let record_before = serde_json::to_vec(&decoded.records[&id]).unwrap();
        let history_before = format!("{:?}", decoded.history);
        let diagnostics_before = serde_json::to_vec(&decoded.diagnostics).unwrap();
        assert!(decoded.headers.capacity() > 0 && decoded.overrides.capacity() > 0);
        release_reader_metadata(&mut decoded);
        assert_eq!(decoded.headers.capacity(), 0);
        assert_eq!(decoded.overrides.capacity(), 0);
        assert!(decoded.headers.is_empty() && decoded.overrides.is_empty());
        assert_eq!(decoded.records[&id].raw_payload.as_ptr(), raw_pointer);
        assert_eq!(decoded.records[&id].raw_payload, b"source");
        assert_eq!(
            serde_json::to_vec(&decoded.records[&id]).unwrap(),
            record_before
        );
        assert_eq!(decoded.history.as_ptr(), history_pointer);
        assert_eq!(format!("{:?}", decoded.history), history_before);
        assert_eq!(
            serde_json::to_vec(&decoded.diagnostics).unwrap(),
            diagnostics_before
        );
    }

    #[test]
    fn consuming_projection_matches_borrowed_oracle_but_moves_exact_ordered_buffers() {
        let mut decoded = record();
        let oracle = decoded.to_raw_record();
        let pointers = decoded
            .fields
            .iter()
            .map(|f| f.canonical_bytes.as_ptr())
            .collect::<Vec<_>>();
        let before = decoded.clone();
        let raw = take_canonical_record(&mut decoded);
        assert_eq!(raw.subrecords, oracle.subrecords);
        assert_eq!(
            raw.subrecords,
            vec![
                (b"DATA".to_vec(), vec![1, 2, 3]),
                (b"DATA".to_vec(), vec![4, 5]),
                (b"FULL".to_vec(), vec![100, 0, 0, 0])
            ]
        );
        assert_eq!(
            (
                raw.form_id,
                raw.record_type,
                raw.flags,
                raw.cell_form_id,
                raw.worldspace_form_id,
                raw.load_order
            ),
            (0x0100_1234, *b"INFO", 0x4000, Some(91), Some(92), 7)
        );
        for (index, ptr) in pointers.into_iter().enumerate() {
            assert_eq!(raw.subrecords[index].1.as_ptr(), ptr);
        }
        assert!(decoded.fields.is_empty());
        let mut expected_survivors = before;
        expected_survivors.fields.clear();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(expected_survivors).unwrap()
        );
        assert_eq!(decoded.raw_payload, b"source");
    }

    #[test]
    fn typed_localized_key_and_digest_are_written_before_runtime_replacement() {
        let mut decoded = record();
        let localization = BTreeMap::from([(
            2usize,
            serde_json::json!({"bank":"strings","id":100,"text":"different UTF8 runtime","status":"resolved"}),
        )]);
        let mut wire = Vec::new();
        records::typed_export::write_record(&mut wire, &decoded, &localization).unwrap();
        let typed: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            typed["fields"][2]["value"],
            serde_json::json!({"LocalizedString":100})
        );
        assert_eq!(
            typed["fields"][2]["canonical_bytes"],
            serde_json::json!([100, 0, 0, 0])
        );
        assert_eq!(
            typed["source_payload_sha256"],
            "41cf6794ba4200b839c53531555f0f3998df4cbb01a4d5cb0b94e3ca5e23947d"
        );
        let mut runtime = take_canonical_record(&mut decoded);
        runtime.subrecords[2].1 = runtime_string("different UTF8 runtime");
        assert_ne!(runtime.subrecords[2].1, vec![100, 0, 0, 0]);
        assert_eq!(decoded.raw_payload, b"source");
        assert_eq!(
            typed["fields"][2]["canonical_bytes"],
            serde_json::json!([100, 0, 0, 0])
        );
    }

    #[test]
    fn empty_fields_still_preserve_source_and_runtime_header_metadata() {
        let mut decoded = record();
        decoded.fields.clear();
        let raw = take_canonical_record(&mut decoded);
        assert!(raw.subrecords.is_empty());
        assert_eq!(raw.form_id, 0x0100_1234);
        assert_eq!(raw.load_order, 7);
        assert_eq!(decoded.source_form_id, 0x0200_5678);
        assert_eq!(decoded.raw_payload, b"source");
        assert_eq!(decoded.rejected_fields, vec![*b"VMAD"]);
        assert!(!decoded.payload_complete);
    }
}

#[cfg(test)]
mod history_reuse_private_controls {
    use super::convert_plugins;
    use crate::records::history_source::{SourceQuery, visit_source_history};
    use rusqlite::{Connection, OpenFlags};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// Independent own native main-header bytes, including unequal padding sentinels.
    fn native_frame(tag: &[u8; 4], raw_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend((payload.len() as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(raw_id.to_le_bytes());
        out.extend(0x18764231u32.to_le_bytes());
        out.extend(44u16.to_le_bytes());
        out.extend(0x2113u16.to_le_bytes());
        out.extend(payload);
        out
    }

    /// Independent own subrecord bytes; no production framing helper supplies expected bytes.
    fn native_subrecord(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend((payload.len() as u16).to_le_bytes());
        out.extend(payload);
        out
    }

    /// Write one independent full plugin with no MAST and native self index zero.
    fn independent_plugin(
        data: &Path,
        name: &str,
        local: u32,
        edid: &[u8],
    ) -> (PathBuf, Vec<u8>, Vec<u8>) {
        let payload = native_subrecord(b"EDID", edid);
        let record = native_frame(b"STAT", local, &payload);
        let mut original = native_frame(b"TES4", 0, &native_subrecord(b"HEDR", &[0; 12]));
        original.extend(&record);
        let path = data.join(name);
        fs::write(&path, original).unwrap();
        (path, payload, record[..24].to_vec())
    }

    /// Repeat real in-house setup/export against one DB, then prove new snapshot source identity.
    /// The old live history FK fails at the second conversion's plugin replacements.
    #[test]
    fn reused_inhouse_database_reorders_plugins_and_refreshes_source_history() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("Data");
        let output = root.path().join("output");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        let (a, a_payload, a_header) = independent_plugin(&data, "A.esm", 0x1800, b"different_a\0");
        let (b, b_payload, b_header) = independent_plugin(&data, "B.esm", 0x1900, b"unequal_b\0");
        let a_pin = crate::cache::hash_bytes(&fs::read(&a).unwrap());
        let b_pin = crate::cache::hash_bytes(&fs::read(&b).unwrap());
        let database = output.join("skyrim_world.db");

        let first = convert_plugins(&[a.clone(), b.clone()], &database, &data).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[&0x1800].load_order, 0);
        assert_eq!(first[&0x0100_1900].load_order, 1);
        drop(first);
        let second = convert_plugins(&[b.clone(), a.clone()], &database, &data).unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[&0x1900].load_order, 0);
        assert_eq!(second[&0x0100_1800].load_order, 1);
        drop(second);

        let conn =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let source_count: u32 = conn
            .query_row("SELECT count(*) FROM inhouse_source_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(source_count, 2); // This owned provenance table is fully refreshed.
        for (runtime, local, supplier, owner, payload) in [
            (0x1900u32, 0x1900u32, 0u32, "b.esm", b_payload.as_slice()),
            (0x0100_1800, 0x1800, 1, "a.esm", a_payload.as_slice()),
        ] {
            let (raw,priority,tag,bytes):(u32,u32,String,Vec<u8>)=conn.query_row(
                "SELECT source_form_id,load_order,record_type,payload FROM inhouse_source_records WHERE form_id=?1",
                [runtime],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            assert_eq!((raw, priority, tag), (local, supplier, "STAT".into()));
            assert_eq!(bytes, payload);
            let (stored_runtime,stored_owner,stored_local):(Option<u32>,String,u32)=conn.query_row(
                "SELECT form_id,original_owner,full_local FROM record_overrides WHERE plugin_id=?1 AND source_form_id=?2 AND status='accepted'",
                rusqlite::params![supplier,local],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            assert_eq!(
                (stored_runtime, stored_owner, stored_local),
                (Some(runtime), owner.into(), local)
            );
            let (runtime_tag, runtime_supplier): (String, u32) = conn
                .query_row(
                    "SELECT record_type,load_order FROM records WHERE form_id=?1",
                    [runtime],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((runtime_tag, runtime_supplier), ("STAT".into(), supplier));
        }
        let headers: u32 = conn
            .query_row("SELECT count(*) FROM record_overrides", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(headers, 4);
        drop(conn);
        // Authenticate this completed synthetic producer's current DB for the raw API call.
        let produced_database_pin = crate::cache::hash_bytes(&fs::read(&database).unwrap());
        for (owner, local, runtime, supplier, payload, header) in [
            (
                "A.esm",
                0x1800u32,
                0x0100_1800u32,
                1u32,
                a_payload.as_slice(),
                a_header.as_slice(),
            ),
            (
                "B.esm",
                0x1900,
                0x1900,
                0,
                b_payload.as_slice(),
                b_header.as_slice(),
            ),
        ] {
            let mut seen = 0;
            let summary = visit_source_history(
                &output,
                None,
                SourceQuery::Source {
                    original_owner: owner.into(),
                    full_local: local,
                },
                |version| {
                    assert_eq!(version.plugin_id, supplier);
                    assert_eq!(version.source_form_id, local);
                    assert_eq!(version.runtime_form_id, Some(runtime));
                    assert_eq!(
                        version.identity.original_owner.as_deref(),
                        Some(crate::esm::load_order::normalize_plugin_name(owner).as_str())
                    );
                    assert_eq!(version.identity.full_local, Some(local));
                    assert!(
                        version.latest_source_candidate && version.latest_reader_admitted_candidate
                    );
                    assert!(version.accepted_source_winner && version.runtime_record_present);
                    assert!(!version.runtime_fallback_before_latest_source);
                    let raw = version.raw.unwrap();
                    assert_eq!(raw.header.as_slice(), header);
                    assert_eq!(raw.stored_payload.as_deref(), Some(payload));
                    seen += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(seen, 1);
            assert!(summary.fully_raw_available);
        }
        assert_eq!(
            crate::cache::hash_bytes(&fs::read(&database).unwrap()),
            produced_database_pin
        );
        assert_eq!(crate::cache::hash_bytes(&fs::read(&a).unwrap()), a_pin);
        assert_eq!(crate::cache::hash_bytes(&fs::read(&b).unwrap()), b_pin);
        // Unrelated old runtime rows are outside this gate; no global cleanup claim is made.
    }
}
