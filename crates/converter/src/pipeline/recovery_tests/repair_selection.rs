use super::*;
use std::collections::BTreeMap;

fn field(signature: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [
        signature.as_slice(),
        &(payload.len() as u16).to_le_bytes(),
        payload,
    ]
    .concat()
}

fn record(signature: &[u8; 4], id: u32, flags: u32, payload: &[u8]) -> Vec<u8> {
    [
        signature.as_slice(),
        &(payload.len() as u32).to_le_bytes(),
        &flags.to_le_bytes(),
        &id.to_le_bytes(),
        &0x7351_294Bu32.to_le_bytes(),
        &44u16.to_le_bytes(),
        &0xA913u16.to_le_bytes(),
        payload,
    ]
    .concat()
}

fn repair_plugin_bytes() -> Vec<u8> {
    let header = field(
        b"HEDR",
        &[
            1.7f32.to_le_bytes(),
            1u32.to_le_bytes(),
            0x900u32.to_le_bytes(),
        ]
        .concat(),
    );
    let mut bytes = record(b"TES4", 0, 1, &header);
    bytes.extend(record(
        b"KYWD",
        0x801,
        0,
        &field(b"EDID", b"RepairSelectionKeyword\0"),
    ));
    bytes
}

fn assert_repairable_pack(output: &Path) {
    let check =
        crate::check::check_output(output, crate::check::CheckMode::Quick, |_, _| {}).unwrap();
    assert!(
        check.problems.iter().all(|problem| matches!(
            problem,
            crate::check::CheckProblem::Incomplete
                | crate::check::CheckProblem::RecordedFailures { .. }
        )),
        "repair fixture has unrelated pack problems: {:?}",
        check.problems
    );
    assert!(
        check
            .problems
            .iter()
            .any(|problem| matches!(problem, crate::check::CheckProblem::Incomplete))
    );
    assert!(
        check
            .problems
            .iter()
            .any(|problem| matches!(problem, crate::check::CheckProblem::RecordedFailures { .. }))
    );
}

fn data_with_script(root: &Path) -> PathBuf {
    let data = root.join("Data");
    fs::create_dir_all(data.join("Scripts")).unwrap();
    fs::write(data.join("repair.esm"), repair_plugin_bytes()).unwrap();
    fs::write(
        data.join("Scripts/good.pex"),
        dummy_content::pex::minimal("Good").unwrap(),
    )
    .unwrap();
    data
}

fn write_script_archive(data: &Path, archive_name: &str, member: &str, name: &str) {
    let script = dummy_content::pex::minimal(name).unwrap();
    let bytes = dummy_content::bsa::v104(
        &[dummy_content::Entry::new(member, &script)],
        dummy_content::bsa::Compression::None,
    )
    .unwrap();
    fs::write(data.join(archive_name), bytes).unwrap();
}

fn add_failure(output: &Path, key: String) {
    let path = output.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&path).unwrap();
    manifest
        .failures
        .insert(key, "simulated discovery failure".into());
    manifest.complete = false;
    manifest.save(&path).unwrap();
    assert_repairable_pack(output);
}

fn published_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap();
        if relative
            .components()
            .next()
            .is_some_and(|part| part.as_os_str().to_string_lossy().starts_with(".repair-"))
        {
            continue;
        }
        files.insert(
            relative.to_string_lossy().replace('\\', "/"),
            fs::read(entry.path()).unwrap(),
        );
    }
    files
}

#[tokio::test]
async fn repair_rescans_and_converts_a_blocked_loose_asset_subtree() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    write_script_archive(&data, "loose.bsa", "scripts/blocked/archive.pex", "Archive");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();
    assert!(output.join("scripts/blocked/archive.luau").is_file());

    let blocked = data.join("Authoring/Scripts/Blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(
        blocked.join("new.pex"),
        dummy_content::pex::minimal("New").unwrap(),
    )
    .unwrap();
    let mut manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(
        manifest
            .entries
            .remove("scripts/blocked/archive.pex")
            .is_some()
    );
    fs::remove_file(output.join("scripts/blocked/archive.luau")).unwrap();
    manifest.failures.insert(
        "authoring/scripts/blocked".into(),
        "simulated discovery failure".into(),
    );
    manifest.complete = false;
    manifest
        .save(&output.join("conversion-manifest.json"))
        .unwrap();
    assert_repairable_pack(&output);

    let report = crate::repair::repair_failed(&settings, true).unwrap();
    assert!(report.published);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(output.join("scripts/blocked/new.luau").is_file());
    assert!(output.join("scripts/blocked/archive.luau").is_file());
    assert!(
        ConversionManifest::load(&output.join("conversion-manifest.json"))
            .unwrap()
            .failures
            .is_empty()
    );
}

#[tokio::test]
async fn failed_child_keeps_directory_failure_and_original_pack_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    let blocked = data.join("Scripts/Blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(blocked.join("bad.pex"), b"not a PEX file").unwrap();
    add_failure(&output, "scripts/blocked".into());
    let before = published_files(&output);

    let preview = crate::repair::repair_failed(&settings, false).unwrap();
    assert_eq!(
        preview.failures.get("scripts/blocked").map(String::as_str),
        Some("simulated discovery failure")
    );
    let preview_manifest =
        ConversionManifest::load(&preview.directory.join("assets/conversion-manifest.json"))
            .unwrap();
    assert!(preview_manifest.failures.contains_key("scripts/blocked"));
    let error = crate::repair::repair_failed(&settings, true).unwrap_err();
    assert!(format!("{error:#}").contains("repair still has"));
    assert_eq!(published_files(&output), before);
}

#[tokio::test]
async fn relative_failure_key_finds_nested_non_mo2_loose_source() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    write_script_archive(&data, "loose.bsa", "scripts/nested.pex", "Archive");
    let output = directory.path().join("modern");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    let loose = data.join("Authoring/Scripts/nested.pex");
    let loose_bytes = dummy_content::pex::minimal("NestedLoose").unwrap();
    fs::create_dir_all(loose.parent().unwrap()).unwrap();
    fs::write(&loose, &loose_bytes).unwrap();
    let manifest_path = output.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    manifest.failures.insert(
        "scripts/nested.pex".into(),
        "skipped loose asset was unreadable".into(),
    );
    manifest.complete = false;
    manifest.save(&manifest_path).unwrap();
    assert_repairable_pack(&output);

    let report = crate::repair::repair_failed(&settings, true).unwrap();
    assert!(report.published);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let repaired = ConversionManifest::load(&manifest_path).unwrap();
    assert_eq!(
        repaired.entries["scripts/nested.pex"].source_hash,
        crate::cache::hash_file(&loose).unwrap()
    );
}

#[tokio::test]
async fn legacy_absolute_asset_failure_repairs_and_clears_its_alias() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    let source = data.join("Scripts/legacy.pex");
    fs::write(&source, dummy_content::pex::minimal("Legacy").unwrap()).unwrap();
    add_failure(&output, source.to_string_lossy().into_owned());

    let report = crate::repair::repair_failed(&settings, true).unwrap();
    assert!(report.published);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(output.join("scripts/legacy.luau").is_file());
    assert!(
        ConversionManifest::load(&output.join("conversion-manifest.json"))
            .unwrap()
            .failures
            .is_empty()
    );
}

#[tokio::test]
async fn metadata_subtree_remains_failed_and_cannot_publish_clean() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    let blocked = data.join("Plugins/Blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(blocked.join("new.esp"), b"plugin bytes").unwrap();
    add_failure(&output, "plugins/blocked".into());
    let before = published_files(&output);

    let preview = crate::repair::repair_failed(&settings, false).unwrap();
    assert!(preview.failures.contains_key("plugins/blocked"));
    assert!(preview.failures["plugins/blocked"].contains("normal conversion"));
    let error = crate::repair::repair_failed(&settings, true).unwrap_err();
    assert!(format!("{error:#}").contains("repair still has"));
    assert_eq!(published_files(&output), before);
}

#[tokio::test]
async fn missing_failed_subtree_remains_failed_and_cannot_publish_clean() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    add_failure(&output, "scripts/missing".into());
    let before = published_files(&output);
    let preview = crate::repair::repair_failed(&settings, false).unwrap();
    assert!(preview.failures.contains_key("scripts/missing"));
    let error = crate::repair::repair_failed(&settings, true).unwrap_err();
    assert!(format!("{error:#}").contains("repair still has"));
    assert_eq!(published_files(&output), before);
}

#[tokio::test]
async fn legacy_absolute_missing_loose_file_does_not_fall_back_to_archive() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    write_script_archive(&data, "loose.bsa", "scripts/legacy.pex", "Archived");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();
    assert!(output.join("scripts/legacy.luau").is_file());

    let mut manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(manifest.entries.remove("scripts/legacy.pex").is_some());
    fs::remove_file(output.join("scripts/legacy.luau")).unwrap();
    let missing_loose = data.join("Scripts/legacy.pex");
    assert!(!missing_loose.exists());
    manifest.failures.insert(
        missing_loose.to_string_lossy().into_owned(),
        "legacy loose source was unreadable".into(),
    );
    manifest.complete = false;
    manifest
        .save(&output.join("conversion-manifest.json"))
        .unwrap();
    assert_repairable_pack(&output);
    let before = published_files(&output);

    let error = crate::repair::repair_failed(&settings, true).unwrap_err();
    assert!(format!("{error:#}").contains("repair still has"));
    assert_eq!(published_files(&output), before);
    let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(
        manifest
            .failures
            .contains_key(&missing_loose.to_string_lossy().into_owned())
    );
}

#[tokio::test]
async fn malformed_percent_loose_asset_stays_failed_while_valid_neighbor_repairs() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    let malformed_loose = data.join("Scripts/bad%.pex");
    fs::write(
        &malformed_loose,
        dummy_content::pex::minimal("LooseBad").unwrap(),
    )
    .unwrap();
    // The archive also has a valid source with the same malformed path. Repair must not
    // turn the malformed loose failure into a successful archive conversion.
    let malformed_archive = dummy_content::pex::minimal("ArchivedBad").unwrap();
    let neighbor_archive = dummy_content::pex::minimal("ArchivedNeighbor").unwrap();
    let archive_bytes = dummy_content::bsa::v104(
        &[
            dummy_content::Entry::new("scripts/bad%.pex", &malformed_archive),
            dummy_content::Entry::new("scripts/neighbor.pex", &neighbor_archive),
        ],
        dummy_content::bsa::Compression::None,
    )
    .unwrap();
    fs::write(data.join("malformed.bsa"), archive_bytes).unwrap();
    fs::write(
        data.join("Scripts/neighbor.pex"),
        dummy_content::pex::minimal("LooseNeighbor").unwrap(),
    )
    .unwrap();

    let settings = config(&data, &output);
    let initial = run(settings.clone()).await.unwrap();
    assert!(!initial.complete);
    let manifest_path = output.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    assert!(manifest.failures.contains_key("scripts/bad%.pex"));
    assert!(manifest.entries.remove("scripts/neighbor.pex").is_some());
    fs::remove_file(output.join("scripts/neighbor.luau")).unwrap();
    manifest.failures.insert(
        "scripts/neighbor.pex".into(),
        "simulated neighbor failure".into(),
    );
    manifest.complete = false;
    manifest.save(&manifest_path).unwrap();
    assert_repairable_pack(&output);

    let preview = crate::repair::repair_failed(&settings, false).unwrap();
    assert!(preview.failures.contains_key("scripts/bad%.pex"));
    assert!(
        !preview.failures.contains_key("scripts/neighbor.pex"),
        "the valid neighbor must be attempted and repaired: {:?}",
        preview.failures
    );
    let preview_assets = preview.directory.join("assets");
    assert!(preview_assets.join("scripts/neighbor.luau").is_file());
    assert!(
        !preview_assets.join("scripts/bad%.luau").exists(),
        "malformed loose source must not fall back to its archive member"
    );
}

#[tokio::test]
async fn archive_only_canonical_failure_repairs_without_a_loose_file() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    write_script_archive(&data, "loose.bsa", "scripts/archived.pex", "Archived");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();
    assert!(output.join("scripts/archived.luau").is_file());

    let mut manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(manifest.entries.remove("scripts/archived.pex").is_some());
    fs::remove_file(output.join("scripts/archived.luau")).unwrap();
    manifest.failures.insert(
        "scripts/archived.pex".into(),
        "scripts/archived.pex: simulated archive conversion failure".into(),
    );
    manifest.complete = false;
    manifest
        .save(&output.join("conversion-manifest.json"))
        .unwrap();
    assert_repairable_pack(&output);
    assert!(!data.join("Scripts/archived.pex").exists());

    let report = crate::repair::repair_failed(&settings, true).unwrap();
    assert!(report.published);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(output.join("scripts/archived.luau").is_file());
}

#[test]
fn producer_failure_keys_use_the_data_relative_canonical_source_space() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let file = data.join("Authoring/Scripts/Blocked/New.PEX");
    assert_eq!(input_failure_key(&data, &file), "scripts/blocked/new.pex");
    assert_eq!(
        input_failure_key(&data, &data.join("Authoring/Scripts/Blocked")),
        "authoring/scripts/blocked"
    );
}

#[tokio::test]
async fn duplicate_canonical_members_in_repaired_archive_cannot_publish() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let output = directory.path().join("modern");
    write_script_archive(&data, "duplicate.bsa", "scripts/initial.pex", "Initial");
    let settings = config(&data, &output);
    run(settings.clone()).await.unwrap();

    let duplicate = dummy_content::pex::minimal("Duplicate").unwrap();
    let replacement = dummy_content::bsa::v104(
        &[
            dummy_content::Entry::new("scripts/dup.pex", &duplicate),
            dummy_content::Entry::new("Data/Scripts/dup.pex", &duplicate),
        ],
        dummy_content::bsa::Compression::None,
    )
    .unwrap();
    fs::write(data.join("duplicate.bsa"), replacement).unwrap();
    let archive_path = data.join("duplicate.bsa");
    let archive_failure = archive_path.to_string_lossy().into_owned();
    let mut manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    manifest
        .failures
        .insert(archive_failure.clone(), "simulated archive failure".into());
    manifest.complete = false;
    manifest
        .save(&output.join("conversion-manifest.json"))
        .unwrap();
    assert_repairable_pack(&output);
    let before = published_files(&output);

    let error = crate::repair::repair_failed(&settings, true).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "archive contains normalized path collision for scripts/dup.pex: scripts/dup.pex and Data/Scripts/dup.pex"
        ),
        "unexpected repair error: {message}; archive entries: scripts/dup.pex, Data/Scripts/dup.pex"
    );
    assert_eq!(published_files(&output), before);
    assert!(
        ConversionManifest::load(&output.join("conversion-manifest.json"))
            .unwrap()
            .failures
            .contains_key(&archive_failure)
    );
}

#[tokio::test]
async fn mo2_loose_failures_repair_only_current_winners_and_keep_valid_neighbors() {
    let directory = tempfile::tempdir().unwrap();
    let data = data_with_script(directory.path());
    let instance = directory.path().join("MO2");
    let mod_root = instance.join("mods/Repair");
    let profile = instance.join("profiles/Default");
    fs::create_dir_all(mod_root.join("scripts")).unwrap();
    fs::create_dir_all(instance.join("overwrite")).unwrap();
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        instance.join("ModOrganizer.ini"),
        "[General]\ngameName=Skyrim Special Edition\n[Settings]\nmod_directory=%BASE_DIR%/mods\nprofiles_directory=%BASE_DIR%/profiles\noverwrite_directory=%BASE_DIR%/overwrite\n",
    )
    .unwrap();
    fs::write(profile.join("modlist.txt"), "+Repair\n").unwrap();
    fs::write(profile.join("plugins.txt"), "*repair.esm\n").unwrap();
    fs::write(profile.join("loadorder.txt"), "repair.esm\n").unwrap();

    let repaired_loose = mod_root.join("scripts/repaired.pex");
    let removed_loose = mod_root.join("scripts/removed.pex");
    fs::write(&repaired_loose, b"broken loose PEX").unwrap();
    fs::write(&removed_loose, b"broken loose PEX").unwrap();
    // Include both archive members so the removed loose winner still has archive bytes.
    let archive_bytes = dummy_content::bsa::v104(
        &[
            dummy_content::Entry::new(
                "scripts/repaired.pex",
                &dummy_content::pex::minimal("ArchiveRepaired").unwrap(),
            ),
            dummy_content::Entry::new(
                "scripts/removed.pex",
                &dummy_content::pex::minimal("ArchiveRemoved").unwrap(),
            ),
        ],
        dummy_content::bsa::Compression::None,
    )
    .unwrap();
    fs::write(mod_root.join("repair.bsa"), archive_bytes).unwrap();

    let output = directory.path().join("modern");
    let mut settings = config(&data, &output);
    settings.mo2 = Some(mo2::Selection {
        instance_path: instance,
        profile: "Default".into(),
    });
    let initial = run(settings.clone()).await.unwrap();
    assert!(!initial.complete);
    let manifest_path = output.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    assert!(manifest.failures.contains_key("scripts/repaired.pex"));
    assert!(manifest.failures.contains_key("scripts/removed.pex"));
    manifest.failures.insert(
        "textures/bad%.dds".into(),
        "simulated invalid failure key".into(),
    );
    manifest.complete = false;
    manifest.save(&manifest_path).unwrap();

    let loose_bytes = dummy_content::pex::minimal("LooseRepaired").unwrap();
    fs::write(&repaired_loose, &loose_bytes).unwrap();
    fs::remove_file(&removed_loose).unwrap();

    let preview = crate::repair::repair_failed(&settings, false).unwrap();
    assert!(
        preview.failures.contains_key("scripts/removed.pex"),
        "missing MO2 loose winner must remain failed: {:?}",
        preview.failures
    );
    assert!(preview.failures.contains_key("textures/bad%.dds"));
    let preview_assets = preview.directory.join("assets");
    assert!(preview_assets.join("scripts/repaired.luau").is_file());
    assert!(
        !preview_assets.join("scripts/removed.luau").exists(),
        "repair must not use the archive member after its MO2 loose winner disappears"
    );
    let preview_manifest =
        ConversionManifest::load(&preview_assets.join("conversion-manifest.json")).unwrap();
    assert_eq!(
        preview_manifest.entries["scripts/repaired.pex"].source_hash,
        crate::cache::hash_file(&repaired_loose).unwrap()
    );
}
