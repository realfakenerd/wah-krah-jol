use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const CONVERTER_SCHEMA_VERSION: u32 = 14;

/// Provenance journal the converter keeps inside a staging directory.
///
/// A staging directory outlives the run that filled it, so a resumed run finds
/// outputs this process did not write. The journal records, per output, the
/// source hash, the converter schema and the configuration hash it was produced
/// under, together with the output's size and hash, appended as the output is
/// written. A resumed run reuses a staged output only while its record still
/// matches the current source, schema and configuration and the bytes on disk.
/// The name is dotted so it can never collide with a converted asset, and the
/// file is dropped from the output directory once the staging directory has
/// been published.
pub const STAGING_JOURNAL_FILE: &str = ".conversion-staging-journal.jsonl";

/// What a staged output was produced from, as recorded when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedOutput {
    pub schema_version: u32,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestedFile {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestionCacheEntry {
    pub source_hash: String,
    pub files: Vec<IngestedFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversionManifest {
    pub schema_version: u32,
    pub complete: bool,
    #[serde(default)]
    pub configuration_hash: String,
    #[serde(default)]
    pub inputs_by_kind: BTreeMap<String, u64>,
    #[serde(default)]
    pub failures: BTreeMap<String, String>,
    #[serde(default)]
    pub archives: BTreeMap<String, IngestionCacheEntry>,
    pub entries: BTreeMap<String, CacheEntry>,
}

impl ConversionManifest {
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
        if matches!(manifest.schema_version, 12 | 13) && CONVERTER_SCHEMA_VERSION == 14 {
            // Schemas 13/14 change only NIF material publication and LAND
            // normalization. Preserve verified archive ingestion, textures,
            // and scripts, but force every GLB plus the always-rebuilt world
            // database and cell cache through the new contracts.
            manifest.complete = false;
            manifest
                .entries
                .retain(|_, entry| !entry.output.to_ascii_lowercase().ends_with(".glb"));
            return Ok(manifest);
        }
        if manifest.schema_version != CONVERTER_SCHEMA_VERSION {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
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

pub fn configuration_hash(config: &crate::config::PipelineConfig) -> Result<String> {
    configuration_hash_for_schema(config, CONVERTER_SCHEMA_VERSION)
}

pub fn configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    let relevant = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_etc1s_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "script_abi_version": config.script_abi_version,
    });
    Ok(hash_bytes(&serde_json::to_vec(&relevant)?))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path)
        .wrap_err_with(|| format!("failed to open {} for hashing", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .wrap_err_with(|| format!("failed to hash {}", path.display()))?;
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
    fn sha256_is_stable() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
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
    fn recent_schema_migrations_reuse_only_unchanged_asset_kinds() {
        for schema_version in [12, 13] {
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
            assert!(migrated.entries.contains_key("textures/a.ktx2"));
            assert!(migrated.entries.contains_key("scripts/a.luau"));
        }
    }
}
