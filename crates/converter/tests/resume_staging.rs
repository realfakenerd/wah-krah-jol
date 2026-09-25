//! A resumed run must never certify a staged output that the current source,
//! converter schema or configuration did not produce.
//!
//! Every test builds a staging directory by hand: a marker file the converters
//! never emit stands in for a stale output, with a journal record describing
//! it. Reuse is then observable as the marker surviving the run; reconversion
//! replaces it with the bytes a fresh run produces.

use converter::{
    AssetPipeline, PipelineConfig, PipelineReport,
    cache::{
        CONVERTER_SCHEMA_VERSION, StagedOutput, StagingJournal, configuration_hash, hash_bytes,
        hash_file, load_staged_outputs,
    },
};
use std::{fs, path::PathBuf};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A marker standing in for the output of a previous run. It compiles as Luau,
/// so artifact validation accepts it, but no conversion produces it.
const MARKER: &[u8] = b"-- staged output from an earlier run\nreturn {}\n";

const KEY: &str = "scripts/one.pex";

struct Fixture {
    _temp: TempDir,
    data: PathBuf,
    output: PathBuf,
    staging: PathBuf,
    config: PipelineConfig,
}

impl Fixture {
    /// A data directory holding one script, and a staging directory named the
    /// way `PipelineConfig::validate` requires for `--resume-staging`.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        let staging = output.with_extension(format!("staging-{}-1", std::process::id()));
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::create_dir_all(&staging).unwrap();
        let config = PipelineConfig::new(&data, &output);
        Self {
            _temp: temp,
            data,
            output,
            staging,
            config,
        }
    }

    fn write_source(&self, name: &str) {
        fs::write(self.data.join("scripts/one.pex"), script(name)).unwrap();
    }

    /// Stages `MARKER` as the output of the current source.
    fn stage_marker(&self) {
        let target = self.staging.join("scripts/one.luau");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, MARKER).unwrap();
    }

    /// The record a previous run would have written for the staged output: the
    /// current source, schema and configuration, and the marker's own bytes.
    fn marker_record(&self) -> StagedOutput {
        StagedOutput {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: configuration_hash(&self.config).unwrap(),
            source_hash: hash_bytes(&fs::read(self.data.join("scripts/one.pex")).unwrap()),
            output_size: MARKER.len() as u64,
            output_hash: hash_bytes(MARKER),
        }
    }

    fn write_record(&self, record: &StagedOutput) {
        let mut journal = StagingJournal::open(&self.staging).unwrap();
        journal.record(KEY, record).unwrap();
    }

    fn resumed_config(&self) -> PipelineConfig {
        let mut config = self.config.clone();
        config.resume_staging = Some(self.staging.clone());
        config
    }

    fn published_script(&self) -> Vec<u8> {
        fs::read(self.output.join("scripts/one.luau")).unwrap()
    }

    async fn run(&self, config: PipelineConfig) -> PipelineReport {
        AssetPipeline::run_async(config, progress_channel())
            .await
            .unwrap()
    }
}

fn script(name: &str) -> Vec<u8> {
    dummy_content::pex::minimal(name).unwrap()
}

fn progress_channel() -> mpsc::Sender<converter::ProgressEvent> {
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tx
}

/// Converts the same source into a directory of its own, for comparison.
async fn fresh_conversion(config: &PipelineConfig) -> Vec<u8> {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("fresh");
    let mut fresh = config.clone();
    fresh.output_dir = output.clone();
    fresh.resume_staging = None;
    AssetPipeline::run_async(fresh, progress_channel())
        .await
        .unwrap();
    fs::read(output.join("scripts/one.luau")).unwrap()
}

#[tokio::test]
async fn reconverts_a_staged_output_when_its_source_changed() {
    let fixture = Fixture::new();
    // The record describes a script that has since been replaced.
    fixture.write_source("One");
    fixture.stage_marker();
    let stale = fixture.marker_record();
    fixture.write_record(&stale);
    fixture.write_source("Renamed");

    let report = fixture.run(fixture.resumed_config()).await;
    let expected = fresh_conversion(&fixture.config).await;

    assert_eq!(report.skipped, 0);
    assert!(report.complete);
    assert_eq!(
        fixture.published_script(),
        expected,
        "a staged output whose source changed was certified instead of converted"
    );
}

#[tokio::test]
async fn reconverts_a_staged_output_written_under_an_older_schema() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fixture.stage_marker();
    let stale = StagedOutput {
        schema_version: CONVERTER_SCHEMA_VERSION.saturating_sub(1),
        ..fixture.marker_record()
    };
    fixture.write_record(&stale);

    let report = fixture.run(fixture.resumed_config()).await;
    let expected = fresh_conversion(&fixture.config).await;

    assert_eq!(report.skipped, 0);
    assert_eq!(
        fixture.published_script(),
        expected,
        "a staged output from an older schema was certified instead of converted"
    );
}

#[tokio::test]
async fn reconverts_a_staged_output_written_under_a_different_configuration() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fixture.stage_marker();
    let mut other = fixture.config.clone();
    other.texture_etc1s_quality = fixture.config.texture_etc1s_quality / 2;
    let stale = StagedOutput {
        configuration_hash: configuration_hash(&other).unwrap(),
        ..fixture.marker_record()
    };
    fixture.write_record(&stale);

    let report = fixture.run(fixture.resumed_config()).await;
    let expected = fresh_conversion(&fixture.config).await;

    assert_eq!(report.skipped, 0);
    assert_eq!(
        fixture.published_script(),
        expected,
        "a staged output from another configuration was certified instead of converted"
    );
}

#[tokio::test]
async fn reconverts_a_damaged_staged_output() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fixture.stage_marker();
    let stale = fixture.marker_record();
    fixture.write_record(&stale);

    // The record still describes the whole marker; the file is truncated.
    let target = fixture.staging.join("scripts/one.luau");
    fs::write(&target, &MARKER[..MARKER.len() / 2]).unwrap();

    let report = fixture.run(fixture.resumed_config()).await;
    let expected = fresh_conversion(&fixture.config).await;

    assert_eq!(report.skipped, 0);
    assert_eq!(
        fixture.published_script(),
        expected,
        "a truncated staged output was certified instead of converted"
    );
}

#[tokio::test]
async fn ignores_staged_outputs_when_the_cache_is_invalidated() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fixture.stage_marker();
    let current = fixture.marker_record();
    fixture.write_record(&current);

    let mut invalidated = fixture.resumed_config();
    invalidated.invalidate_cache = true;
    let report = fixture.run(invalidated).await;
    let expected = fresh_conversion(&fixture.config).await;

    assert_eq!(report.skipped, 0);
    assert_eq!(
        fixture.published_script(),
        expected,
        "an invalidated run certified a staged output"
    );
}

#[tokio::test]
async fn reuses_a_staged_output_whose_provenance_is_current() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fixture.stage_marker();
    let current = fixture.marker_record();
    fixture.write_record(&current);

    let report = fixture.run(fixture.resumed_config()).await;

    assert_eq!(report.skipped, 0);
    assert!(report.complete);
    assert_eq!(
        fixture.published_script(),
        MARKER,
        "a staged output matching source, schema, configuration and bytes was converted again"
    );
}

#[tokio::test]
async fn does_not_publish_the_staging_journal() {
    let fixture = Fixture::new();
    fixture.write_source("One");

    fixture.run(fixture.resumed_config()).await;

    assert!(
        !StagingJournal::path_in(&fixture.output).exists(),
        "the staging journal shipped with the published assets"
    );
    assert!(fixture.output.join("conversion-manifest.json").is_file());
}

/// A later resume is judged against the journal this run writes, so read it
/// back from a run whose publish was refused, leaving the staging directory
/// and its journal in place.
#[tokio::test]
async fn records_provenance_for_every_output_it_stages() {
    let fixture = Fixture::new();
    fixture.write_source("One");
    fs::write(fixture.data.join("scripts/two.pex"), script("Two")).unwrap();
    let config = fixture.resumed_config();
    // publish_directory refuses to overwrite a stale backup, so the run stops
    // with a complete staging directory and nothing published.
    fs::write(
        fixture
            .output
            .with_extension(format!("backup-{}", std::process::id())),
        b"stale backup",
    )
    .unwrap();

    let error = AssetPipeline::run_async(config.clone(), progress_channel())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stale backup"), "{error}");

    let records = load_staged_outputs(&fixture.staging).unwrap();
    assert_eq!(records.len(), 2);
    for name in ["one", "two"] {
        let key = format!("scripts/{name}.pex");
        let staged = fixture.staging.join(format!("scripts/{name}.luau"));
        let record = records
            .get(&key)
            .unwrap_or_else(|| panic!("no record for {key}"));
        assert_eq!(record.schema_version, CONVERTER_SCHEMA_VERSION);
        assert_eq!(
            record.configuration_hash,
            configuration_hash(&config).unwrap()
        );
        assert_eq!(
            record.source_hash,
            hash_file(&fixture.staging.join(format!("vfs/scripts/{name}.pex"))).unwrap()
        );
        assert_eq!(record.output_size, fs::metadata(&staged).unwrap().len());
        assert_eq!(record.output_hash, hash_file(&staged).unwrap());
    }
}
