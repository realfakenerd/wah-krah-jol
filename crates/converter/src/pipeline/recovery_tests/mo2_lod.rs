use super::*;

#[test]
fn invalid_mo2_texture_is_recorded_and_skipped_before_lod_staging() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = directory.path().join("vfs");
    let bad = directory.path().join("bad.dds");
    let good = directory.path().join("good.dds");
    let lod = directory.path().join("tamriel.lod");
    fs::write(&bad, b"bad").unwrap();
    fs::write(&good, b"good").unwrap();
    fs::write(&lod, b"lod").unwrap();
    let sources = BTreeMap::from([
        ("textures/bad%.dds".to_string(), bad),
        ("textures/good.dds".to_string(), good),
        ("lodsettings/tamriel.lod".to_string(), lod),
    ]);
    let mut report = PipelineReport::default();
    let mut manifest = ConversionManifest::default();
    let mut skipped = BTreeSet::new();

    assert!(
        canonicalize_loose_mo2_asset(
            "textures/bad%.dds",
            AssetKind::Texture,
            "dds",
            false,
            &mut report,
            &mut manifest,
            &mut skipped,
        )
        .unwrap()
        .is_none()
    );
    stage_mo2_lod_inputs(&vfs, &sources, &skipped).unwrap();

    assert_eq!(report.skipped, 1);
    assert!(manifest.failures.contains_key("textures/bad%.dds"));
    assert!(!vfs.join("textures/bad%.dds").exists());
    assert_eq!(fs::read(vfs.join("textures/good.dds")).unwrap(), b"good");
    assert_eq!(
        fs::read(vfs.join("lodsettings/tamriel.lod")).unwrap(),
        b"lod"
    );
}

#[test]
fn invalid_mo2_texture_remains_fail_fast() {
    let mut report = PipelineReport::default();
    let mut manifest = ConversionManifest::default();
    let mut skipped = BTreeSet::new();
    assert!(
        canonicalize_loose_mo2_asset(
            "textures/bad%.dds",
            AssetKind::Texture,
            "dds",
            true,
            &mut report,
            &mut manifest,
            &mut skipped,
        )
        .is_err()
    );
    assert_eq!(report.skipped, 0);
    assert!(manifest.failures.is_empty());
    assert!(skipped.is_empty());
}
