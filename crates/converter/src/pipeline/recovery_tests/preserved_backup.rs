use super::*;

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

#[tokio::test]
async fn preserved_unusable_backup_allows_a_fresh_run_after_output_removal() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("modern");
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(
        data.join("scripts/good.pex"),
        dummy_content::pex::minimal("Good").unwrap(),
    )
    .unwrap();
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("conversion-manifest.json"), b"{\"torn\":").unwrap();
    fs::write(output.join("retained-original"), b"original bytes\0\xff").unwrap();

    run(config(&data, &output)).await.unwrap();
    let preserved = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains("-unusable-")
        })
        .expect("unusable original was preserved");
    let manifest_bytes = fs::read(preserved.join("conversion-manifest.json")).unwrap();
    let original_bytes = fs::read(preserved.join("retained-original")).unwrap();
    assert_eq!(manifest_bytes, b"{\"torn\":");
    assert_eq!(original_bytes, b"original bytes\0\xff");

    fs::remove_dir_all(&output).unwrap();
    let second = run(config(&data, &output)).await.unwrap();
    assert!(second.complete, "{:?}", second.warnings);
    assert!(output.join("scripts/good.luau").is_file());
    assert_eq!(
        fs::read(preserved.join("conversion-manifest.json")).unwrap(),
        manifest_bytes
    );
    assert_eq!(
        fs::read(preserved.join("retained-original")).unwrap(),
        original_bytes
    );
}

#[test]
fn ordinary_unowned_backup_with_unusable_substring_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let prefix = publication_backup_prefix(&output).unwrap();
    let unowned = directory
        .path()
        .join(format!("{prefix}unrelated-unusable-name"));
    fs::create_dir(&unowned).unwrap();

    let error = recover_interrupted_publication(&output).unwrap_err();
    assert!(
        format!("{error:#}").contains("unowned publication backup"),
        "{error:#}"
    );
    assert!(unowned.is_dir());
}

#[test]
fn preserved_unusable_backup_with_generated_retry_suffix_is_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let prefix = publication_backup_prefix(&output).unwrap();
    let preserved = directory.path().join(format!("{prefix}123-unusable-456-1"));
    fs::create_dir(&preserved).unwrap();

    recover_interrupted_publication(&output).unwrap();
    assert!(preserved.is_dir());
}

#[test]
fn noncanonical_unusable_backup_retry_suffix_is_refused() {
    for retry in ["+1", "01"] {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern");
        let prefix = publication_backup_prefix(&output).unwrap();
        let unowned = directory
            .path()
            .join(format!("{prefix}123-unusable-456-{retry}"));
        fs::create_dir(&unowned).unwrap();

        assert!(recover_interrupted_publication(&output).is_err(), "{retry}");
        assert!(unowned.is_dir());
    }
}

#[cfg(unix)]
#[test]
fn symlink_named_like_a_preserved_backup_is_still_refused() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("modern");
    let prefix = publication_backup_prefix(&output).unwrap();
    let link = directory.path().join(format!("{prefix}123-unusable-456"));
    symlink(directory.path().join("missing-target"), &link).unwrap();

    let error = recover_interrupted_publication(&output).unwrap_err();
    assert!(
        format!("{error:#}").contains("unowned publication backup"),
        "{error:#}"
    );
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}
