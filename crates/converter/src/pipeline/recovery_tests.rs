use super::*;

#[path = "recovery_tests/repair_selection.rs"]
mod repair_selection;

#[path = "recovery_tests/mo2_lod.rs"]
mod mo2_lod;
#[path = "recovery_tests/pathless.rs"]
mod pathless;
#[path = "recovery_tests/preserved_backup.rs"]
mod preserved_backup;

fn config(data: &Path, output: &Path) -> PipelineConfig {
    let mut config = PipelineConfig::new(data, output);
    config.cpu_jobs = 2;
    config.no_lod = true;
    config
}

async fn run(config: PipelineConfig) -> Result<PipelineReport, PipelineFailure> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let progress = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = AssetPipeline::run_async(config, tx).await;
    progress.await.unwrap();
    result
}

fn pack(root: &Path) {
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(root.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
    ConversionManifest {
        schema_version: CONVERTER_SCHEMA_VERSION,
        complete: true,
        ..Default::default()
    }
    .save(&root.join("conversion-manifest.json"))
    .unwrap();
}

#[tokio::test]
async fn unusable_manifest_rebuilds_and_preserves_the_original_pack() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("modern");
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(
        data.join("scripts/good.pex"),
        dummy_content::pex::minimal("Good").unwrap(),
    )
    .unwrap();
    pack(&output);
    let damaged = b"{\"torn\":";
    fs::write(output.join("conversion-manifest.json"), damaged).unwrap();
    fs::write(output.join("retained-original"), b"original bytes").unwrap();

    let report = run(config(&data, &output)).await.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    assert!(output.join("scripts/good.luau").is_file());
    assert!(!output.join("retained-original").exists());
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains("rebuilding without"))
    );
    let backups: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("modern.backup-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read(backups[0].join("conversion-manifest.json")).unwrap(),
        damaged
    );
    assert_eq!(
        fs::read(backups[0].join("retained-original")).unwrap(),
        b"original bytes"
    );
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains(&backups[0].display().to_string()))
    );
    assert!(run(config(&data, &output)).await.unwrap().complete);
    assert!(
        backups[0].is_dir(),
        "a later run removed the preserved original"
    );
}

#[tokio::test]
async fn failed_rebuild_leaves_the_damaged_previous_pack_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("modern");
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(data.join("scripts/bad.pex"), b"broken script").unwrap();
    pack(&output);
    fs::write(output.join("conversion-manifest.json"), b"torn manifest").unwrap();
    let mut settings = config(&data, &output);
    settings.fail_fast = true;
    assert!(run(settings).await.is_err());
    assert_eq!(
        fs::read(output.join("conversion-manifest.json")).unwrap(),
        b"torn manifest"
    );
    assert_eq!(
        fs::read(output.join("scripts/papyrus_runtime.luau")).unwrap(),
        b"return {}"
    );
}

#[test]
fn cleanup_retries_remove_ordinary_leftovers_but_keep_unusable_originals() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let staging = directory.path().join("staging");
    pack(&output);
    pack(&staging);
    FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(true));
    let published = publish_directory(&staging, &output);
    FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(false));
    published.unwrap();
    let leftovers = read_publication_leftovers(&output).unwrap();
    assert_eq!(leftovers.len(), 1);
    let ordinary_backup = directory.path().join(&leftovers[0]);

    fs::write(output.join("conversion-manifest.json"), b"torn manifest").unwrap();
    fs::write(output.join("original-only"), b"preserved bytes").unwrap();
    pack(&staging);
    FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(true));
    let published = publish_directory(&staging, &output);
    FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(false));
    published.unwrap();
    assert_eq!(read_publication_leftovers(&output).unwrap(), leftovers);
    let preserved = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir() && path.join("original-only").is_file())
        .unwrap();

    recover_interrupted_publication(&output).unwrap();
    assert!(!ordinary_backup.exists());
    assert!(read_publication_leftovers(&output).unwrap().is_empty());
    assert_eq!(
        fs::read(preserved.join("original-only")).unwrap(),
        b"preserved bytes"
    );
    assert_eq!(
        fs::read(preserved.join("conversion-manifest.json")).unwrap(),
        b"torn manifest"
    );
    validate_publication_package(&output).unwrap();
}

#[test]
fn unusable_manifest_recovery_preserves_exact_bytes_at_each_publication_boundary() {
    for boundary in 0..3 {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern");
        let backup = directory.path().join("modern.backup-fixture");
        let staging = directory.path().join("staging");
        pack(&output);
        pack(&staging);
        fs::write(output.join("conversion-manifest.json"), b"torn manifest").unwrap();
        fs::write(output.join("old-only"), b"retained bytes").unwrap();
        write_publication_record(&output, &backup, &staging).unwrap();
        if boundary >= 1 {
            fs::rename(&output, &backup).unwrap();
        }
        if boundary >= 2 {
            fs::rename(&staging, &output).unwrap();
        }

        recover_interrupted_publication(&output).unwrap();
        let original = if boundary == 2 { &backup } else { &output };
        assert_eq!(
            fs::read(original.join("conversion-manifest.json")).unwrap(),
            b"torn manifest"
        );
        assert_eq!(
            fs::read(original.join("old-only")).unwrap(),
            b"retained bytes"
        );
        assert!(!publication_record_path(&output).unwrap().exists());
        if boundary == 2 {
            validate_publication_package(&output).unwrap();
        }
    }
}

#[test]
fn unusable_manifest_recovery_refuses_a_changed_backup() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let backup = directory.path().join("modern.backup-fixture");
    let staging = directory.path().join("staging");
    pack(&output);
    pack(&staging);
    fs::write(output.join("conversion-manifest.json"), b"torn manifest").unwrap();
    write_publication_record(&output, &backup, &staging).unwrap();
    fs::rename(&output, &backup).unwrap();
    fs::write(backup.join("scripts/papyrus_runtime.luau"), b"changed").unwrap();
    assert!(recover_interrupted_publication(&output).is_err());
    assert!(!output.exists());
    assert!(backup.is_dir());
}

#[test]
fn unusable_manifest_recovery_refuses_unsealed_paths() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let backup = directory.path().join("modern.backup-fixture");
    let staging = directory.path().join("staging");
    pack(&output);
    pack(&staging);
    fs::write(output.join("conversion-manifest.json"), b"torn manifest").unwrap();
    write_publication_record(&output, &backup, &staging).unwrap();
    fs::rename(&output, &backup).unwrap();
    fs::create_dir_all(backup.join("vfs")).unwrap();
    fs::write(backup.join("vfs/unsealed"), b"new bytes").unwrap();
    assert!(recover_interrupted_publication(&output).is_err());
    assert!(!output.exists());
    assert!(backup.is_dir());
}

#[tokio::test]
async fn invalid_loose_filename_does_not_stop_valid_neighbors() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("modern");
    fs::create_dir_all(data.join("textures")).unwrap();
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(data.join("textures/bad%.dds"), b"unusable filename").unwrap();
    fs::write(
        data.join("scripts/good.pex"),
        dummy_content::pex::minimal("Good").unwrap(),
    )
    .unwrap();
    let report = run(config(&data, &output)).await.unwrap();
    assert!(!report.complete);
    assert_eq!(report.skipped, 1);
    assert!(output.join("scripts/good.luau").is_file());
    assert!(
        ConversionManifest::load(&output.join("conversion-manifest.json"))
            .unwrap()
            .failures
            .keys()
            .any(|key| key.contains("bad%.dds"))
    );
}

#[test]
fn failed_loose_override_is_not_replaced_by_archive_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let vfs = directory.path().join("vfs");
    fs::create_dir_all(data.join("textures/raced.dds")).unwrap();
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(data.join("scripts/good.pex"), b"neighbor").unwrap();
    fs::create_dir_all(vfs.join("textures")).unwrap();
    let blob = directory.path().join("archive-blob");
    fs::write(&blob, b"archive bytes").unwrap();
    link_or_copy(&blob, &vfs.join("textures/raced.dds")).unwrap();
    let mut report = PipelineReport::default();
    let mut manifest = ConversionManifest::default();
    overlay_loose_assets_with_recovery(
        &data,
        &vfs,
        &[
            data.join("textures/raced.dds"),
            data.join("scripts/good.pex"),
        ],
        |source, key, error| {
            record_input_failure(&mut report, &mut manifest, key.unwrap_or(source), error);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(report.skipped, 1);
    assert!(manifest.failures.contains_key("textures/raced.dds"));
    assert!(!vfs.join("textures/raced.dds").exists());
    assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
    assert_eq!(fs::read(vfs.join("scripts/good.pex")).unwrap(), b"neighbor");
}

#[test]
fn undiscovered_subtrees_mask_archive_versions_without_losing_neighbors() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let vfs = directory.path().join("vfs");
    let blocked = data.join("Textures/Blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::create_dir_all(vfs.join("textures/blocked")).unwrap();
    fs::write(vfs.join("textures/blocked/old.dds"), b"archive").unwrap();
    fs::write(vfs.join("textures/neighbor.dds"), b"neighbor").unwrap();
    mask_undiscovered_inputs(&data, &vfs, &[blocked]).unwrap();
    assert!(!vfs.join("textures/blocked/old.dds").exists());
    assert_eq!(
        fs::read(vfs.join("textures/neighbor.dds")).unwrap(),
        b"neighbor"
    );
}
