use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::SystemTime,
};

#[cfg(test)]
#[path = "cache/rv_b_tests.rs"]
mod rv_b_tests;

// Combined native-BC, lighting, LOD, TXST and XESP producer. Earlier numeric identities
// alone do not establish compatible output semantics.
pub const CONVERTER_SCHEMA_VERSION: u32 = shared::LOD_CONVERTER_SCHEMA_VERSION;

/// Provenance journal the converter keeps inside a staging directory.
///
/// A staging directory outlives the run that filled it, so a resumed run finds
/// outputs this process did not write. The journal records, per output, the
/// source hash, converter schema and asset-kind settings hash it was produced
/// under, together with the output's size and hash, appended as the output is
/// written. A resumed run reuses a staged output only while its record still
/// matches the current source, schema, kind settings and bytes on disk. The
/// serialized `configuration_hash` field carries this narrow settings proof;
/// the published manifest separately records the full metadata identity.
/// The name is dotted so it can never collide with a converted asset, and the
/// file is dropped from the output directory once the staging directory has
/// been published.
pub const STAGING_JOURNAL_FILE: &str = ".conversion-staging-journal.jsonl";

/// What a staged output was produced from, as recorded when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedOutput {
    pub schema_version: u32,
    /// Asset-kind settings proof; independent of full reader/schema metadata.
    pub configuration_hash: String,
    pub source_hash: String,
    pub output_size: u64,
    pub output_hash: String,
}

impl StagedOutput {
    /// True when the file at `path` is the output this record describes,
    /// produced from `source_hash` by the current converter schema and
    /// configuration.
    pub fn is_current(&self, path: &Path, source_hash: &str, configuration_hash: &str) -> bool {
        self.schema_version == CONVERTER_SCHEMA_VERSION
            && self.configuration_hash == configuration_hash
            && self.source_hash == source_hash
            && fs::metadata(path).is_ok_and(|metadata| metadata.len() == self.output_size)
            && hash_file(path).is_ok_and(|hash| hash == self.output_hash)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StagingJournalLine {
    key: String,
    #[serde(flatten)]
    output: StagedOutput,
}

#[cfg(test)]
thread_local! {
    /// Makes every journal write on this thread fail, for tests of the
    /// pipeline's error path. The batch loop records on the test's own thread.
    pub(crate) static FAIL_JOURNAL_WRITES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

pub struct StagingJournal {
    path: PathBuf,
    file: fs::File,
}

impl StagingJournal {
    pub fn path_in(staging: &Path) -> PathBuf {
        staging.join(STAGING_JOURNAL_FILE)
    }

    /// Opens the journal belonging to `staging`, creating it when the directory
    /// has none yet. A run killed mid-append leaves a partial last line; it is
    /// ended here, so the next record starts a line of its own and only the
    /// partial one is dropped when the journal is read.
    pub fn open(staging: &Path) -> Result<Self> {
        let path = Self::path_in(staging);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .wrap_err_with(|| format!("failed to open staging journal {}", path.display()))?;
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0_u8];
            file.seek(SeekFrom::Start(length - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                // Append mode writes at the end whatever the read position.
                file.write_all(b"\n")
                    .wrap_err_with(|| format!("failed to repair {}", path.display()))?;
            }
        }
        Ok(Self { path, file })
    }

    /// Appends one output's provenance. Each record reaches the journal in a
    /// single write, so a run killed mid-append loses at most the last record,
    /// and the output it describes is converted again. The journal is not
    /// fsynced after each record, so that holds for a crashed or killed process;
    /// a power loss can lose more of the unsynced tail, and those outputs are
    /// converted again too.
    pub fn record(&mut self, key: &str, output: &StagedOutput) -> Result<()> {
        #[cfg(test)]
        if FAIL_JOURNAL_WRITES.with(std::cell::Cell::get) {
            color_eyre::eyre::bail!("injected journal write failure");
        }
        let mut line = serde_json::to_vec(&StagingJournalLine {
            key: key.to_owned(),
            output: output.clone(),
        })?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .wrap_err_with(|| format!("failed to append to {}", self.path.display()))
    }
}

/// Reads the records a previous run left in `staging`, keyed by canonical
/// source key. The last record for a key wins, so an output converted by this
/// run replaces the record of the run it resumes. A truncated or otherwise
/// unreadable line is dropped with a warning: the output it described has no
/// provenance and is converted again.
pub fn load_staged_outputs(staging: &Path) -> Result<BTreeMap<String, StagedOutput>> {
    let mut records = BTreeMap::new();
    let path = StagingJournal::path_in(staging);
    if !path.is_file() {
        return Ok(records);
    }
    let bytes = fs::read(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let mut dropped = 0u32;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<StagingJournalLine>(line) {
            Ok(parsed) => {
                records.insert(parsed.key, parsed.output);
            }
            Err(_) => dropped += 1,
        }
    }
    if dropped > 0 {
        eprintln!(
            "warning: dropped {dropped} unreadable record(s) from {}; those outputs are converted again",
            path.display()
        );
    }
    Ok(records)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub source_hash: String,
    pub output: String,
    pub output_size: u64,
    pub output_hash: String,
}

/// Body composition with thin morph inputs and effective catalog skin choices.
pub(crate) const BODY_COMPOSE_RECIPE: &str = "body-compose-v2";

/// Recipe-owned context digest, alongside the four canonical raw input hashes.
pub(crate) const BODY_COMPOSE_CONTEXT: &str = "recipes/body-compose-v2/context.json";

/// The heavy parts and skeleton shared by historical and current body recipes.
pub(crate) fn body_compose_inputs(sex: &str) -> [String; 4] {
    let assets = "meshes/actors/character/character assets";
    [
        format!("{assets}/{sex}body_1.nif"),
        format!("{assets}/{sex}hands_1.nif"),
        format!("{assets}/{sex}feet_1.nif"),
        format!("{assets}/skeleton.nif"),
    ]
}

/// Versioned default-gait recipe and its canonical raw input contract.
pub(crate) const DEFAULT_GAIT_RECIPE: &str = "default-gait-v1";

/// The complete input set shared by gait proof production and reuse validation.
pub(crate) fn default_gait_inputs(sex: &str, stem: &str) -> [String; 4] {
    let assets = "meshes/actors/character/character assets";
    [
        format!("{assets}/{sex}body_1.nif"),
        format!("{assets}/skeleton.nif"),
        format!("{assets}/skeleton.hkx"),
        format!("meshes/actors/character/animations/{sex}/{stem}.hkx"),
    ]
}

/// Provenance for a generated output with several winning raw source inputs.
/// Recipe versions identify the operation; producer and mesh settings remain
/// in the surrounding manifest, including during metadata-only rebuilds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DerivedOutput {
    pub recipe: String,
    /// Canonical logical source paths and SHA-256 hashes of their winning bytes,
    /// plus any recipe-owned context digest required by the versioned recipe.
    pub inputs: BTreeMap<String, String>,
    pub output_size: u64,
    pub output_hash: String,
}

/// Keeps malformed proof keys as ineligible placeholders for counted local diagnostics.
/// Semantic validation (paths, recipes and hashes) belongs to the reuse consumer.
fn deserialize_derived_outputs<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, DerivedOutput>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Object(values) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, serde_json::from_value(value).unwrap_or_default()))
        .collect())
}

fn deserialize_extensions<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<shared::extensions::ExtensionEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let (entries, issues) = shared::extensions::parse_entries(&value);
    for issue in issues {
        log::warn!(
            "ignored optional capability declaration {:?}: {}",
            issue.id,
            issue.reason
        );
    }
    Ok(entries)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestedFile {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

/// Which archive entries an ingestion cache proves it contains.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IngestionSelection {
    /// Older manifests extracted every archive entry.
    #[default]
    All,
    /// Runtime mesh/texture/script inputs, LOD settings, and English string banks.
    RuntimeEnglishV1,
    ConverterInputsV1,
}

impl IngestionSelection {
    pub fn can_satisfy(self, requested: Self) -> bool {
        self == Self::All
            || self == requested
            || (self == Self::ConverterInputsV1 && requested == Self::RuntimeEnglishV1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestionCacheEntry {
    pub source_hash: String,
    #[serde(default)]
    pub selection: IngestionSelection,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recipe: String,
    pub files: Vec<IngestedFile>,
    /// Version of the archive reader that produced `files`, for formats whose reader changed
    /// what it stores (BSA names are decoded as Windows-1252 from version 1). Absent means 0,
    /// the reader before any such change; an entry is reused only when it matches the current
    /// reader, so a changed reader re-extracts the archive once instead of keeping old paths.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reader_version: u32,
}

pub(crate) fn is_zero(value: &u32) -> bool {
    *value == 0
}

const INGESTION_JOURNAL_FILE: &str = ".ingestion-archives.jsonl";

#[derive(Serialize, Deserialize)]
struct IngestionJournalLine {
    archive: String,
    entry: IngestionCacheEntry,
}

/// Completed archive inventories. Sealed batch packs carry the durable bytes;
/// this journal avoids decoding completed archives when resuming a staging run.
pub(crate) struct IngestionJournal(fs::File);

impl IngestionJournal {
    pub(crate) fn open(staging: &Path) -> Result<Self> {
        let path = staging.join(INGESTION_JOURNAL_FILE);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0_u8];
            file.seek(SeekFrom::Start(length - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        Ok(Self(file))
    }

    pub(crate) fn record(&mut self, archive: &str, entry: &IngestionCacheEntry) -> Result<()> {
        let mut bytes = serde_json::to_vec(&IngestionJournalLine {
            archive: archive.to_owned(),
            entry: entry.clone(),
        })?;
        bytes.push(b'\n');
        self.0.write_all(&bytes)?;
        self.0.sync_all()?;
        Ok(())
    }

    pub(crate) fn load(staging: &Path) -> Result<BTreeMap<String, IngestionCacheEntry>> {
        let path = staging.join(INGESTION_JOURNAL_FILE);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(error.into()),
        };
        let mut entries = BTreeMap::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Ok(record) = serde_json::from_slice::<IngestionJournalLine>(line) {
                entries.insert(record.archive, record.entry);
            }
        }
        Ok(entries)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversionManifest {
    pub schema_version: u32,
    /// Metadata-only rebuilds do not upgrade the retained mesh cache contract.
    /// Absent means the meshes follow `schema_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_mesh_schema_version: Option<u32>,
    /// Producer settings of retained bytes, independent of rebuilt metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_asset_configuration_hash: Option<String>,
    pub complete: bool,
    /// Whether the last authoritative record conversion had no data-loss warnings.
    /// Missing historical proof stays unknown; asset repair cannot establish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_data_complete: Option<bool>,
    #[serde(default)]
    pub configuration_hash: String,
    /// Ordered identities of the runtime schema add-ons used by this pack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schema_addons: Vec<crate::records::addons::SchemaAddonIdentity>,
    /// Original producer settings of each converted asset kind, independent of
    /// record-reader metadata. A metadata-only migration retains these values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub asset_configuration_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub inputs_by_kind: BTreeMap<String, u64>,
    #[serde(default)]
    pub failures: BTreeMap<String, String>,
    /// Non-runtime inputs deliberately excluded, with an auditable reason.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub excluded_inputs: BTreeMap<String, String>,
    /// Texture references a published mesh omits because the game data does not
    /// contain that texture, keyed by the published `.glb` and holding the
    /// resolved texture paths it dropped. Kept out of `failures`: nothing failed
    /// to convert, so these do not make the conversion incomplete.
    ///
    /// This is an audit record for whoever reads the published manifest: the
    /// engine and launcher accept an asset set on `complete` plus the converter
    /// schema version, and nothing else in the workspace reads this list.
    #[serde(default)]
    pub pruned_texture_references: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub archives: BTreeMap<String, IngestionCacheEntry>,
    pub entries: BTreeMap<String, CacheEntry>,
    /// Optional multi-input proofs keyed by canonical published output paths.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "deserialize_derived_outputs"
    )]
    pub derived_outputs: BTreeMap<String, DerivedOutput>,
    /// Versioned derived output, separate from the upstream core contract.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_extensions"
    )]
    pub extensions: Vec<shared::extensions::ExtensionEntry>,
}

/// Historical producers whose scripts may pass normal reuse eligibility.
/// Script ABI/settings, source and output proofs still have to match. Archive
/// extraction uses its own source/blob integrity proof, independent of this gate.
/// Schema 25 is excluded: two unmerged candidates both used it for different contracts.
/// The producer-27 to producer-28 transition changes only texture container bytes,
/// so producer-27 scripts remain valid.
pub(crate) fn can_reuse_scripts_and_archives(schema: u32) -> bool {
    matches!(schema, 12..=24 | 26 | 27)
}

/// Published producer-24 meshes retain the producer-26 byte contract, and producer-27
/// meshes retain the producer-28 byte contract (only texture containers changed).
/// Producer 27 changed the GLB collision data, so no producer before 27 passes this
/// gate at 28 or later. This never authorizes texture reuse or bypasses source/configuration/
/// output proof.
pub(crate) fn can_reuse_meshes(schema: u32) -> bool {
    can_reuse_meshes_for_producer(schema, CONVERTER_SCHEMA_VERSION)
}

/// Check the explicit producer pair so later contracts cannot inherit old mesh reuse.
/// Each pair names a byte-identical GLB contract; `(24, 26)` must never be read as
/// transitive, so `(24, 28)` and `(26, 28)` are deliberately absent. A named
/// [`MESH_OUTPUT_CONTRACTS`] entry changes GLB bytes without advancing the producer, which
/// a historical producer's settings proof cannot name, so cross-producer reuse is suspended
/// while any entry is active.
pub(crate) fn can_reuse_meshes_for_producer(schema: u32, current: u32) -> bool {
    schema == current
        || (MESH_OUTPUT_CONTRACTS.is_empty() && matches!((schema, current), (24, 26) | (27, 28)))
}

/// Named changes to converted mesh bytes made without advancing `CONVERTER_SCHEMA_VERSION`.
/// Each name enters the current producer's mesh settings proof, so a normal conversion rebuilds
/// meshes an earlier converter of the same producer wrote. Append a name per change, never
/// rename or reorder one, and clear the list when the producer advances.
pub(crate) const MESH_OUTPUT_CONTRACTS: &[&str] = &[
    "legacy-skin-export",
    "msn-vertex-normals",
    "facegen-tint-slot",
    "lighting-type-data",
    "msn-specular-default",
    "hidden-niavobject-excluded",
    "nif-root-transform-ignored",
    "skin-static-fallback",
];

/// Whether `schema`'s mesh settings proof carries [`MESH_OUTPUT_CONTRACTS`]. Historical
/// producers keep their recorded proofs; an empty list leaves every proof unchanged.
pub(crate) fn mesh_output_contracts_apply(schema: u32) -> bool {
    schema == CONVERTER_SCHEMA_VERSION && !MESH_OUTPUT_CONTRACTS.is_empty()
}

impl ConversionManifest {
    /// Load archive candidates independently of converted-output producer eligibility.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
        }
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let mut manifest: Self =
            serde_json::from_slice(&bytes).wrap_err("invalid conversion manifest")?;
        let metadata_rebuild = path
            .parent()
            .is_some_and(|root| root.join("metadata-rebuild.json").exists());
        let retained_assets_are_stale = (manifest.schema_version == CONVERTER_SCHEMA_VERSION)
            && match manifest.retained_mesh_schema_version {
                Some(schema) => schema != CONVERTER_SCHEMA_VERSION,
                None => metadata_rebuild,
            };
        if (can_reuse_scripts_and_archives(manifest.schema_version)
            && manifest.schema_version != CONVERTER_SCHEMA_VERSION)
            || retained_assets_are_stale
        {
            let mesh_schema = manifest
                .retained_mesh_schema_version
                .unwrap_or(manifest.schema_version);
            let compatible_meshes = can_reuse_meshes(mesh_schema)
                && mesh_schema <= manifest.schema_version
                && (manifest.retained_mesh_schema_version.is_some() || !metadata_rebuild);
            // All old textures regenerate. GLBs of a producer `can_reuse_meshes`
            // accepts (at 28: producers 27 only, never 24 or 26) and scripts remain
            // candidates for the usual per-asset verification.
            manifest.complete = false;
            manifest.entries.retain(|_, entry| {
                let output = entry.output.to_ascii_lowercase();
                output.ends_with(".luau") || (compatible_meshes && output.ends_with(".glb"))
            });
            return Ok(manifest);
        }
        if manifest.schema_version != CONVERTER_SCHEMA_VERSION {
            // Archive extraction is independent of converted output schemas.
            // Preserve its candidates and original producer metadata; each
            // archive still has to pass its source/integrity checks. Unknown
            // producers never establish normal converted-output eligibility.
            manifest.complete = false;
            manifest.entries.clear();
            return Ok(manifest);
        }
        Ok(manifest)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let temporary = path.with_extension(format!("json.{}.partial", std::process::id()));
        let mut file = fs::File::create(&temporary)
            .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .wrap_err_with(|| format!("failed to publish {}", path.display()))
    }
}

/// Full configuration identity for metadata provenance and older manifests.
/// New converted outputs use separate per-kind settings proofs.
pub fn configuration_hash(config: &crate::config::PipelineConfig) -> Result<String> {
    // no_lod only selects regenerated outputs; it does not change converted asset
    // bytes. Resume invalidates database/LOD outputs before evaluating this proof.
    current_configuration_hash_for_schema(config, CONVERTER_SCHEMA_VERSION)
}

/// Recreate the historical combined proof for a recorded producer schema.
/// Legacy proofs predate projection versions and omit the reader identity; the
/// non-MO2 calculation remains byte-identical. New publication uses `configuration_hash`.
/// Matching this known hash is the conservative fallback when per-kind settings
/// are absent; an arbitrary historical hash cannot establish those settings.
/// MO2 instance/profile paths also bind source selection; winning input bytes
/// remain independently hashed per source.
pub fn configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    let identity = if config.record_reader == crate::config::RecordReader::Inhouse {
        Some(crate::esm::inhouse::configured_reader_identity(config)?)
    } else {
        None
    };
    configuration_hash_with_identity(config, schema, identity.as_ref())
}

/// Bind current metadata projections independently of per-kind asset settings.
pub(crate) fn current_configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    let identity = crate::esm::inhouse::configured_reader_identity(config)?;
    configuration_hash_with_identity(config, schema, Some(&identity))
}

/// Reconstruct the exact adapter-10 reader proof for unchanged retained asset settings.
/// These compatibility pairs are limited to metadata adapters 11 and 12; a future
/// adapter must establish its own retained-byte compatibility instead of inheriting it.
pub(crate) fn inhouse_adapter10_reader_identity(
    schemas: &crate::records::addons::RuntimeSchemas,
) -> Option<serde_json::Value> {
    if !matches!(crate::esm::inhouse::ADAPTER_VERSION, 11 | 12) {
        return None;
    }
    let mut identity = crate::esm::inhouse::reader_identity_with_schemas(
        crate::config::RecordReader::Inhouse,
        schemas,
    );
    identity["adapter_version"] = serde_json::json!(10);
    Some(identity)
}

/// Adapter 12 corrects metadata identities while retaining adapter-11 asset bytes.
pub(crate) fn inhouse_adapter11_reader_identity(
    schemas: &crate::records::addons::RuntimeSchemas,
) -> Option<serde_json::Value> {
    if crate::esm::inhouse::ADAPTER_VERSION != 12 {
        return None;
    }
    let mut identity = crate::esm::inhouse::reader_identity_with_schemas(
        crate::config::RecordReader::Inhouse,
        schemas,
    );
    identity["adapter_version"] = serde_json::json!(11);
    Some(identity)
}

/// Hash the unchanged settings payload with an explicitly selected metadata identity.
fn configuration_hash_with_identity(
    config: &crate::config::PipelineConfig,
    schema: u32,
    identity: Option<&serde_json::Value>,
) -> Result<String> {
    let mut relevant = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_fallback_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "script_abi_version": config.script_abi_version,
    });
    // Actual DDS roles remain in per-source proofs rather than this broad identity.
    if let Some(identity) = identity {
        relevant["record_reader"] = identity.clone();
    }
    if let Some(selection) = &config.mo2 {
        let instance = mo2::Instance::open(&selection.instance_path)?;
        let profile = instance.profile_dir(&selection.profile)?;

        relevant["mo2"] = serde_json::json!({
            "instance": instance.instance_path,
            "profile": profile,
            "mods": instance.mods_dir,
            "overwrite": instance.overwrite_dir,
            "data": std::fs::canonicalize(&config.data_dir)?,
        });
    }
    if schema >= 16 {
        relevant["texture_zstd_level"] = serde_json::json!(config.texture_zstd_level);
    }
    if schema >= 22 {
        // CPU native-BC output and GPU UASTC output have different contracts;
        // quality changes bytes, while GPU batch size only changes scheduling.
        relevant["texture_encoder"] = match config.texture_encoder {
            crate::config::TextureEncoder::Cpu => serde_json::json!({"mode": "cpu"}),
            crate::config::TextureEncoder::Gpu { quality, .. } => {
                serde_json::json!({"mode": "gpu", "quality": quality})
            }
        };
    }
    Ok(hash_bytes(&serde_json::to_vec(&relevant)?))
}

/// Reproduce the exact version-1 legacy projection proof for retained assets.
pub(crate) fn legacy_projection_v1_configuration_hash(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    configuration_hash_with_identity(
        config,
        schema,
        Some(&serde_json::json!({"mode": "legacy", "projection_version": 1})),
    )
}

/// Reproduce the exact version-3 legacy projection proof for retained assets.
pub(crate) fn legacy_projection_v3_configuration_hash(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    configuration_hash_with_identity(
        config,
        schema,
        Some(&serde_json::json!({"mode": "legacy", "projection_version": 3})),
    )
}

/// Reproduce the exact version-2 legacy projection proof for retained assets.
pub(crate) fn legacy_projection_v2_configuration_hash(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    configuration_hash_with_identity(
        config,
        schema,
        Some(&serde_json::json!({"mode": "legacy", "projection_version": 2})),
    )
}

/// Settings that change one asset kind's bytes, excluding full reader metadata.
///
/// Mesh conversion consumes NIF/skeleton/material source dependencies. Texture
/// source proofs additionally include the resolved encoding from fresh SQL/GLB
/// usage. Producer compatibility remains a separate eligibility check; matching
/// one of these hashes does not authorize an unknown or retained old producer.
pub fn asset_configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
    source_ext: &str,
) -> Result<String> {
    let mut relevant = match source_ext {
        "nif" => {
            let mut mesh = serde_json::json!({"mesh_producer": schema});
            if mesh_output_contracts_apply(schema) {
                mesh["mesh_contracts"] = serde_json::json!(MESH_OUTPUT_CONTRACTS);
            }
            mesh
        }
        "dds" => serde_json::json!({
            "texture_producer": schema,
            "texture_etc1s_quality": config.texture_fallback_quality,
            "texture_uastc_level": config.texture_uastc_level,
        }),
        // Script producer eligibility is checked separately. The configured ABI
        // must advance when script-emission behavior changes; metadata migrations
        // keep the original ABI settings hash with the retained script bytes.
        "pex" => serde_json::json!({
            "script_abi_version": config.script_abi_version,
        }),
        _ => color_eyre::eyre::bail!("unsupported asset cache kind {source_ext}"),
    };
    if source_ext == "dds" && schema >= 16 {
        relevant["texture_zstd_level"] = serde_json::json!(config.texture_zstd_level);
    }
    if source_ext == "dds" && schema >= 22 {
        relevant["texture_encoder"] = match config.texture_encoder {
            crate::config::TextureEncoder::Cpu => serde_json::json!({"mode": "cpu"}),
            crate::config::TextureEncoder::Gpu { quality, .. } => {
                serde_json::json!({"mode": "gpu", "quality": quality})
            }
        };
    }
    Ok(hash_bytes(&serde_json::to_vec(&relevant)?))
}

/// Puts `from`'s bytes at `to` as a hard link where the filesystem allows one, else as a copy.
///
/// Every extracted archive entry is stored twice: once under `vfs` and once as the
/// content-addressed blob in `.ingestion-cache` (a cache hit repeats that split). Copying them
/// stored each asset twice, which is tens of gigabytes of game data for a full conversion.
/// Linking costs nothing where the filesystem supports it (NTFS, ext4 and APFS do) and is
/// impossible otherwise, so a cross-volume or linkless filesystem falls back to a copy.
///
/// Linking is safe because nothing writes a shared file in place: extraction and the loose-asset
/// overlay replace the path with a new, unshared file (extraction renames it over the path, the
/// overlay unlinks the path and copies) and no cache blob is ever written in place. An existing `to` is removed rather than written through, and is
/// left alone when it already names `from`. The destination's parent directory must exist.
pub(crate) fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    link_or_copy_with(from, to, |from, to| fs::hard_link(from, to))
}

/// `link_or_copy` with the link step injected, so a test can force the copy fallback on a
/// filesystem that has links.
fn link_or_copy_with(
    from: &Path,
    to: &Path,
    link: fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match fs::symlink_metadata(to) {
        Ok(_) => {
            // Removing the source itself would lose its bytes; a dangling destination symlink
            // must still be removed before the copy fallback can follow it.
            if same_path(from, to) {
                return Ok(());
            }
            fs::remove_file(to)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    link(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))
}

/// How many spill copies of one blob `link_or_copy_spilling` makes before it gives up and copies.
const MAX_SPILLS: u32 = 64;

/// Verification and spill cursors belong to one extraction. Only blobs that
/// reach the filesystem's link limit enter this bounded cache. A state retains
/// the base and active spill handles; saturated prior spills are discarded.
#[derive(Default)]
pub(crate) struct SpillCache {
    states: Mutex<BTreeMap<PathBuf, Arc<Mutex<SpillState>>>>,
    available: Condvar,
}

const MAX_SPILL_STATES: usize = 128;

#[derive(Default)]
struct SpillState {
    base: Option<(SpillProof, String)>,
    active: Option<SpillProof>,
    cursor: u32,
}

struct SpillLease<'a> {
    cache: &'a SpillCache,
    state: Option<Arc<Mutex<SpillState>>>,
}

impl SpillCache {
    fn lease(&self, blob: &Path) -> SpillLease<'_> {
        let mut states = self.states.lock().unwrap();
        let state = loop {
            if let Some(state) = states.get(blob) {
                break Arc::clone(state);
            }
            if states.len() >= MAX_SPILL_STATES {
                let idle = states
                    .iter()
                    .find(|(_, state)| Arc::strong_count(state) == 1)
                    .map(|(path, _)| path.clone());
                if let Some(idle) = idle {
                    states.remove(&idle);
                } else {
                    states = self.available.wait(states).unwrap();
                    continue;
                }
            }
            let state = Arc::new(Mutex::new(SpillState::default()));
            states.insert(blob.to_owned(), Arc::clone(&state));
            break state;
        };
        SpillLease {
            cache: self,
            state: Some(state),
        }
    }
}

impl Drop for SpillLease<'_> {
    fn drop(&mut self) {
        // Pair the state-release notification with the lease predicate's mutex.
        let _states = self.cache.states.lock().unwrap();
        drop(self.state.take());
        self.cache.available.notify_all();
    }
}

struct SpillProof {
    handle: same_file::Handle,
    length: u64,
    modified: Option<SystemTime>,
}

impl SpillProof {
    fn open(path: &Path) -> std::io::Result<Self> {
        let handle = same_file::Handle::from_path(path)?;
        let metadata = handle.as_file().metadata()?;
        Ok(Self {
            handle,
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn matches(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.length == other.length
            // Unsupported modification metadata requires hashing again.
            && self.modified.is_some()
            && self.modified == other.modified
    }

    fn hash(
        &self,
        hash: &impl Fn(&fs::File) -> std::io::Result<String>,
    ) -> std::io::Result<String> {
        let digest = hash(self.handle.as_file())?;
        let after = self.handle.as_file().metadata()?;
        if after.len() != self.length || after.modified().ok() != self.modified {
            return Err(std::io::Error::other("cache file changed while hashing"));
        }
        Ok(digest)
    }

    fn verify_link(&self, destination: &Path) -> std::io::Result<()> {
        let result = (|| {
            let linked = Self::open(destination)?;
            if self.handle != linked.handle
                || self.length != linked.length
                || self.modified != linked.modified
            {
                return Err(std::io::Error::other("cache file changed before linking"));
            }
            Ok(())
        })();
        result.map_err(|error| discard_unverified(destination, error))
    }
}

/// Link through verified spill instances, avoiding repeated reads and scans.
/// Owned cache inodes stay immutable during extraction; writers replace paths.
/// Replacement or observable size/mtime changes invalidate saved proofs, and
/// fresh extractions rehash restored spills. Different blobs have separate locks.
pub(crate) fn link_or_copy_spilling(
    blob: &Path,
    to: &Path,
    cache: &SpillCache,
) -> std::io::Result<()> {
    link_or_copy_spilling_with(
        blob,
        to,
        cache,
        &|from, to| fs::hard_link(from, to),
        &hash_open_file,
    )
}

/// Publish an owned cache temporary by replacing its destination name, preserving old inodes.
/// Cached proof handles and hard links may still own the previous bytes. On Windows the POSIX
/// rename keeps that guarantee for an open destination. It needs NTFS, and a handle open on the
/// destination must allow delete sharing (std's default). A volume or API that reports POSIX
/// rename as unsupported publishes through ordinary persistence instead, which cannot replace a
/// destination that is still open.
pub(crate) fn persist_cache_file(
    temporary: tempfile::NamedTempFile,
    destination: &Path,
) -> std::io::Result<fs::File> {
    #[cfg(windows)]
    {
        publish_cache_file(temporary, destination, rename_posix_windows)
    }
    #[cfg(not(windows))]
    {
        temporary.persist(destination).map_err(|error| error.error)
    }
}

// Win32 codes that a volume or API returns when it lacks POSIX-semantics rename. Access, sharing,
// path, and integrity failures are absent so that they are never retried as a missing capability.
// ERROR_INVALID_PARAMETER is the NT invalid-parameter and invalid-information-class mapping, which
// filesystems use for an unrecognized rename class or flag. Native Windows behaviour is unverified.
#[cfg(any(windows, test))]
const ERROR_INVALID_FUNCTION: i32 = 1;
#[cfg(any(windows, test))]
const ERROR_NOT_SUPPORTED: i32 = 50;
#[cfg(any(windows, test))]
const ERROR_INVALID_PARAMETER: i32 = 87;
#[cfg(any(windows, test))]
const ERROR_CALL_NOT_IMPLEMENTED: i32 = 120;

/// Runs the POSIX-semantics rename first. Only a recognized missing-capability error falls back to
/// ordinary persistence, which retries the still-owned temporary. Every other error leaves the
/// destination untouched and drops the temporary, which removes it.
#[cfg(any(windows, test))]
fn publish_cache_file(
    mut temporary: tempfile::NamedTempFile,
    destination: &Path,
    posix_rename: impl FnOnce(&tempfile::NamedTempFile, &Path) -> std::io::Result<()>,
) -> std::io::Result<fs::File> {
    match posix_rename(&temporary, destination) {
        Ok(()) => {
            // The temporary's name is now the destination, so its cleanup must not run.
            temporary.disable_cleanup(true);
            Ok(temporary.into_file())
        }
        Err(error) if is_missing_rename_capability(&error) => {
            temporary.persist(destination).map_err(|error| error.error)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(windows, test))]
fn is_missing_rename_capability(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            ERROR_INVALID_FUNCTION
                | ERROR_NOT_SUPPORTED
                | ERROR_INVALID_PARAMETER
                | ERROR_CALL_NOT_IMPLEMENTED
        )
    )
}

/// Renames `temporary` over `destination` with POSIX semantics, replacing even an open destination.
/// The rename handle lives only in this call, so it is closed before any fallback persists the
/// temporary. A failure at any step returns before the rename takes effect, leaving the
/// destination's previous bytes in place.
#[cfg(windows)]
fn rename_posix_windows(
    temporary: &tempfile::NamedTempFile,
    destination: &Path,
) -> std::io::Result<()> {
    use std::{
        mem,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_NORMAL, FILE_RENAME_INFO, FileRenameInfoEx, SetFileAttributesW,
            SetFileInformationByHandle,
        },
        System::WindowsProgramming::{
            FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
        },
    };
    let wide = |path: &Path| -> std::io::Result<Vec<u16>> {
        let mut units: Vec<_> = path.as_os_str().encode_wide().collect();
        if units.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cache path contains NUL",
            ));
        }
        units.push(0);
        Ok(units)
    };
    let name = wide(&std::path::absolute(destination)?)?;
    let source = wide(temporary.path())?;
    let name_bytes = (name.len() - 1)
        .checked_mul(mem::size_of::<u16>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path is too long")
        })?;
    // The Win32 wrapper requires a terminating NUL in addition to FileNameLength.
    let buffer_bytes = name
        .len()
        .checked_mul(mem::size_of::<u16>())
        .and_then(|tail| mem::offset_of!(FILE_RENAME_INFO, FileName).checked_add(tail))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cache rename buffer is too large",
            )
        })?;
    let buffer_size = u32::try_from(buffer_bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache rename buffer is too large",
        )
    })?;
    let mut buffer = vec![
        FILE_RENAME_INFO::default();
        buffer_bytes.div_ceil(mem::size_of::<FILE_RENAME_INFO>())
    ];
    let rename = fs::OpenOptions::new()
        .access_mode(DELETE)
        .open(temporary.path())?;
    // SAFETY: the SDK-typed allocation provides alignment and enough initialized storage for
    // the header and complete NUL-terminated UTF-16 tail. Both file handles and buffers remain
    // live throughout the calls. No pointer escapes; the source is our owned temporary.
    unsafe {
        let info = buffer.as_mut_ptr();
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        // Match NamedTempFile::persist's durability contract before publishing the file.
        if SetFileAttributesW(source.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // POSIX replacement preserves the old inode behind proof handles and prior hard links.
        // MoveFileExW, used by tempfile::persist, refuses this open-destination case.
        if SetFileInformationByHandle(
            rename.as_raw_handle(),
            FileRenameInfoEx,
            buffer.as_ptr().cast(),
            buffer_size,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn link_or_copy_spilling_with(
    blob: &Path,
    to: &Path,
    cache: &SpillCache,
    link: &impl Fn(&Path, &Path) -> std::io::Result<()>,
    hash: &impl Fn(&fs::File) -> std::io::Result<String>,
) -> std::io::Result<()> {
    link_or_copy_spilling_impl(blob, to, cache, link, hash, &mut |from, to, spill| {
        if !spill {
            return fs::copy(from, to).map(|_| ());
        }
        let temporary = tempfile::NamedTempFile::new_in(to.parent().unwrap())?;
        fs::copy(from, temporary.path())?;
        persist_cache_file(temporary, to)?;
        Ok(())
    })
}

pub(crate) fn link_or_copy_spilling_with_copy_and_link(
    blob: &Path,
    to: &Path,
    cache: &SpillCache,
    link: &impl Fn(&Path, &Path) -> std::io::Result<()>,
    mut copy: impl FnMut(&Path, &Path, bool) -> std::io::Result<()>,
) -> std::io::Result<()> {
    link_or_copy_spilling_impl(blob, to, cache, link, &hash_open_file, &mut copy)
}

fn link_or_copy_spilling_impl(
    blob: &Path,
    to: &Path,
    cache: &SpillCache,
    link: &impl Fn(&Path, &Path) -> std::io::Result<()>,
    hash: &impl Fn(&fs::File) -> std::io::Result<String>,
    copy: &mut impl FnMut(&Path, &Path, bool) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match fs::symlink_metadata(to) {
        Ok(metadata) => {
            if metadata.is_file() && same_path(blob, to) {
                return Ok(());
            }
            fs::remove_file(to)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match link(blob, to) {
        Ok(()) => return Ok(()),
        Err(error) if !is_too_many_links(&error) => return copy(blob, to, false),
        Err(_) => {}
    }
    let lease = cache.lease(blob);
    let mut state = lease.state.as_ref().unwrap().lock().unwrap();
    let base = SpillProof::open(blob)?;
    if !state
        .base
        .as_ref()
        .is_some_and(|(proof, _)| proof.matches(&base))
    {
        let digest = base.hash(hash)?;
        state.base = Some((base, digest));
        state.cursor = 1;
        state.active = None;
    }
    while state.cursor <= MAX_SPILLS {
        let mut name = blob.as_os_str().to_owned();
        name.push(format!(".{}", state.cursor));
        let spill = PathBuf::from(name);
        let candidate = SpillProof::open(&spill).ok();
        let unchanged = candidate.as_ref().is_some_and(|candidate| {
            state
                .active
                .as_ref()
                .is_some_and(|proof| proof.matches(candidate))
        });
        if !unchanged {
            let expected = &state.base.as_ref().unwrap().1;
            let valid = candidate.as_ref().is_some_and(|candidate| {
                candidate.hash(hash).is_ok_and(|digest| &digest == expected)
            });
            if valid {
                state.active = candidate;
            } else {
                // The callback atomically replaces the logical spill path and records
                // that physical file's durability, rather than a disposable temporary name.
                copy(blob, &spill, true)?;
                let proof = SpillProof::open(&spill)?;
                if &proof.hash(hash)? != expected {
                    return Err(discard_unverified(
                        &spill,
                        std::io::Error::other("cache file changed before spill copy"),
                    ));
                }
                state.active = Some(proof);
            }
        }
        match link(&spill, to) {
            Ok(()) => return state.active.as_ref().unwrap().verify_link(to),
            Err(error) if is_too_many_links(&error) => {
                state.cursor += 1;
                state.active = None;
            }
            Err(_) => break,
        }
    }
    // Beyond the bounded spill count, verify this derived copy before use.
    let result = (|| {
        copy(blob, to, false)?;
        let copied = SpillProof::open(to)?;
        if copied.hash(hash)? != state.base.as_ref().unwrap().1 {
            return Err(std::io::Error::other("cache file changed before copying"));
        }
        Ok(())
    })();
    result.map_err(|error| discard_unverified(to, error))
}

fn discard_unverified(path: &Path, error: std::io::Error) -> std::io::Error {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
        Err(cleanup) => std::io::Error::other(format!(
            "{error}; failed to remove unverified destination {}: {cleanup}",
            path.display()
        )),
    }
}

/// A link refused because the file already has as many names as the filesystem allows.
fn is_too_many_links(error: &std::io::Error) -> bool {
    // ERROR_TOO_MANY_LINKS on Windows; EMLINK elsewhere.
    error.kind() == std::io::ErrorKind::TooManyLinks || error.raw_os_error() == Some(1142)
}

/// Whether `from` and `to` are the same path (removing `to` would then delete `from`).
fn same_path(from: &Path, to: &Path) -> bool {
    match fs::canonicalize(from) {
        Ok(from) => fs::canonicalize(to).is_ok_and(|to| to == from),
        Err(_) => false,
    }
}

/// Retained schema-15/16 producers also recorded their fixed Zstd level of 6.
/// The protected schema-15 source manifest proves this exact configuration
/// variant; quality and script ABI must still match its recorded hash.
/// This compatibility route verifies retained bytes, not normal cache reuse.
pub(crate) fn retained_configuration_matches(
    config: &crate::config::PipelineConfig,
    schema: u32,
    recorded: &str,
) -> Result<bool> {
    if recorded == configuration_hash_for_schema(config, schema)? {
        return Ok(true);
    }
    if !matches!(schema, 15 | 16) || config.texture_zstd_level != 6 {
        return Ok(false);
    }
    let native = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_fallback_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "texture_zstd_level": 6,
        "script_abi_version": config.script_abi_version,
    });
    Ok(recorded == hash_bytes(&serde_json::to_vec(&native)?))
}

/// Verify full settings under their recorded projection contract without upgrading it.
pub(crate) fn retained_configuration_matches_for_reader(
    config: &crate::config::PipelineConfig,
    schema: u32,
    recorded: &str,
    identity: &serde_json::Value,
) -> Result<bool> {
    if config.record_reader == crate::config::RecordReader::Legacy
        && identity == &serde_json::json!({"mode": "legacy"})
    {
        return retained_configuration_matches(config, schema, recorded);
    }
    if config.record_reader == crate::config::RecordReader::Legacy
        && identity == &serde_json::json!({"mode": "legacy", "projection_version": 1})
    {
        return Ok(recorded == legacy_projection_v1_configuration_hash(config, schema)?);
    }
    if config.record_reader == crate::config::RecordReader::Legacy
        && identity == &serde_json::json!({"mode": "legacy", "projection_version": 2})
    {
        return Ok(recorded == legacy_projection_v2_configuration_hash(config, schema)?);
    }
    if config.record_reader == crate::config::RecordReader::Legacy
        && identity == &serde_json::json!({"mode": "legacy", "projection_version": 3})
    {
        // Projection v3 resolved only English banks. Preserve old proofs under that contract.
        if !crate::esm::inhouse::is_english(&config.language) {
            return Ok(false);
        }
        return Ok(recorded == legacy_projection_v3_configuration_hash(config, schema)?);
    }
    let current = crate::esm::inhouse::configured_reader_identity(config)?;
    if identity == &current {
        return Ok(recorded == current_configuration_hash_for_schema(config, schema)?);
    }
    if config.record_reader == crate::config::RecordReader::Inhouse
        && [
            inhouse_adapter10_reader_identity(config.schemas()?.as_ref()),
            inhouse_adapter11_reader_identity(config.schemas()?.as_ref()),
        ]
        .iter()
        .any(|prior| prior.as_ref() == Some(identity))
    {
        return Ok(recorded == configuration_hash_with_identity(config, schema, Some(identity))?);
    }
    Ok(false)
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path)
        .wrap_err_with(|| format!("failed to open {} for hashing", path.display()))?;
    hash_open_file(&file).wrap_err_with(|| format!("failed to hash {}", path.display()))
}

fn hash_open_file(mut file: &fs::File) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_capability_parse_is_tolerant_per_entry() {
        let good = shared::extensions::ExtensionEntry {
            id: "example.valid".into(),
            version: 1,
            owns: Default::default(),
            sha256: String::new(),
            built_from: "core".into(),
            stats: Default::default(),
        };
        let mut document = serde_json::to_value(ConversionManifest::default()).unwrap();
        document["extensions"] = serde_json::json!([
            {"id": "example.missing_fields"},
            {"id": "example.float_stat", "version": 1, "sha256": "", "built_from": "core", "stats": {"rows": 1.5}},
            good,
        ]);
        let manifest: ConversionManifest = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(manifest.extensions, vec![good]);
        document["extensions"] = serde_json::Value::Null;
        let manifest: ConversionManifest = serde_json::from_value(document).unwrap();
        assert!(manifest.extensions.is_empty());
    }

    #[test]
    fn extensions_are_optional_and_round_trip_without_changing_core_fields() {
        let mut manifest = ConversionManifest::default();
        let before = serde_json::to_value(&manifest).unwrap();
        assert!(before.get("extensions").is_none());
        let legacy: ConversionManifest = serde_json::from_value(before.clone()).unwrap();
        assert!(legacy.extensions.is_empty());
        manifest
            .extensions
            .push(shared::extensions::ExtensionEntry {
                id: "example.payload".to_owned(),
                version: 1,
                owns: shared::extensions::ExtensionOwns::default(),
                sha256: "0".repeat(64),
                built_from: "core".to_owned(),
                stats: BTreeMap::new(),
            });
        let mut with_extension = serde_json::to_value(&manifest).unwrap();
        let restored: ConversionManifest = serde_json::from_value(with_extension.clone()).unwrap();
        assert_eq!(restored, manifest);
        with_extension.as_object_mut().unwrap().remove("extensions");
        assert_eq!(with_extension, before);
    }

    /// Legacy absence and malformed optional records do not poison neighboring proofs.
    #[test]
    fn derived_output_deserialization_is_tolerant_per_value() {
        let base = serde_json::to_value(ConversionManifest::default()).unwrap();
        assert!(base.get("derived_outputs").is_none());
        let legacy: ConversionManifest = serde_json::from_value(base.clone()).unwrap();
        assert!(legacy.derived_outputs.is_empty());
        let valid = serde_json::json!({
            "recipe": "body-compose-v1",
            "inputs": {"meshes/body.nif": "raw-hash"},
            "output_size": 17,
            "output_hash": "output-hash"
        });
        let mut json = base.clone();
        json["derived_outputs"] = serde_json::json!({
            "meshes/good.glb": valid,
            "meshes/null.glb": null,
            "meshes/string.glb": "bad",
            "meshes/missing.glb": {"recipe": "body-compose-v1"},
            "meshes/type.glb": {
                "recipe": "default-gait-v1", "inputs": [],
                "output_size": "large", "output_hash": 123
            }
        });
        let manifest: ConversionManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.derived_outputs.len(), 5);
        assert_eq!(manifest.derived_outputs["meshes/good.glb"].output_size, 17);
        assert_eq!(
            manifest.derived_outputs["meshes/missing.glb"],
            DerivedOutput::default()
        );
        let roundtrip: ConversionManifest =
            serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(roundtrip, manifest);
        for malformed_map in [serde_json::Value::Null, serde_json::json!([1, 2])] {
            let mut json = base.clone();
            json["derived_outputs"] = malformed_map;
            let manifest: ConversionManifest = serde_json::from_value(json).unwrap();
            assert!(manifest.derived_outputs.is_empty());
        }
    }

    /// Historical absence stays unknown while known loss and success round-trip distinctly.
    #[test]
    fn parallel_spill_creation_uses_unique_temporary_files() {
        let barrier = std::sync::Barrier::new(4);
        let full_blob = |from: &Path, to: &Path| -> std::io::Result<()> {
            if from.file_name().is_some_and(|name| name == "parallel_blob") {
                barrier.wait();
                Err(std::io::ErrorKind::TooManyLinks.into())
            } else {
                fs::hard_link(from, to)
            }
        };
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("parallel_blob");
        fs::write(&blob, b"immutable bytes").unwrap();
        let cache = SpillCache::default();
        std::thread::scope(|scope| {
            for index in 0..4 {
                let blob = &blob;
                let cache = &cache;
                let full_blob = &full_blob;
                let destination = directory.path().join(format!("asset{index}"));
                scope.spawn(move || {
                    link_or_copy_spilling_with(
                        blob,
                        &destination,
                        cache,
                        full_blob,
                        &hash_open_file,
                    )
                    .unwrap();
                    assert_eq!(fs::read(destination).unwrap(), b"immutable bytes");
                });
            }
        });
    }

    #[test]
    fn cache_publication_replaces_open_inodes_and_preserves_prior_links() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("cache");
        let previous = directory.path().join("previous");
        fs::write(&destination, b"old bytes").unwrap();
        fs::hard_link(&destination, &previous).unwrap();
        let proof = SpillProof::open(&destination).unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        let published = persist_cache_file(temporary, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new bytes");
        assert_eq!(fs::read(&previous).unwrap(), b"old bytes");
        assert_eq!(
            proof.hash(&hash_open_file).unwrap(),
            hash_file(&previous).unwrap()
        );
        assert_eq!(
            hash_open_file(&published).unwrap(),
            hash_file(&destination).unwrap()
        );
        assert!(!temporary_path.exists());

        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fs::write(temporary.path(), b"fresh bytes").unwrap();
        let fresh = directory.path().join("fresh");
        persist_cache_file(temporary, &fresh).unwrap();
        assert_eq!(fs::read(fresh).unwrap(), b"fresh bytes");
    }

    #[test]
    fn failed_cache_publication_removes_the_temporary_and_keeps_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        // A directory cannot be replaced by a file, so the rename itself fails.
        let destination = directory.path().join("occupied");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("kept"), b"kept").unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        assert!(persist_cache_file(temporary, &destination).is_err());
        assert!(!temporary_path.exists());
        assert_eq!(fs::read(destination.join("kept")).unwrap(), b"kept");
    }

    #[test]
    fn cache_publication_falls_back_to_persistence_when_rename_capability_is_missing() {
        for code in [
            ERROR_INVALID_FUNCTION,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_PARAMETER,
            ERROR_CALL_NOT_IMPLEMENTED,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("cache");
            let previous = directory.path().join("previous");
            fs::write(&destination, b"old bytes").unwrap();
            fs::hard_link(&destination, &previous).unwrap();
            // Ordinary persistence cannot replace an open destination on Windows, so the proof
            // checks the prior bytes and closes before publication.
            let proof = SpillProof::open(&destination).unwrap();
            assert_eq!(
                proof.hash(&hash_open_file).unwrap(),
                hash_file(&previous).unwrap()
            );
            drop(proof);
            let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
            let temporary_path = temporary.path().to_owned();
            fs::write(temporary.path(), b"new bytes").unwrap();
            let mut attempts = 0;
            let published = publish_cache_file(temporary, &destination, |_, _| {
                attempts += 1;
                Err(std::io::Error::from_raw_os_error(code))
            })
            .unwrap();
            assert_eq!(attempts, 1);
            assert_eq!(fs::read(&destination).unwrap(), b"new bytes");
            assert_eq!(fs::read(&previous).unwrap(), b"old bytes");
            assert_eq!(
                hash_open_file(&published).unwrap(),
                hash_file(&destination).unwrap()
            );
            assert!(!temporary_path.exists());

            // An absent destination takes the same fallback without any prior proof.
            let absent = directory.path().join("absent");
            let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
            let temporary_path = temporary.path().to_owned();
            fs::write(temporary.path(), b"absent bytes").unwrap();
            let published = publish_cache_file(temporary, &absent, |_, _| {
                Err(std::io::Error::from_raw_os_error(code))
            })
            .unwrap();
            assert_eq!(fs::read(&absent).unwrap(), b"absent bytes");
            assert_eq!(
                hash_open_file(&published).unwrap(),
                hash_file(&absent).unwrap()
            );
            assert!(!temporary_path.exists());
        }
    }

    /// On Windows, ordinary persistence cannot replace a destination that is still open. The
    /// fallback must fail, leaving the prior bytes in place and the temporary removed.
    #[cfg(windows)]
    #[test]
    fn cache_publication_fallback_fails_safely_when_an_open_destination_cannot_be_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("cache");
        let previous = directory.path().join("previous");
        fs::write(&destination, b"old bytes").unwrap();
        fs::hard_link(&destination, &previous).unwrap();
        let proof = SpillProof::open(&destination).unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        let result = publish_cache_file(temporary, &destination, |_, _| {
            Err(std::io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED))
        });
        assert!(result.is_err());
        assert_eq!(
            proof.hash(&hash_open_file).unwrap(),
            hash_file(&previous).unwrap()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"old bytes");
        assert_eq!(fs::read(&previous).unwrap(), b"old bytes");
        assert!(!temporary_path.exists());
    }

    #[test]
    fn cache_publication_keeps_the_destination_when_rename_fails_for_other_reasons() {
        // Win32 access-denied and sharing-violation codes: failures a fallback must not retry.
        for code in [5, 32] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("cache");
            let previous = directory.path().join("previous");
            fs::write(&destination, b"old bytes").unwrap();
            fs::hard_link(&destination, &previous).unwrap();
            let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
            let temporary_path = temporary.path().to_owned();
            fs::write(temporary.path(), b"new bytes").unwrap();
            let mut attempts = 0;
            let error = publish_cache_file(temporary, &destination, |_, _| {
                attempts += 1;
                Err(std::io::Error::from_raw_os_error(code))
            })
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
            assert_eq!(attempts, 1);
            assert_eq!(fs::read(&destination).unwrap(), b"old bytes");
            assert_eq!(fs::read(&previous).unwrap(), b"old bytes");
            assert!(!temporary_path.exists());
        }
    }

    #[test]
    fn cache_publication_fallback_failure_keeps_the_destination_and_removes_the_temporary() {
        let directory = tempfile::tempdir().unwrap();
        // The fallback persist fails on a directory destination, so the error must come back
        // and the directory's contents must survive.
        let destination = directory.path().join("occupied");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("kept"), b"kept").unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        let result = publish_cache_file(temporary, &destination, |_, _| {
            Err(std::io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED))
        });
        assert!(result.is_err());
        assert!(!temporary_path.exists());
        assert_eq!(fs::read(destination.join("kept")).unwrap(), b"kept");
    }

    #[test]
    fn cache_publication_returns_the_published_file_after_a_posix_rename() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("cache");
        fs::write(&destination, b"old bytes").unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        let published = publish_cache_file(temporary, &destination, |temporary, destination| {
            fs::rename(temporary.path(), destination)
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new bytes");
        assert_eq!(
            hash_open_file(&published).unwrap(),
            hash_file(&destination).unwrap()
        );
        assert!(!temporary_path.exists());
    }

    #[test]
    fn cache_publication_rejects_nul_without_replacing_the_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("cache");
        fs::write(&prefix, b"old bytes").unwrap();
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(temporary.path(), b"new bytes").unwrap();
        let mut invalid = prefix.into_os_string();
        invalid.push("\0suffix");
        let error = persist_cache_file(temporary, Path::new(&invalid)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read(directory.path().join("cache")).unwrap(),
            b"old bytes"
        );
        assert!(!temporary_path.exists());
    }

    #[test]
    fn corrupt_derived_spills_are_replaced_before_reuse() {
        fn full_blob(from: &Path, to: &Path) -> std::io::Result<()> {
            if from.file_name().is_some_and(|name| name == "blob") {
                Err(std::io::ErrorKind::TooManyLinks.into())
            } else {
                fs::hard_link(from, to)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"valid bytes").unwrap();
        fs::write(directory.path().join("blob.1"), b"wrong bytes").unwrap();
        let destination = directory.path().join("asset");
        link_or_copy_spilling_with(
            &blob,
            &destination,
            &SpillCache::default(),
            &full_blob,
            &hash_open_file,
        )
        .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"valid bytes");
    }

    fn full_base(from: &Path, to: &Path) -> std::io::Result<()> {
        if from.file_name().is_some_and(|name| name == "blob") {
            Err(std::io::ErrorKind::TooManyLinks.into())
        } else {
            fs::hard_link(from, to)
        }
    }

    #[test]
    fn spill_verification_reads_and_link_attempts_grow_linearly() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for count in [8, 16, 32] {
            let directory = tempfile::tempdir().unwrap();
            let blob = directory.path().join("blob");
            fs::write(&blob, [0x5a; 4096]).unwrap();
            let cache = SpillCache::default();
            let links = Mutex::new(BTreeMap::<PathBuf, usize>::new());
            let attempts = AtomicUsize::new(0);
            let reads = AtomicUsize::new(0);
            let link = |from: &Path, to: &Path| {
                attempts.fetch_add(1, Ordering::Relaxed);
                let mut links = links.lock().unwrap();
                let used = links.entry(from.to_owned()).or_default();
                if *used >= 2 {
                    return Err(std::io::ErrorKind::TooManyLinks.into());
                }
                fs::hard_link(from, to)?;
                *used += 1;
                Ok(())
            };
            let hash = |file: &fs::File| {
                reads.fetch_add(1, Ordering::Relaxed);
                hash_open_file(file)
            };
            for index in 0..count {
                let destination = directory.path().join(format!("asset{index}"));
                link_or_copy_spilling_with(&blob, &destination, &cache, &link, &hash).unwrap();
                assert_eq!(fs::read(destination).unwrap(), [0x5a; 4096]);
            }
            // One base hash and one per new spill. With the previous scan,
            // the same cases read 15, 63 and 255 full files.
            assert_eq!(reads.load(Ordering::Relaxed), count / 2);
            assert!(attempts.load(Ordering::Relaxed) < count * 3);
        }
    }

    #[test]
    fn cached_spill_revalidates_corruption_and_same_metadata_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        let spill = directory.path().join("blob.1");
        fs::write(&blob, b"valid bytes").unwrap();
        let cache = SpillCache::default();
        let restore = |index| {
            let destination = directory.path().join(format!("asset{index}"));
            link_or_copy_spilling_with(&blob, &destination, &cache, &full_base, &hash_open_file)
                .unwrap();
            assert_eq!(fs::read(destination).unwrap(), b"valid bytes");
        };
        restore(0);
        fs::write(&spill, b"wrong bytes").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&spill)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        restore(1);
        let modified = fs::metadata(&spill).unwrap().modified().unwrap();
        let replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fs::write(replacement.path(), b"wrong bytes").unwrap();
        replacement
            .as_file()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        persist_cache_file(replacement, &spill).unwrap();
        assert_eq!(fs::metadata(&spill).unwrap().modified().unwrap(), modified);
        restore(2);
        // A new extraction must verify a restored spill even if the previous
        // extraction already trusted the path.
        let fresh = SpillCache::default();
        fs::write(&spill, b"wrong bytes").unwrap();
        let destination = directory.path().join("fresh");
        link_or_copy_spilling_with(&blob, &destination, &fresh, &full_base, &hash_open_file)
            .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"valid bytes");
    }

    #[test]
    fn replacement_during_link_cannot_publish_unverified_spill_bytes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"valid bytes").unwrap();
        let cache = SpillCache::default();
        let replace = AtomicBool::new(true);
        let link = |from: &Path, to: &Path| {
            if from == blob {
                return Err(std::io::ErrorKind::TooManyLinks.into());
            }
            if replace.swap(false, Ordering::Relaxed) {
                let modified = fs::metadata(from)?.modified()?;
                let replacement = tempfile::NamedTempFile::new_in(directory.path())?;
                fs::write(replacement.path(), b"wrong bytes")?;
                replacement
                    .as_file()
                    .set_times(fs::FileTimes::new().set_modified(modified))?;
                persist_cache_file(replacement, from)?;
            }
            fs::hard_link(from, to)
        };
        let destination = directory.path().join("asset");
        assert!(
            link_or_copy_spilling_with(&blob, &destination, &cache, &link, &hash_open_file)
                .is_err()
        );
        assert!(!destination.exists());
        link_or_copy_spilling_with(&blob, &destination, &cache, &link, &hash_open_file).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"valid bytes");
    }

    #[test]
    fn failed_final_copy_verification_removes_only_the_new_destination() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"valid bytes").unwrap();
        let cache = SpillCache::default();
        let verified = directory.path().join("verified");
        link_or_copy_spilling_with(&blob, &verified, &cache, &full_base, &hash_open_file).unwrap();
        {
            let states = cache.states.lock().unwrap();
            let mut state = states[&blob].lock().unwrap();
            state.cursor = MAX_SPILLS + 1;
            state.active = None;
        }
        let destination = directory.path().join("new");
        let fail_hash = |_: &fs::File| Err(std::io::Error::other("injected verification failure"));
        let error = link_or_copy_spilling_with(&blob, &destination, &cache, &full_base, &fail_hash)
            .unwrap_err();
        assert!(error.to_string().contains("injected verification failure"));
        assert!(!destination.exists());
        assert_eq!(fs::read(verified).unwrap(), b"valid bytes");
    }

    #[test]
    fn base_replacement_resets_the_spill_cursor_and_keeps_previous_links() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"first bytes").unwrap();
        let cache = SpillCache::default();
        let link = |from: &Path, to: &Path| {
            if from == blob || (from.ends_with("blob.1") && to.ends_with("second")) {
                Err(std::io::ErrorKind::TooManyLinks.into())
            } else {
                fs::hard_link(from, to)
            }
        };
        let first = directory.path().join("first");
        link_or_copy_spilling_with(&blob, &first, &cache, &link, &hash_open_file).unwrap();
        link_or_copy_spilling_with(
            &blob,
            &directory.path().join("second"),
            &cache,
            &link,
            &hash_open_file,
        )
        .unwrap();
        let replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fs::write(replacement.path(), b"later bytes").unwrap();
        persist_cache_file(replacement, &blob).unwrap();
        let later = directory.path().join("later");
        link_or_copy_spilling_with(&blob, &later, &cache, &link, &hash_open_file).unwrap();
        assert_eq!(fs::read(later).unwrap(), b"later bytes");
        assert_eq!(fs::read(first).unwrap(), b"first bytes");
        assert_eq!(
            cache.states.lock().unwrap()[&blob].lock().unwrap().cursor,
            1
        );
    }

    #[test]
    fn a_full_spill_cache_wakes_a_waiter_after_a_lease_is_released() {
        let cache = SpillCache::default();
        std::thread::scope(|scope| {
            let mut leases = (0..MAX_SPILL_STATES)
                .map(|index| cache.lease(Path::new(&format!("blob{index}"))))
                .collect::<Vec<_>>();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let cache = &cache;
            scope.spawn(move || {
                started_tx.send(()).unwrap();
                let _lease = cache.lease(Path::new("extra"));
                done_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            drop(leases.pop());
            done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert_eq!(cache.states.lock().unwrap().len(), MAX_SPILL_STATES);
        });
    }

    #[test]
    fn spill_cache_evicts_idle_states_and_releases_their_handles() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SpillCache::default();
        for index in 0..MAX_SPILL_STATES + 2 {
            let blob = directory.path().join(format!("blob{index}"));
            fs::write(&blob, b"bytes").unwrap();
            let link = |from: &Path, to: &Path| {
                if from == blob {
                    Err(std::io::ErrorKind::TooManyLinks.into())
                } else {
                    fs::hard_link(from, to)
                }
            };
            link_or_copy_spilling_with(
                &blob,
                &directory.path().join(format!("asset{index}")),
                &cache,
                &link,
                &hash_open_file,
            )
            .unwrap();
        }
        let states = cache.states.lock().unwrap();
        assert_eq!(states.len(), MAX_SPILL_STATES);
        assert!(!states.contains_key(&directory.path().join("blob0")));
        assert!(states.values().all(|state| Arc::strong_count(state) == 1));
    }

    #[test]
    fn ingestion_journal_resumes_completed_inventories_and_drops_partial_tail() {
        let directory = tempfile::tempdir().unwrap();
        let entry = IngestionCacheEntry {
            source_hash: "ab".repeat(32),
            selection: IngestionSelection::ConverterInputsV1,
            recipe: IngestionSelection::ConverterInputsV1.recipe().to_owned(),
            files: vec![IngestedFile {
                path: "lodsettings/tamriel.lod".to_owned(),
                size: 4,
                hash: "cd".repeat(32),
            }],
            reader_version: 0,
        };
        let mut journal = IngestionJournal::open(directory.path()).unwrap();
        journal.record("base.bsa", &entry).unwrap();
        drop(journal);
        let path = directory.path().join(INGESTION_JOURNAL_FILE);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"archive\":\"truncated")
            .unwrap();
        assert_eq!(
            IngestionJournal::load(directory.path()).unwrap()["base.bsa"],
            entry
        );
        let mut journal = IngestionJournal::open(directory.path()).unwrap();
        journal.record("dlc.bsa", &entry).unwrap();
        let loaded = IngestionJournal::load(directory.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["dlc.bsa"], entry);
    }

    #[test]
    fn record_completeness_preserves_unknown_false_and_true() {
        let original = serde_json::to_value(ConversionManifest::default()).unwrap();
        assert!(original.get("record_data_complete").is_none());
        let historical: ConversionManifest = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(historical.record_data_complete, None);
        for complete in [false, true] {
            let mut json = original.clone();
            json["record_data_complete"] = serde_json::json!(complete);
            let manifest: ConversionManifest = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(manifest.record_data_complete, Some(complete));
            assert_eq!(serde_json::to_value(manifest).unwrap(), json);
        }
    }

    /// Known prior adapter proofs retain asset bytes without accepting unknown readers.
    #[test]
    fn adapter10_retained_settings_keep_original_identity_and_require_exact_proofs() {
        let mut config = crate::config::PipelineConfig::new("Data", "output");
        config.record_reader = crate::config::RecordReader::Inhouse;
        for adapter in [10, 11] {
            let mut original = crate::esm::inhouse::reader_identity(config.record_reader);
            original["adapter_version"] = serde_json::json!(adapter);
            let recorded = configuration_hash_with_identity(
                &config,
                CONVERTER_SCHEMA_VERSION,
                Some(&original),
            )
            .unwrap();
            assert!(
                retained_configuration_matches_for_reader(
                    &config,
                    CONVERTER_SCHEMA_VERSION,
                    &recorded,
                    &original
                )
                .unwrap(),
                "the exact original adapter-{adapter} proof must survive the metadata-only adapter change"
            );
            assert_ne!(recorded, configuration_hash(&config).unwrap());
            assert_eq!(original["adapter_version"], adapter);

            let mut changed = config.clone();
            changed.texture_fallback_quality = 191;
            assert!(
                !retained_configuration_matches_for_reader(
                    &changed,
                    CONVERTER_SCHEMA_VERSION,
                    &recorded,
                    &original
                )
                .unwrap()
            );
            let mut changed = config.clone();
            changed.script_abi_version = 2;
            assert!(
                !retained_configuration_matches_for_reader(
                    &changed,
                    CONVERTER_SCHEMA_VERSION,
                    &recorded,
                    &original
                )
                .unwrap()
            );
            assert!(
                !retained_configuration_matches_for_reader(
                    &config,
                    CONVERTER_SCHEMA_VERSION,
                    "different-proof",
                    &original
                )
                .unwrap()
            );
            for (key, value) in [
                ("adapter_version", serde_json::json!(8)),
                ("schema_sha256", serde_json::json!("different-schema")),
                ("text_encoding", serde_json::json!("utf-8")),
                ("extra", serde_json::json!(true)),
            ] {
                let mut altered = original.clone();
                altered[key] = value;
                assert!(
                    !retained_configuration_matches_for_reader(
                        &config,
                        CONVERTER_SCHEMA_VERSION,
                        &recorded,
                        &altered
                    )
                    .unwrap(),
                    "accepted altered {key}"
                );
            }
            for kind in ["nif", "dds", "pex"] {
                let mut legacy = config.clone();
                legacy.record_reader = crate::config::RecordReader::Legacy;
                assert_eq!(
                    asset_configuration_hash_for_schema(&config, CONVERTER_SCHEMA_VERSION, kind)
                        .unwrap(),
                    asset_configuration_hash_for_schema(&legacy, CONVERTER_SCHEMA_VERSION, kind)
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn both_prior_cache_contracts_limit_coverage_and_unknown_recipes_miss() {
        let pack_entry: IngestionCacheEntry = serde_json::from_value(serde_json::json!({
            "source_hash": "source",
            "recipe": IngestionSelection::ConverterInputsV1.recipe(),
            "files": []
        }))
        .unwrap();
        assert!(pack_entry.covers(IngestionSelection::RuntimeEnglishV1));
        assert!(pack_entry.covers(IngestionSelection::ConverterInputsV1));
        assert!(!pack_entry.covers(IngestionSelection::All));
        // The recipe changed when the animation inputs joined the selection, so a pack minted
        // under the earlier converter-inputs recipe cannot prove the wider file set.
        let mut narrower = pack_entry.clone();
        narrower.recipe = "converter-inputs-v1".into();
        assert!(!narrower.covers(IngestionSelection::ConverterInputsV1));
        assert!(!narrower.covers(IngestionSelection::RuntimeEnglishV1));
        let raw_entry: IngestionCacheEntry = serde_json::from_value(serde_json::json!({
            "source_hash": "source", "selection": "runtime_english_v1", "files": []
        }))
        .unwrap();
        assert!(!raw_entry.covers(IngestionSelection::RuntimeEnglishV1));
        assert!(!raw_entry.covers(IngestionSelection::All));
        assert!(!raw_entry.covers(IngestionSelection::ConverterInputsV1));
        let recipe_less_converter_entry: IngestionCacheEntry =
            serde_json::from_value(serde_json::json!({
                "source_hash": "source",
                "selection": "converter_inputs_v1",
                "files": []
            }))
            .unwrap();
        assert!(!recipe_less_converter_entry.covers(IngestionSelection::ConverterInputsV1));
        let mut conflicting = raw_entry.clone();
        conflicting.recipe = "all-v1".into();
        assert!(!conflicting.covers(IngestionSelection::All));
        conflicting.recipe = "unknown-v99".into();
        assert!(!conflicting.covers(IngestionSelection::RuntimeEnglishV1));
        let roundtrip: IngestionCacheEntry =
            serde_json::from_slice(&serde_json::to_vec(&pack_entry).unwrap()).unwrap();
        assert_eq!(roundtrip, pack_entry);
    }

    #[test]
    fn older_ingestion_entries_prove_all_files_and_coverage_is_directional() {
        let entry: IngestionCacheEntry = serde_json::from_value(serde_json::json!({
            "source_hash": "source", "files": [],
        }))
        .unwrap();
        assert_eq!(entry.selection, IngestionSelection::All);
        assert!(entry.covers(IngestionSelection::All));
        assert!(entry.covers(IngestionSelection::RuntimeEnglishV1));
        assert!(entry.covers(IngestionSelection::ConverterInputsV1));
        assert!(
            entry
                .selection
                .can_satisfy(IngestionSelection::RuntimeEnglishV1)
        );
        assert!(!IngestionSelection::RuntimeEnglishV1.can_satisfy(IngestionSelection::All));
        assert!(
            IngestionSelection::RuntimeEnglishV1.can_satisfy(IngestionSelection::RuntimeEnglishV1)
        );
    }

    #[test]
    fn retained_schema15_fixed_zstd_proof_preserves_quality_abi_and_cache_identity() {
        let mut config = crate::config::PipelineConfig::new("Data", "output");
        config.record_reader = crate::config::RecordReader::Legacy;
        // Exact configuration hash in the protected original schema-15
        // manifest and its metadata rebuild's retained producer provenance.
        let recorded = "b23881a864cdcfe1609779a9a27fefab2de629b50ec339101d4e0ad592e7e435";
        assert!(retained_configuration_matches(&config, 15, recorded).unwrap());
        assert_eq!(
            configuration_hash_for_schema(&config, 15).unwrap(),
            "9a58fda00b27d0f2a8e46afb9334ea869602556393a35bffd7fcb39582a08a4f"
        );
        assert!(!retained_configuration_matches(&config, 16, recorded).unwrap());
        assert!(!retained_configuration_matches(&config, 15, "different-hash").unwrap());
        let mut changed = config.clone();
        changed.texture_fallback_quality = 191;
        assert!(!retained_configuration_matches(&changed, 15, recorded).unwrap());
        let mut changed = config.clone();
        changed.script_abi_version = 2;
        assert!(!retained_configuration_matches(&changed, 15, recorded).unwrap());
        let mut changed = config;
        changed.texture_zstd_level = 7;
        assert!(!retained_configuration_matches(&changed, 15, recorded).unwrap());
    }

    #[test]
    fn prior_producers_cannot_reuse_staged_meshes_or_textures() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["old.glb", "old.ktx2"] {
            let path = directory.path().join(name);
            fs::write(&path, b"verified old bytes").unwrap();
            for schema_version in [17, 18, 19, 20, 21, 22, 23, 24, 25, 26] {
                let record = StagedOutput {
                    schema_version,
                    configuration_hash: "matching-config".to_owned(),
                    source_hash: "matching-source".to_owned(),
                    output_size: 18,
                    output_hash: hash_file(&path).unwrap(),
                };
                assert!(
                    !record.is_current(&path, "matching-source", "matching-config"),
                    "legacy producer {schema_version} accepted for {name}"
                );
            }
        }
    }

    #[test]
    fn v73_encoder_contract_participates_in_current_configuration_proof() {
        use crate::config::{PipelineConfig, TextureEncoder};
        let mut config = PipelineConfig::new("Data", "output");
        let cpu = configuration_hash(&config).unwrap();
        let legacy = configuration_hash_for_schema(&config, 18).unwrap();
        config.texture_encoder = TextureEncoder::Gpu {
            quality: 0,
            batch_mb: 4,
        };
        let gpu = configuration_hash(&config).unwrap();
        assert_ne!(
            cpu, gpu,
            "native CPU and GPU texture contracts share configuration identity"
        );
        assert_eq!(legacy, configuration_hash_for_schema(&config, 18).unwrap());
        config.texture_encoder = TextureEncoder::Gpu {
            quality: 1,
            batch_mb: 4,
        };
        let refined = configuration_hash(&config).unwrap();
        assert_ne!(
            gpu, refined,
            "GPU quality changes must invalidate output provenance"
        );
        config.texture_encoder = TextureEncoder::Gpu {
            quality: 1,
            batch_mb: 8,
        };
        assert_eq!(
            refined,
            configuration_hash(&config).unwrap(),
            "batch scheduling does not change bytes"
        );
    }

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_matches_known_padding_boundary_vectors() {
        // Fixed digests computed independently with Python's hashlib.
        let vectors = [
            (
                0,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                55,
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                63,
                "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                65,
                "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0",
            ),
        ];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("padding-vector.bin");
        for (size, expected) in vectors {
            let bytes = vec![b'a'; size];
            assert_eq!(hash_bytes(&bytes), expected, "byte vector length {size}");
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                hash_file(&path).unwrap(),
                expected,
                "file vector length {size}"
            );
        }
    }

    #[test]
    fn sha256_file_matches_known_multiblock_vector() {
        // Standard SHA-256 million-'a' vector, spanning many file read chunks.
        let bytes = vec![b'a'; 1_000_000];
        let expected = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
        assert_eq!(hash_bytes(&bytes), expected);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("million-a.bin");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(hash_file(&path).unwrap(), expected);
    }

    #[test]
    fn manifests_written_before_pruned_reference_tracking_still_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conversion-manifest.json");
        // Every key but `pruned_texture_references`, exactly as manifests were
        // written before that field existed.
        fs::write(
            &path,
            format!(
                r#"{{
                    "schema_version": {CONVERTER_SCHEMA_VERSION},
                    "complete": true,
                    "configuration_hash": "configuration",
                    "inputs_by_kind": {{"nif": 4}},
                    "failures": {{}},
                    "archives": {{}},
                    "entries": {{}}
                }}"#
            ),
        )
        .unwrap();

        let manifest = ConversionManifest::load(&path).unwrap();

        assert_eq!(manifest.schema_version, CONVERTER_SCHEMA_VERSION);
        assert!(manifest.complete);
        assert_eq!(manifest.configuration_hash, "configuration");
        assert_eq!(manifest.inputs_by_kind.get("nif"), Some(&4));
        assert!(manifest.pruned_texture_references.is_empty());
    }

    #[test]
    fn journal_keeps_the_last_record_and_drops_a_truncated_tail() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let stale = StagedOutput {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: "config".to_owned(),
            source_hash: "stale".to_owned(),
            output_size: 4,
            output_hash: "stale".to_owned(),
        };
        let current = StagedOutput {
            source_hash: "current".to_owned(),
            ..stale.clone()
        };
        let mut journal = StagingJournal::open(staging).unwrap();
        journal.record("scripts/one.pex", &stale).unwrap();
        journal.record("scripts/one.pex", &current).unwrap();
        journal.record("scripts/two.pex", &stale).unwrap();
        drop(journal);

        // A run killed mid-append leaves a partial line behind.
        let path = StagingJournal::path_in(staging);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"key\":\"scripts/three.pex\",\"schema_ver");
        fs::write(&path, &bytes).unwrap();

        let records = load_staged_outputs(staging).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records["scripts/one.pex"], current);
        assert_eq!(records["scripts/two.pex"], stale);
        assert!(!records.contains_key("scripts/three.pex"));

        // A resumed run appends after the partial line without losing its record.
        let mut journal = StagingJournal::open(staging).unwrap();
        journal.record("scripts/four.pex", &current).unwrap();
        drop(journal);
        let records = load_staged_outputs(staging).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records["scripts/four.pex"], current);
        assert!(!records.contains_key("scripts/three.pex"));

        let missing = directory.path().join("absent");
        assert!(load_staged_outputs(&missing).unwrap().is_empty());
    }

    #[test]
    fn link_or_copy_leaves_a_file_that_already_names_its_source_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rock.dds");
        fs::write(&path, b"archive bytes").unwrap();

        // `from` and `to` are the same path: removing it first would delete the only copy.
        link_or_copy(&path, &path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"archive bytes");
    }

    #[test]
    fn link_or_copy_accepts_a_destination_that_is_already_linked_to_its_source() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");
        fs::hard_link(&blob, &vfs).unwrap();

        link_or_copy(&blob, &vfs).unwrap();

        assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
    }

    #[cfg(unix)]
    #[test]
    fn restore_replaces_destination_symlinks_without_touching_their_targets() {
        use std::os::unix::fs::symlink;

        fn force_copy(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("no hard links here"))
        }
        type Restore = fn(&Path, &Path) -> std::io::Result<()>;
        let restores: [Restore; 2] = [
            |from, to| link_or_copy_with(from, to, force_copy),
            |from, to| {
                link_or_copy_spilling_with(
                    from,
                    to,
                    &SpillCache::default(),
                    &force_copy,
                    &hash_open_file,
                )
            },
        ];
        for restore in restores {
            for target_exists in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let blob = directory.path().join("blob");
                fs::write(&blob, b"archive bytes").unwrap();
                let outside = directory.path().join("outside");
                if target_exists {
                    fs::write(&outside, b"external bytes").unwrap();
                }
                let staging = directory.path().join("staging");
                fs::create_dir(&staging).unwrap();
                let destination = staging.join("restored.bin");
                symlink(&outside, &destination).unwrap();

                restore(&blob, &destination).unwrap();

                assert!(fs::symlink_metadata(&destination).unwrap().is_file());
                assert_eq!(fs::read(&destination).unwrap(), b"archive bytes");
                assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
                if target_exists {
                    assert_eq!(fs::read(&outside).unwrap(), b"external bytes");
                } else {
                    assert!(fs::symlink_metadata(&outside).is_err());
                }
            }
        }
    }

    #[test]
    fn a_full_blob_shares_one_spill_copy_instead_of_copying_per_path() {
        // Stands in for ERROR_TOO_MANY_LINKS: the original blob takes no more names, a spill does.
        fn blob_is_full(from: &Path, to: &Path) -> std::io::Result<()> {
            if from.extension().is_none() {
                return Err(std::io::Error::from(std::io::ErrorKind::TooManyLinks));
            }
            fs::hard_link(from, to)
        }
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"terrain").unwrap();
        let first = directory.path().join("first.dds");
        let second = directory.path().join("second.dds");

        let cache = SpillCache::default();
        link_or_copy_spilling_with(&blob, &first, &cache, &blob_is_full, &hash_open_file).unwrap();
        link_or_copy_spilling_with(&blob, &second, &cache, &blob_is_full, &hash_open_file).unwrap();

        // Both paths name the one spill copy: a write through one shows in the other, and the
        // blob itself is untouched.
        let spill = directory.path().join("blob.1");
        assert!(spill.is_file());
        fs::OpenOptions::new()
            .append(true)
            .open(&first)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"terrain+");
        assert_eq!(fs::read(&spill).unwrap(), b"terrain+");
        assert_eq!(fs::read(&blob).unwrap(), b"terrain");
    }

    #[test]
    fn a_link_refused_for_another_reason_still_copies_without_spilling() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");

        link_or_copy_spilling_with(
            &blob,
            &vfs,
            &SpillCache::default(),
            &|_, _| Err(std::io::Error::other("cross-volume")),
            &hash_open_file,
        )
        .unwrap();

        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
        assert!(!directory.path().join("blob.1").exists());
    }

    #[test]
    fn link_or_copy_copies_when_the_filesystem_refuses_a_link() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");

        link_or_copy_with(&blob, &vfs, |_, _| {
            Err(std::io::Error::other("no hard links here"))
        })
        .unwrap();

        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
        // The fallback is a copy, not a second name for the blob: writing through one must not
        // reach the other.
        fs::OpenOptions::new()
            .append(true)
            .open(&vfs)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes+");
    }

    #[test]
    fn v91_lod_staging_identity_excludes_lighting_and_old_lod_schemas() {
        let directory = tempfile::tempdir().unwrap();
        let config =
            crate::config::PipelineConfig::new(directory.path(), directory.path().join("output"));
        let current_hash = configuration_hash(&config).unwrap();
        for schema in [16, 17, 18, 19, 20, 21, 22, 23] {
            assert_ne!(
                current_hash,
                configuration_hash_for_schema(&config, schema).unwrap()
            );
        }
        let path = directory.path().join("mesh.glb");
        fs::write(&path, b"verified mesh").unwrap();
        let record = StagedOutput {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: "config".into(),
            source_hash: "source".into(),
            output_size: 13,
            output_hash: hash_file(&path).unwrap(),
        };
        assert!(record.is_current(&path, "source", "config"));
        for schema_version in [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26] {
            let stale = StagedOutput {
                schema_version,
                ..record.clone()
            };
            assert!(!stale.is_current(&path, "source", "config"));
        }
    }

    #[test]
    fn recent_schema_migrations_reuse_only_unchanged_asset_kinds() {
        for schema_version in [12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version,
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.glb", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.to_owned(),
                    CacheEntry {
                        source_hash: "source".to_owned(),
                        output: output.to_owned(),
                        output_size: 1,
                        output_hash: "output".to_owned(),
                    },
                );
            }
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

            let migrated = ConversionManifest::load(&path).unwrap();

            assert_eq!(migrated.schema_version, schema_version);
            assert!(!migrated.complete);
            assert!(!migrated.entries.contains_key("meshes/a.glb"));
            assert!(!migrated.entries.contains_key("textures/a.ktx2"));
            assert!(migrated.entries.contains_key("scripts/a.luau"));
        }
    }

    /// The contract list is the only thing that may change a mesh proof at a fixed producer, so
    /// an empty list has to leave the `nif` proof byte-identical and a historical producer's
    /// proof untouched whatever the list says.
    #[test]
    fn mesh_output_contracts_enter_only_the_current_producers_mesh_proof() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::PipelineConfig::new(
            directory.path().join("Data"),
            directory.path().join("output"),
        );
        let plain = |schema: u32| {
            hash_bytes(&serde_json::to_vec(&serde_json::json!({"mesh_producer": schema})).unwrap())
        };
        let nif =
            |schema: u32| asset_configuration_hash_for_schema(&config, schema, "nif").unwrap();

        assert_eq!(
            mesh_output_contracts_apply(CONVERTER_SCHEMA_VERSION),
            !MESH_OUTPUT_CONTRACTS.is_empty()
        );
        if MESH_OUTPUT_CONTRACTS.is_empty() {
            assert_eq!(
                nif(CONVERTER_SCHEMA_VERSION),
                plain(CONVERTER_SCHEMA_VERSION),
                "an empty contract list must not change the nif proof"
            );
        } else {
            assert_ne!(
                nif(CONVERTER_SCHEMA_VERSION),
                plain(CONVERTER_SCHEMA_VERSION),
                "a named contract must change the nif proof"
            );
        }
        for schema in [12, 24, 26, 27] {
            assert!(!mesh_output_contracts_apply(schema));
            assert_eq!(
                nif(schema),
                plain(schema),
                "schema {schema} gained a contract"
            );
        }
    }

    /// Producer 27 changed the GLB collision data, so no earlier producer's meshes
    /// (including the producer-24 and producer-26 contract) are reusable. Provenance
    /// is still kept as recorded, scripts stay reusable, and textures never are.
    #[test]
    fn database_only_migration_discards_meshes_of_earlier_producers() {
        for (producer, retained, compatible) in [
            (24, None, false),
            (26, None, false),
            (CONVERTER_SCHEMA_VERSION, Some(24), false),
            (CONVERTER_SCHEMA_VERSION, Some(26), false),
            (24, Some(CONVERTER_SCHEMA_VERSION), false),
            (26, Some(CONVERTER_SCHEMA_VERSION), false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version: producer,
                retained_mesh_schema_version: retained,
                retained_asset_configuration_hash: retained.map(|_| "original-config".into()),
                configuration_hash: "producer-config".into(),
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.glb", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.into(),
                    CacheEntry {
                        source_hash: "source".into(),
                        output: output.into(),
                        output_size: 1,
                        output_hash: "output".into(),
                    },
                );
            }
            manifest.save(&path).unwrap();
            let migrated = ConversionManifest::load(&path).unwrap();
            assert!(!migrated.complete);
            assert_eq!(migrated.entries.contains_key("meshes/a.glb"), compatible);
            assert!(!migrated.entries.contains_key("textures/a.ktx2"));
            assert!(migrated.entries.contains_key("scripts/a.luau"));
            assert_eq!(migrated.retained_mesh_schema_version, retained);
            assert_eq!(migrated.configuration_hash, "producer-config");
            assert_eq!(
                migrated.retained_asset_configuration_hash,
                manifest.retained_asset_configuration_hash
            );
        }
    }

    /// Unproven older contracts never gain the current mesh identity.
    #[test]
    fn current_metadata_never_promotes_an_older_or_unknown_mesh_contract() {
        for mesh_schema in [
            15,
            16,
            17,
            18,
            19,
            20,
            21,
            22,
            23,
            24,
            25,
            26,
            CONVERTER_SCHEMA_VERSION + 1,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version: CONVERTER_SCHEMA_VERSION,
                retained_mesh_schema_version: Some(mesh_schema),
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.GLB", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.to_owned(),
                    CacheEntry {
                        source_hash: "source".to_owned(),
                        output: output.to_owned(),
                        output_size: 1,
                        output_hash: "output".to_owned(),
                    },
                );
            }
            manifest.save(&path).unwrap();
            let eligible = ConversionManifest::load(&path).unwrap();
            assert!(!eligible.complete);
            assert!(!eligible.entries.contains_key("meshes/a.GLB"));
            assert!(!eligible.entries.contains_key("textures/a.ktx2"));
            assert!(eligible.entries.contains_key("scripts/a.luau"));
        }
    }
}
