use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

/// Record frontend used to generate the runtime database and terrain cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordReader {
    /// Hand-written record reader, available through an explicit choice.
    Legacy,
    /// MudCrab's default schema-driven Rust decoder.
    #[default]
    Inhouse,
}

/// Which encoder turns DDS textures into UASTC KTX2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureEncoder {
    /// Basis Universal on the CPU (UASTC level `texture_uastc_level`).
    #[default]
    Cpu,
    /// wgpu compute shader, batched across textures (`texture_gpu`).
    /// Falls back to the CPU encoder when no GPU is available.
    Gpu {
        /// Endpoint refinement passes (0 = fastest).
        quality: u32,
        /// Texel megabytes packed into one GPU dispatch.
        batch_mb: u64,
    },
}

/// Which texture entry 0 of the land texture table stands for (#215): the quadrant base of a
/// cell with no BTXT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerrainDefaultTexture {
    /// A generated white diffuse, with no normal map.
    #[default]
    White,
    /// `[Landscape] sDefaultLandDiffuseTexture` and `sDefaultLandNormalTexture` of a Skyrim.ini
    /// (`terrain_ini`), or the built-in Dirt02 paths without one.
    SkyrimIni,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
    #[serde(skip)]
    pub resume_staging: Option<PathBuf>,
    /// Persistent ingestion-cache root. Defaults to `<output>.assets-cache`
    /// alongside the output directory; the runtime pack no longer contains it.
    #[serde(skip)]
    pub cache_dir: Option<PathBuf>,
    /// Explicit player selection; absent uses MO2 when configured, else automatic discovery.
    pub plugins_file: Option<PathBuf>,
    /// Resolve this MO2 profile over physical Data; incompatible with plugins_file.
    #[serde(default)]
    pub mo2: Option<mo2::Selection>,
    /// Defaults to the in-house reader when older serialized configurations omit it.
    #[serde(default)]
    pub record_reader: RecordReader,
    /// Ordered schema JSON add-ons for the in-house reader.
    #[serde(default)]
    pub schema_addons: Vec<PathBuf>,
    /// A run freezes schema bytes before creating or recovering output.
    #[serde(skip)]
    pub(crate) runtime_schemas: Option<std::sync::Arc<crate::records::addons::RuntimeSchemas>>,
    /// Explicit Creation list; absent uses Skyrim.ccc beside Data with the built-in fallback.
    #[serde(default)]
    pub skyrim_ccc_file: Option<PathBuf>,
    /// Explicit General sTestFile settings; no ambient profile is read.
    #[serde(default)]
    pub skyrim_ini_file: Option<PathBuf>,
    /// Explicit per-worldspace LOD origins for custom worlds, keyed by
    /// worldspace editor id: `[grid_x, grid_y]`. Installed worlds read
    /// `lodsettings/<WorldspaceEDID>.lod` instead; a world with neither gets
    /// no LOD, never an assumed origin of zero (GEOM-02).
    #[serde(default)]
    pub lod_origins: BTreeMap<String, [i32; 2]>,
    /// Skip generated terrain LOD in both conversion routes. Asset byte contracts
    /// are unchanged; generated database/LOD outputs are rebuilt on every run.
    #[serde(default)]
    pub no_lod: bool,
    /// Build the merged-terrain outputs (#215) after the world database and the land textures:
    /// the land texture arrays, one merged mesh per exterior cell and the terrain chunk files.
    /// Off by default, because the engine half that reads them has not landed, and converted
    /// asset bytes are unchanged either way; generated terrain outputs are rebuilt every run.
    #[serde(default)]
    pub merged_terrain: bool,
    /// Where entry 0 of the land texture table comes from; `white` unless a user selects the
    /// game's own default.
    #[serde(default)]
    pub terrain_default_texture: TerrainDefaultTexture,
    /// The `Skyrim.ini` a `skyrim_ini` default texture is read from. `None` uses the built-in
    /// `textures/landscape/Dirt02.dds` and `Dirt02_N.dds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain_ini: Option<PathBuf>,
    /// Passed to the land texture array builder
    /// (`terrain_arrays::ArrayOptions::allow_downscale`): a land texture larger than the largest
    /// size class is downscaled into it, and the report labels every one, instead of stopping the
    /// merged-terrain build.
    #[serde(default)]
    pub terrain_allow_downscale: bool,
    pub cpu_jobs: usize,
    pub io_jobs: usize,
    pub enable_ba2: bool,
    pub fail_fast: bool,
    pub invalidate_cache: bool,
    pub verify_cache: bool,
    /// Include archive entries beyond the runtime inputs and English string banks.
    #[serde(default)]
    pub extract_all_archive_files: bool,
    /// Use durable batch packs, additionally flush derived payloads, or skip extraction flushes.
    #[serde(default)]
    pub ingestion_sync: crate::archive::IngestionSync,
    /// Quality for the UASTC fallback path (uncompressed/legacy sources).
    /// Named `texture_etc1s_quality` in serialized configs for compatibility;
    /// it never selected ETC1S encoding, which the Bevy runtime rejects.
    #[serde(rename = "texture_etc1s_quality", alias = "texture_fallback_quality")]
    pub texture_fallback_quality: u8,
    pub texture_uastc_level: u8,
    /// Zstandard level for per-mip KTX2 supercompression (0 = off).
    #[serde(default = "default_texture_zstd_level")]
    pub texture_zstd_level: i32,
    #[serde(default)]
    pub texture_encoder: TextureEncoder,
    /// Encoder for generated terrain atlases. New configs use GPU encoding;
    /// an omitted serialized field retains the legacy CPU recipe.
    #[serde(default)]
    pub lod_texture_encoder: TextureEncoder,
    /// Script output ABI; full conversion raises saved values to the current emitter ABI.
    pub script_abi_version: u32,
    /// Language of the string tables localized plugins' names are read from, as in Skyrim's
    /// `sLanguage`: `Strings/<plugin>_<language>.STRINGS`.
    #[serde(default = "default_language")]
    pub language: String,
}

/// The language Skyrim's own `sLanguage` defaults to.
pub const DEFAULT_LANGUAGE: &str = "english";

fn default_language() -> String {
    DEFAULT_LANGUAGE.to_owned()
}

fn default_texture_zstd_level() -> i32 {
    6
}

impl PipelineConfig {
    /// Read all configured add-ons once, rejecting the batch before output work begins.
    pub(crate) fn freeze_schema_addons(&mut self) -> color_eyre::Result<()> {
        self.runtime_schemas = Some(std::sync::Arc::new(
            crate::records::addons::RuntimeSchemas::load(&self.schema_addons)
                .map_err(color_eyre::eyre::Error::msg)?,
        ));
        Ok(())
    }

    /// Use the frozen run schema, or load a temporary snapshot for settings inspection.
    pub(crate) fn schemas(
        &self,
    ) -> color_eyre::Result<std::sync::Arc<crate::records::addons::RuntimeSchemas>> {
        if let Some(schemas) = &self.runtime_schemas {
            return Ok(schemas.clone());
        }
        Ok(std::sync::Arc::new(
            crate::records::addons::RuntimeSchemas::load(&self.schema_addons)
                .map_err(color_eyre::eyre::Error::msg)?,
        ))
    }

    /// A configuration with the default settings for converting `data_dir` into `output_dir`.
    pub fn new(data_dir: impl Into<PathBuf>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            output_dir: output_dir.into(),
            resume_staging: None,
            cache_dir: None,
            plugins_file: None,
            mo2: None,
            record_reader: RecordReader::default(),
            schema_addons: Vec::new(),
            runtime_schemas: None,
            skyrim_ccc_file: None,
            skyrim_ini_file: None,
            lod_origins: BTreeMap::new(),
            no_lod: false,
            merged_terrain: false,
            terrain_default_texture: TerrainDefaultTexture::default(),
            terrain_ini: None,
            terrain_allow_downscale: false,
            cpu_jobs: std::thread::available_parallelism().map_or(1, usize::from),
            io_jobs: 2,
            enable_ba2: true,
            fail_fast: false,
            invalidate_cache: false,
            verify_cache: true,
            extract_all_archive_files: false,
            ingestion_sync: crate::archive::IngestionSync::Archive,
            texture_fallback_quality: 192,
            texture_uastc_level: 2,
            texture_zstd_level: default_texture_zstd_level(),
            texture_encoder: TextureEncoder::Cpu,
            lod_texture_encoder: TextureEncoder::Gpu {
                quality: crate::texture_gpu::DEFAULT_QUALITY,
                batch_mb: crate::texture_gpu::DEFAULT_BATCH_MB,
            },
            script_abi_version: crate::script::SCRIPT_ABI_VERSION,
            language: default_language(),
        }
    }

    /// Resolves the persistent ingestion-cache root outside the published pack.
    pub fn ingestion_cache_dir(&self) -> PathBuf {
        if let Some(dir) = &self.cache_dir {
            return dir.clone();
        }
        let file_name = self
            .output_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("modern_assets");
        self.output_dir
            .with_file_name(format!("{file_name}.assets-cache"))
    }

    /// Checks that the directories and settings can be used for a run.
    pub(crate) fn validate(&self) -> color_eyre::Result<()> {
        color_eyre::eyre::ensure!(
            self.schema_addons.is_empty() || self.record_reader == RecordReader::Inhouse,
            "schema add-ons require --record-reader inhouse"
        );
        // The merged-terrain settings only mean something to a merged-terrain build, and the
        // Skyrim.ini only to the skyrim-ini default. Checked here so a programmatic config is held to
        // the same rules as the command line.
        let terrain_default = TerrainDefaultTexture::default();
        color_eyre::eyre::ensure!(
            self.merged_terrain
                || (self.terrain_default_texture == terrain_default
                    && self.terrain_ini.is_none()
                    && !self.terrain_allow_downscale),
            "merged terrain settings require merged_terrain"
        );
        if let Some(ini) = &self.terrain_ini {
            color_eyre::eyre::ensure!(
                self.merged_terrain
                    && self.terrain_default_texture == TerrainDefaultTexture::SkyrimIni,
                "terrain_ini requires merged_terrain and the skyrim-ini default texture"
            );
            color_eyre::eyre::ensure!(
                ini.is_file(),
                "terrain_ini is not a readable file: {}",
                ini.display()
            );
        }
        color_eyre::eyre::ensure!(
            self.data_dir.is_dir(),
            "Skyrim Data directory does not exist: {}",
            self.data_dir.display()
        );
        color_eyre::eyre::ensure!(self.cpu_jobs > 0, "cpu_jobs must be greater than zero");
        color_eyre::eyre::ensure!(self.io_jobs > 0, "io_jobs must be greater than zero");
        color_eyre::eyre::ensure!(
            self.ingestion_sync != crate::archive::IngestionSync::None || self.verify_cache,
            "ingestion_sync=none requires verify_cache"
        );
        color_eyre::eyre::ensure!(
            (1..=255).contains(&self.texture_fallback_quality),
            "texture_fallback_quality must be between 1 and 255"
        );
        color_eyre::eyre::ensure!(
            self.texture_uastc_level <= 4,
            "texture_uastc_level must be between 0 and 4"
        );
        for encoder in [self.texture_encoder, self.lod_texture_encoder] {
            if let TextureEncoder::Gpu { quality, batch_mb } = encoder {
                color_eyre::eyre::ensure!(quality <= 8, "GPU quality must be between 0 and 8");
                color_eyre::eyre::ensure!(
                    (1..=4096).contains(&batch_mb),
                    "GPU batch size must be between 1 and 4096 MiB"
                );
            }
        }
        color_eyre::eyre::ensure!(
            (0..=22).contains(&self.texture_zstd_level),
            "texture_zstd_level must be between 0 and 22"
        );
        // Part of a file name: `Strings/<plugin>_<language>.STRINGS`.
        color_eyre::eyre::ensure!(
            !self.language.is_empty()
                && self
                    .language
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_'),
            "language must be a name such as english, made of letters, digits and underscores"
        );
        // Compared as resolved folders, not as spelled: the output is replaced
        // on publish, staging is deleted after it, and the cache is pruned, so
        // none of them may be, hold, or sit inside the game data.
        let data = std::fs::canonicalize(&self.data_dir)?;
        let mut sources = vec![data.clone()];
        if let Some(selection) = &self.mo2 {
            color_eyre::eyre::ensure!(
                self.plugins_file.is_none(),
                "MO2 selection cannot be combined with plugins_file"
            );
            let instance = mo2::Instance::open(&selection.instance_path)?;
            instance.profile_dir(&selection.profile)?;
            sources.extend(
                instance
                    .source_roots()
                    .iter()
                    .map(|path| path.to_path_buf()),
            );
        }
        let mut written = vec![
            ("output directory", self.output_dir.clone()),
            ("ingestion cache directory", self.ingestion_cache_dir()),
        ];
        if let Some(staging) = &self.resume_staging {
            written.push(("resume staging directory", staging.clone()));
        }
        for (role, path) in written {
            let resolved = resolve_path(&path)?;
            for source in &sources {
                color_eyre::eyre::ensure!(
                    !(resolved.starts_with(source) || source.starts_with(&resolved)),
                    "the {role} {} overlaps the Skyrim Data directory or MO2 source {}; choose a folder outside it that does not contain it",
                    path.display(),
                    source.display()
                );
            }
        }
        check_output_dir(&self.output_dir)?;
        if let Some(staging) = &self.resume_staging {
            color_eyre::eyre::ensure!(
                staging.is_dir(),
                "resume staging directory does not exist: {}",
                staging.display()
            );
            let output_name = self
                .output_dir
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| color_eyre::eyre::eyre!("output directory has no valid name"))?;
            let staging_name = staging
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            color_eyre::eyre::ensure!(
                staging_name.starts_with(&format!("{output_name}.staging-")),
                "resume directory is not a staging directory for {}",
                self.output_dir.display()
            );
            let output_parent = parent_or_cwd(&self.output_dir);
            let staging_parent = parent_or_cwd(staging);
            color_eyre::eyre::ensure!(
                std::fs::canonicalize(output_parent)? == std::fs::canonicalize(staging_parent)?,
                "resume directory must share the output directory parent"
            );
            // The persistent cache is written after publishing, just before staging is removed,
            // so a cache folder inside the resume folder would be deleted with it.
            let cache = self.ingestion_cache_dir();
            color_eyre::eyre::ensure!(
                !resolve_path(&cache)?.starts_with(std::fs::canonicalize(staging)?),
                "ingestion cache directory {} must not be inside the resume staging directory {}",
                cache.display(),
                staging.display()
            );
        }
        Ok(())
    }
}

/// Why [`check_output_dir`] refused a folder.
#[derive(Debug)]
pub enum OutputDirError {
    /// The path exists but is not a directory.
    NotADirectory(PathBuf),
    /// The directory has files in it but no `conversion-manifest.json`, so it
    /// is not an earlier conversion. Publishing replaces the whole folder, so
    /// converting into it would delete what is there.
    NotConverterOutput(PathBuf),
    /// The path could not be inspected.
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for OutputDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADirectory(path) => {
                write!(f, "output path is not a directory: {}", path.display())
            }
            Self::NotConverterOutput(path) => write!(
                f,
                "output directory {} is not empty and is not an earlier conversion (no \
                 conversion-manifest.json); converting into it would delete its contents, \
                 so choose an empty or new folder",
                path.display()
            ),
            Self::Unreadable { path, source } => {
                write!(
                    f,
                    "cannot read output directory {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for OutputDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Whether `output` is safe to convert into. Publishing replaces the output
/// folder as a whole, so only a folder that does not exist yet, an empty one,
/// or an earlier conversion (it has `conversion-manifest.json`) is accepted.
/// The pipeline checks this before it starts and again just before it
/// publishes; front ends can call it to warn before a run.
pub fn check_output_dir(output: &Path) -> Result<(), OutputDirError> {
    let unreadable = |source| OutputDirError::Unreadable {
        path: output.to_path_buf(),
        source,
    };
    let metadata = match std::fs::metadata(output) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(unreadable(error)),
    };
    if !metadata.is_dir() {
        return Err(OutputDirError::NotADirectory(output.to_path_buf()));
    }
    // Listed first, even when a manifest is there: publishing moves and then
    // deletes the whole folder, so a folder that cannot be listed cannot be
    // checked or safely replaced.
    let mut entries = std::fs::read_dir(output).map_err(unreadable)?;
    match entries.next() {
        None => return Ok(()),
        Some(Err(error)) => return Err(unreadable(error)),
        Some(Ok(_)) => {}
    }
    if output.join("conversion-manifest.json").is_file() {
        return Ok(());
    }
    Err(OutputDirError::NotConverterOutput(output.to_path_buf()))
}

/// `path` made absolute with its symbolic links and junctions resolved, as far
/// as it exists: the nearest existing ancestor is canonicalised and the rest
/// appended, so a folder that has not been created yet still compares with
/// the folders it would sit in or hold.
fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(resolved) => {
                return Ok(missing.iter().rev().fold(resolved, |mut resolved, part| {
                    if *part == std::ffi::OsStr::new("..") {
                        resolved.pop();
                    } else if *part != std::ffi::OsStr::new(".") {
                        resolved.push(part);
                    }
                    resolved
                }));
            }
            Err(error) => match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_owned());
                    existing = parent;
                }
                _ => return Err(error),
            },
        }
    }
}

/// The staging folder an unfinished conversion into `output` left behind, for
/// `--resume-staging` (`PipelineConfig::resume_staging`): the newest
/// `<output>.staging-<pid>-<stamp>` directory beside `output`, and how many
/// older staging folders were passed over, which a front end can offer to
/// delete. It checks only what a resume checks before it starts (the name and
/// the parent folder); the resume itself re-verifies the files inside. Other
/// folders the converter keeps beside the output are never returned.
pub fn find_resumable_staging(output: &Path) -> Option<(PathBuf, usize)> {
    let prefix = format!("{}.staging-", output.file_name()?.to_str()?);
    let mut candidates: Vec<(u128, PathBuf)> = std::fs::read_dir(parent_or_cwd(output))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let suffix = name.to_str()?.strip_prefix(&prefix)?;
            // `<pid>-<stamp>`, the stamp in nanoseconds since the epoch; a
            // folder named some other way sorts oldest.
            let stamp = suffix.rsplit('-').next()?.parse().unwrap_or(0);
            Some((stamp, entry.path()))
        })
        .collect();
    candidates.sort();
    let (_, newest) = candidates.pop()?;
    Some((newest, candidates.len()))
}

// `Path::parent` returns `Some("")` for bare file names, so empty parents
// must also fall back to the current directory.
fn parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A language is part of a string table's file name, so only letters, digits and underscores
    /// are accepted.
    #[test]
    fn language_must_be_usable_in_a_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Data");
        std::fs::create_dir(&data).unwrap();
        let mut config = PipelineConfig::new(&data, dir.path().join("pack"));
        assert_eq!(config.language, "english");
        for language in ["german", "chinese_traditional"] {
            config.language = language.into();
            config.validate().unwrap();
        }
        for language in ["", "../english", "en glish", "english.strings"] {
            config.language = language.into();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("language must be"), "{language:?}: {error}");
        }
    }

    /// The merged-terrain settings are accepted only with the option, the ini only with the
    /// skyrim-ini default, and the ini must be a file that exists.
    #[test]
    fn terrain_settings_need_the_merged_terrain_option_and_an_existing_ini() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Data");
        std::fs::create_dir(&data).unwrap();
        let ini = dir.path().join("Skyrim.ini");
        std::fs::write(&ini, "[Landscape]\n").unwrap();
        let mut config = PipelineConfig::new(&data, dir.path().join("pack"));
        config.validate().unwrap();

        config.terrain_allow_downscale = true;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("require merged_terrain"), "{error}");
        config.terrain_allow_downscale = false;
        config.terrain_default_texture = TerrainDefaultTexture::SkyrimIni;
        assert!(config.validate().is_err());
        config.terrain_default_texture = TerrainDefaultTexture::White;

        // An ini needs the option and the skyrim-ini default; white with an ini is refused.
        config.terrain_ini = Some(ini.clone());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("merged_terrain"), "{error}");
        config.merged_terrain = true;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("skyrim-ini default texture"), "{error}");

        config.terrain_default_texture = TerrainDefaultTexture::SkyrimIni;
        config.validate().unwrap();
        config.terrain_ini = Some(dir.path().join("Missing.ini"));
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("not a readable file"), "{error}");
        config.terrain_ini = Some(dir.path().to_path_buf());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("not a readable file"), "{error}");
    }

    /// New and older unstamped configurations use in-house while explicit legacy stays selected.
    #[test]
    fn record_reader_defaults_to_inhouse_and_preserves_serialized_choices() {
        let config = PipelineConfig::new("Data", "pack");
        assert_eq!(config.record_reader, RecordReader::Inhouse);
        assert_eq!(RecordReader::default(), RecordReader::Inhouse);

        let mut serialized = serde_json::to_value(&config).unwrap();
        serialized.as_object_mut().unwrap().remove("record_reader");
        let unstamped: PipelineConfig = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(unstamped.record_reader, RecordReader::Inhouse);

        serialized["record_reader"] = serde_json::json!("legacy");
        let legacy: PipelineConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(legacy.record_reader, RecordReader::Legacy);
    }

    /// Older serialized configs keep default selection inputs and explicit LOD origins.
    #[test]
    fn new_configs_default_to_gpu_lod_and_round_trip_the_selected_recipe() {
        let config = PipelineConfig::new("Data", "modern_assets");
        let expected = TextureEncoder::Gpu {
            quality: crate::texture_gpu::DEFAULT_QUALITY,
            batch_mb: crate::texture_gpu::DEFAULT_BATCH_MB,
        };
        assert_eq!(config.lod_texture_encoder, expected);
        assert_eq!(config.texture_encoder, TextureEncoder::Cpu);
        assert_eq!(TextureEncoder::default(), TextureEncoder::Cpu);
        let restored: PipelineConfig =
            serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        assert_eq!(restored.lod_texture_encoder, expected);
        assert_eq!(restored.texture_encoder, TextureEncoder::Cpu);
    }

    #[test]
    fn missing_lod_encoder_keeps_cpu_even_when_source_texture_encoding_is_gpu() {
        let mut config = PipelineConfig::new("Data", "modern_assets");
        config.texture_encoder = TextureEncoder::Gpu {
            quality: 3,
            batch_mb: 16,
        };
        let mut legacy = serde_json::to_value(&config).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("lod_texture_encoder");
        let restored: PipelineConfig = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.lod_texture_encoder, TextureEncoder::Cpu);
        assert_eq!(restored.texture_encoder, config.texture_encoder);

        config.lod_texture_encoder = TextureEncoder::Cpu;
        let explicit_cpu: PipelineConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(explicit_cpu.lod_texture_encoder, TextureEncoder::Cpu);
        assert_eq!(explicit_cpu.texture_encoder, config.texture_encoder);
    }

    #[test]
    fn config_v90_accepts_legacy_configs_without_inventing_lod_origins() {
        let mut legacy = serde_json::json!({
            "data_dir": "Data",
            "output_dir": "modern_assets",
            "plugins_file": null,
            "cpu_jobs": 2,
            "io_jobs": 2,
            "enable_ba2": true,
            "fail_fast": false,
            "invalidate_cache": false,
            "verify_cache": true,
            "texture_etc1s_quality": 192,
            "texture_uastc_level": 2,
            "texture_zstd_level": 6,
            "script_abi_version": 1
        });
        let config: PipelineConfig = serde_json::from_value(legacy.clone()).unwrap();
        assert!(config.skyrim_ccc_file.is_none());
        assert!(config.skyrim_ini_file.is_none());
        assert!(config.mo2.is_none());
        assert_eq!(config.record_reader, RecordReader::Inhouse);
        assert!(config.schema_addons.is_empty());
        assert!(config.lod_origins.is_empty());
        assert!(!config.no_lod);
        assert!(!config.extract_all_archive_files);
        assert_eq!(
            config.ingestion_sync,
            crate::archive::IngestionSync::PerFile
        );
        assert_eq!(config.texture_encoder, TextureEncoder::Cpu);
        assert_eq!(config.lod_texture_encoder, TextureEncoder::Cpu);
        assert_eq!(config.data_dir, PathBuf::from("Data"));
        assert_eq!(config.cpu_jobs, 2);

        legacy["lod_origins"] = serde_json::json!({"GeneratedWorld": [-4, 12]});
        let explicit: PipelineConfig = serde_json::from_value(legacy).unwrap();
        assert_eq!(explicit.lod_origins["GeneratedWorld"], [-4, 12]);
    }

    /// Mixed source selection is rejected before opening an MO2 instance or starting output work.
    #[test]
    fn mo2_and_player_selection_are_rejected_before_instance_open() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        std::fs::create_dir(&data).unwrap();
        let output = directory.path().join("pack");
        let mut config = PipelineConfig::new(data, &output);
        config.plugins_file = Some(directory.path().join("plugins.txt"));
        config.mo2 = Some(mo2::Selection {
            instance_path: directory.path().join("absent-instance"),
            profile: "Absent".into(),
        });
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("MO2 selection cannot be combined with plugins_file")
        );
        assert!(!output.exists());
    }

    #[test]
    fn no_ingestion_sync_requires_cache_verification() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        std::fs::create_dir(&data).unwrap();
        let mut config = PipelineConfig::new(data, directory.path().join("output"));
        config.ingestion_sync = crate::archive::IngestionSync::None;
        config.validate().unwrap();
        config.verify_cache = false;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("requires verify_cache")
        );
    }

    #[test]
    fn finds_the_newest_staging_folder_beside_the_output() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        assert_eq!(find_resumable_staging(&output), None);

        for name in [
            "modern_assets.staging-7-100",
            "modern_assets.staging-9-300",
            "modern_assets.staging-8-200",
            // Not staging for this output: another output's, the publish pack,
            // the persistent cache, and a file with a staging name.
            "other.staging-1-999",
            "modern_assets.pack-1-999",
            "modern_assets.assets-cache",
        ] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        std::fs::write(directory.path().join("modern_assets.staging-1-999"), b"").unwrap();

        assert_eq!(
            find_resumable_staging(&output),
            Some((directory.path().join("modern_assets.staging-9-300"), 2))
        );
    }

    #[test]
    fn a_cache_folder_inside_the_resume_staging_folder_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        std::fs::create_dir_all(&data).unwrap();
        let output = directory.path().join("modern");
        let staging = directory.path().join("modern.staging-1-1");
        std::fs::create_dir_all(&staging).unwrap();

        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging.clone());
        // The default cache sits beside the output, outside staging.
        config.validate().unwrap();

        // A cache folder that does not exist yet, inside the resume folder.
        config.cache_dir = Some(staging.join("cache"));
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("must not be inside the resume staging"),
            "{error}"
        );
        // The resume folder itself.
        config.cache_dir = Some(staging.clone());
        assert!(config.validate().is_err());
        // Beside it is fine.
        config.cache_dir = Some(directory.path().join("cache"));
        config.validate().unwrap();
    }

    #[test]
    fn a_resume_staging_folder_holding_the_data_folder_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern");
        let staging = directory.path().join("modern.staging-1-1");
        let data = staging.join("Data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("Skyrim.esm"), b"game data").unwrap();

        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging.clone());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("overlaps the Skyrim Data directory"),
            "{error}"
        );

        // The same folder is refused when it is the data folder itself.
        let mut config = PipelineConfig::new(&staging, &output);
        config.resume_staging = Some(staging.clone());
        assert!(config.validate().is_err());

        assert_eq!(
            std::fs::read(data.join("Skyrim.esm")).unwrap(),
            b"game data"
        );
        assert!(!output.exists());

        // A staging folder beside the data folder still passes.
        let other_data = directory.path().join("Data");
        std::fs::create_dir_all(&other_data).unwrap();
        let mut config = PipelineConfig::new(&other_data, &output);
        config.resume_staging = Some(staging);
        config.validate().unwrap();
    }

    #[test]
    fn output_dir_check_accepts_new_empty_and_earlier_output_only() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert!(check_output_dir(&missing).is_ok());

        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(check_output_dir(&empty).is_ok());

        let earlier = directory.path().join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        std::fs::write(earlier.join("conversion-manifest.json"), b"{}").unwrap();
        std::fs::write(earlier.join("skyrim_world.db"), b"").unwrap();
        assert!(check_output_dir(&earlier).is_ok());

        let foreign = directory.path().join("Games");
        std::fs::create_dir_all(foreign.join("Skyrim")).unwrap();
        assert!(matches!(
            check_output_dir(&foreign),
            Err(OutputDirError::NotConverterOutput(_))
        ));
        // A folder named like the manifest is not a manifest.
        let dir_manifest = directory.path().join("dir-manifest");
        std::fs::create_dir_all(dir_manifest.join("conversion-manifest.json")).unwrap();
        assert!(matches!(
            check_output_dir(&dir_manifest),
            Err(OutputDirError::NotConverterOutput(_))
        ));

        let file = directory.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(
            check_output_dir(&file),
            Err(OutputDirError::NotADirectory(_))
        ));
    }

    /// A config with an existing Data folder, for the overlap checks.
    fn config_with_data(root: &Path) -> (PathBuf, PipelineConfig) {
        let data = root.join("Data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("Skyrim.esm"), b"keep me").unwrap();
        let config = PipelineConfig::new(&data, root.join("modern"));
        (data, config)
    }

    #[test]
    fn folders_the_run_writes_must_not_overlap_the_data_folder() {
        let directory = tempfile::tempdir().unwrap();
        let (data, config) = config_with_data(directory.path());
        assert!(config.validate().is_ok(), "a sibling output is fine");

        // An earlier conversion that holds the Data folder: publishing would
        // move it aside and delete it, Data included.
        let mut holding = config.clone();
        holding.output_dir = directory.path().to_path_buf();
        std::fs::write(directory.path().join("conversion-manifest.json"), b"{}").unwrap();
        let error = holding.validate().unwrap_err().to_string();
        assert!(
            error.contains("overlaps the Skyrim Data directory"),
            "{error}"
        );

        // The Data folder spelled another way.
        let mut same = config.clone();
        same.output_dir = directory.path().join("Data").join("..").join("Data");
        assert!(same.validate().is_err());

        // Output, cache or resume staging inside the Data folder.
        let mut inside = config.clone();
        inside.output_dir = data.join("modern");
        assert!(inside.validate().is_err());
        let mut cache = config.clone();
        cache.cache_dir = Some(data.join("cache"));
        assert!(cache.validate().is_err());

        // A resume staging folder that holds the Data folder: staging is
        // deleted after a successful publish.
        let staging = directory.path().join("modern.staging-1-1");
        let nested = staging.join("Data");
        std::fs::create_dir_all(&nested).unwrap();
        let mut resume = PipelineConfig::new(&nested, directory.path().join("modern"));
        resume.resume_staging = Some(staging);
        let error = resume.validate().unwrap_err().to_string();
        assert!(error.contains("resume staging directory"), "{error}");

        assert_eq!(std::fs::read(data.join("Skyrim.esm")).unwrap(), b"keep me");
    }

    #[cfg(unix)]
    #[test]
    fn an_output_folder_that_cannot_be_listed_is_refused_even_with_a_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("conversion-manifest.json"), b"{}").unwrap();
        // Search but no read permission: the manifest can be opened by name,
        // the folder cannot be listed.
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o111)).unwrap();
        let listable = std::fs::read_dir(&output).is_ok();
        let result = check_output_dir(&output);
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            // Running as root: permissions are not enforced, nothing to check.
            return;
        }
        assert!(matches!(result, Err(OutputDirError::Unreadable { .. })));
    }

    #[test]
    fn bare_relative_names_resolve_to_the_current_directory() {
        assert_eq!(parent_or_cwd(Path::new("modern_assets")), Path::new("."));
        assert_eq!(
            parent_or_cwd(Path::new("modern_assets.staging-1")),
            Path::new(".")
        );
        assert_eq!(parent_or_cwd(Path::new("./modern_assets")), Path::new("."));
        assert_eq!(
            parent_or_cwd(Path::new("/data/modern_assets")),
            Path::new("/data")
        );
        assert_eq!(parent_or_cwd(Path::new("/data")), Path::new("/"));
    }
}
