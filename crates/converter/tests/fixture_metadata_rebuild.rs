mod common;

use converter::{
    AssetPipeline, PipelineConfig,
    cache::{ConversionManifest, hash_bytes, hash_file},
};
use dummy_content::{Entry, bsa, esm, layout, nif};
use rusqlite::Connection;
use std::{fs, path::Path};

const PACKED_SETTINGS: [u8; 16] = [0xfc, 0xff, 0xfc, 0xff, 32, 0, 0, 0, 4, 0, 0, 0, 32, 0, 0, 0];
const VOLCANIC_MODEL: &str = "meshes/landscape/grass/dlc02volcanicashgrass01.nif";
const VOLCANIC_GLB: &str = "meshes/landscape/grass/dlc02volcanicashgrass01.glb";
const VOLCANIC_ORIGINAL: &str = "textures/landscape/grass/volcanicashgrass02_n.dds";
const VOLCANIC_TARGET: &str = "textures/dlc02/landscape/grass/volcanicashgrass02_n.dds";

/// A capability dropped during final refresh must affect both published completeness flags.
#[tokio::test]
async fn metadata_rebuild_completeness_includes_late_extension_warnings() {
    use shared::extensions::{ExtensionEntry, ExtensionHasher, ExtensionOwns};
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("rebuilt");
    generate(&data);
    convert(&data, &source).await;
    let foreign_path = "optional-payload.bin";
    let payload = b"source-owned optional payload";
    fs::write(source.join(foreign_path), payload).unwrap();
    let connection = Connection::open(source.join("skyrim_world.db")).unwrap();
    let core: String = connection
        .query_row(
            "SELECT build_identity FROM lod_build WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let mut hasher = ExtensionHasher::new("example.optional", 1);
    hasher.add_file(foreign_path, payload);
    let manifest_path = source.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    manifest.extensions.push(ExtensionEntry {
        id: "example.optional".into(),
        version: 1,
        owns: ExtensionOwns {
            files: vec![foreign_path.into()],
            ..Default::default()
        },
        sha256: hasher.finish(),
        built_from: core,
        stats: Default::default(),
    });
    manifest.save(&manifest_path).unwrap();
    let source_hash = hash_file(&manifest_path).unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("example.optional")),
        "late refresh warning was not exercised: {:?}",
        report.warnings
    );
    assert!(
        !report.complete,
        "late extension warning must mark the report incomplete"
    );
    let published = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(
        !published.complete,
        "manifest and report must agree after final refresh"
    );
    assert!(
        !published
            .extensions
            .iter()
            .any(|entry| entry.id == "example.optional")
    );
    assert_eq!(
        hash_file(&manifest_path).unwrap(),
        source_hash,
        "rebuild must not modify the source manifest"
    );
    assert_eq!(fs::read(source.join(foreign_path)).unwrap(), payload);
}

/// Old complete packs can contain optional derived files and strays without output proofs.
/// Omit only those files while publishing every verified neighboring asset unchanged.
#[tokio::test]
async fn metadata_rebuild_skips_unproven_derived_outputs_and_strays_locally() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("rebuilt");
    generate(&data);
    convert(&data, &source).await;
    let manifest = ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
    let manifest_hash = hash_file(&source.join("conversion-manifest.json")).unwrap();
    let before: Vec<_> = manifest
        .entries
        .values()
        .map(|entry| {
            (
                entry.output.clone(),
                hash_file(&source.join(&entry.output)).unwrap(),
            )
        })
        .collect();
    let unproven = [
        "meshes/actors/character/composed/default/male/body.glb",
        "meshes/actors/character/composed/default/female/body.glb",
        "meshes/actors/character/animations/male/mt_idle.glb",
        "meshes/actors/character/animations/female/mt_idle.glb",
        "scripts/mod-stray.luau",
        "textures/mod-stray.bin",
    ];
    for relative in unproven {
        let path = source.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"unproven bytes must never be retained").unwrap();
    }
    let result = rebuild(&data, &source, &output).await;
    assert!(
        result.is_ok(),
        "local unproven files aborted publication: {result:?}"
    );
    let report = result.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    assert!(report.warnings.is_empty());
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains("6 unproven retained assets")),
        "{:?}",
        report.notices
    );
    for relative in unproven {
        assert!(
            !output.join(relative).exists(),
            "unproven output published: {relative}"
        );
        assert_eq!(
            fs::read(source.join(relative)).unwrap(),
            b"unproven bytes must never be retained"
        );
    }
    for (relative, hash) in before {
        assert_eq!(
            hash_file(&output.join(&relative)).unwrap(),
            hash,
            "verified neighbor changed: {relative}"
        );
        assert_eq!(hash_file(&source.join(&relative)).unwrap(), hash);
    }
    assert_eq!(
        hash_file(&source.join("conversion-manifest.json")).unwrap(),
        manifest_hash
    );
    let connection = Connection::open(output.join("skyrim_world.db")).unwrap();
    let count: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM \"references\" WHERE id=4660",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "valid neighboring plugin record must publish");
}

/// Supply independent synthetic producer facts for the consumer's body-recipe fixture.
fn derived_body_proof(bytes: &[u8]) -> converter::cache::DerivedOutput {
    converter::cache::DerivedOutput {
        recipe: "body-compose-v1".into(),
        inputs: [
            (
                "meshes/actors/character/character assets/malebody_1.nif",
                "body",
            ),
            (
                "meshes/actors/character/character assets/malehands_1.nif",
                "hands",
            ),
            (
                "meshes/actors/character/character assets/malefeet_1.nif",
                "feet",
            ),
            (
                "meshes/actors/character/character assets/skeleton.nif",
                "skeleton",
            ),
        ]
        .into_iter()
        .map(|(path, bytes)| (path.into(), converter::cache::hash_bytes(bytes.as_bytes())))
        .collect(),
        output_size: bytes.len() as u64,
        output_hash: converter::cache::hash_bytes(bytes),
    }
}

/// Hold a Windows source file open without sharing reads until the guard is dropped.
#[cfg(windows)]
fn block_file_reads(path: &Path) -> Option<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    Some(
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap(),
    )
}

/// Restore a Unix fixture's permissions even if the rebuild or an assertion panics.
#[cfg(unix)]
struct RestoreFilePermissions<'a> {
    path: &'a Path,
    permissions: fs::Permissions,
}

#[cfg(unix)]
impl Drop for RestoreFilePermissions<'_> {
    /// Best-effort restoration must not hide the original assertion failure.
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.path, self.permissions.clone());
    }
}

/// Deny Unix reads, skipping root or another process that bypasses file permissions.
#[cfg(unix)]
fn block_file_reads(path: &Path) -> Option<RestoreFilePermissions<'_>> {
    use std::os::unix::fs::PermissionsExt;
    let restore = RestoreFilePermissions {
        path,
        permissions: fs::metadata(path).unwrap().permissions(),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(path).is_ok() {
        eprintln!("skipping unreadable-file fixture: file permissions are bypassed (root)");
        return None;
    }
    Some(restore)
}

/// An unreadable optional body with a valid proof cannot block verified neighboring publication.
#[cfg(any(windows, unix))]
#[tokio::test]
async fn metadata_rebuild_omits_unreadable_proven_derived_output() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("rebuilt");
    generate(&data);
    convert(&data, &source).await;
    let manifest_path = source.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    let neighbor = manifest.entries[layout::GENERATED_MODEL_PATH].clone();
    let neighbor_hash = hash_file(&source.join(&neighbor.output)).unwrap();
    let bytes = fs::read(source.join(&neighbor.output)).unwrap();
    let relative = "meshes/actors/character/composed/default/male/body.glb";
    let body = source.join(relative);
    fs::create_dir_all(body.parent().unwrap()).unwrap();
    fs::write(&body, &bytes).unwrap();
    manifest
        .derived_outputs
        .insert(relative.into(), derived_body_proof(&bytes));
    manifest.save(&manifest_path).unwrap();
    let source_manifest_hash = hash_file(&manifest_path).unwrap();
    let Some(unreadable) = block_file_reads(&body) else {
        return;
    };
    assert!(fs::read(&body).is_err(), "fixture must actually deny reads");
    let result = rebuild(&data, &source, &output).await;
    drop(unreadable);
    assert!(
        result.is_ok(),
        "unreadable optional derived output aborted publication: {result:?}"
    );
    let report = result.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    assert!(report.warnings.is_empty());
    let omissions: Vec<_> = report
        .notices
        .iter()
        .filter(|notice| notice.contains("unproven retained assets"))
        .collect();
    assert_eq!(omissions.len(), 1, "{:?}", report.notices);
    assert!(omissions[0].contains("1 unproven retained assets"));
    assert!(omissions[0].contains(relative));
    assert!(
        !output.join(relative).exists(),
        "partial body must not publish"
    );
    assert_eq!(
        hash_file(&output.join(&neighbor.output)).unwrap(),
        neighbor_hash
    );
    let published = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
    assert!(published.complete);
    assert!(published.derived_outputs.is_empty());
    assert_eq!(
        published.entries[layout::GENERATED_MODEL_PATH].output_hash,
        neighbor_hash
    );
    assert_eq!(fs::read(&body).unwrap(), bytes);
    assert_eq!(
        hash_file(&source.join(&neighbor.output)).unwrap(),
        neighbor_hash
    );
    assert_eq!(hash_file(&manifest_path).unwrap(), source_manifest_hash);
}

/// The optional-output recovery must never hide an unreadable ordinary recorded asset.
#[cfg(any(windows, unix))]
#[tokio::test]
async fn metadata_rebuild_rejects_unreadable_ordinary_asset() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("rebuilt");
    generate(&data);
    convert(&data, &source).await;
    let manifest = ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
    let relative = &manifest.entries[layout::GENERATED_MODEL_PATH].output;
    let path = source.join(relative);
    let original_hash = hash_file(&path).unwrap();
    let Some(unreadable) = block_file_reads(&path) else {
        return;
    };
    assert!(fs::read(&path).is_err(), "fixture must actually deny reads");
    let result = rebuild(&data, &source, &output).await;
    drop(unreadable);
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(&format!("failed to copy retained asset {relative}")),
        "{error}"
    );
    assert!(
        !output.exists(),
        "unverified ordinary bytes must not publish"
    );
    assert_eq!(hash_file(&path).unwrap(), original_hash);
}

/// Unproven symlinks are omitted without following or modifying their external target.
#[cfg(unix)]
#[tokio::test]
async fn metadata_rebuild_skips_stray_symlinks_without_following_them() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("rebuilt");
    generate(&data);
    convert(&data, &source).await;
    let external = directory.path().join("protected.luau");
    fs::write(&external, b"protected external bytes").unwrap();
    std::os::unix::fs::symlink(&external, source.join("scripts/stray.luau")).unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains("1 unproven retained assets"))
    );
    assert!(!output.join("scripts/stray.luau").exists());
    assert_eq!(fs::read(&external).unwrap(), b"protected external bytes");
    assert!(
        source
            .join("scripts/stray.luau")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// Runtime-only packs preserve authenticated derived bytes and their original proof repeatedly.
#[tokio::test]
async fn metadata_rebuild_retains_proven_derived_bytes_without_raw_inputs() {
    for recipe in ["body-compose-v1", "body-compose-v2"] {
        assert_runtime_only_derived_reuse(recipe).await;
    }
}

/// Publishes each supported body recipe twice without consulting its original inputs.
async fn assert_runtime_only_derived_reuse(recipe: &str) {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    generate(&data);
    convert(&data, &source).await;
    assert!(!source.join("vfs").exists());
    let manifest_path = source.join("conversion-manifest.json");
    let mut manifest = ConversionManifest::load(&manifest_path).unwrap();
    let mesh = manifest
        .entries
        .values()
        .find(|entry| entry.output.ends_with(".glb"))
        .unwrap();
    let bytes = fs::read(source.join(&mesh.output)).unwrap();
    let relative = "meshes/actors/character/composed/default/male/body.glb";
    fs::create_dir_all(source.join(relative).parent().unwrap()).unwrap();
    fs::write(source.join(relative), &bytes).unwrap();
    // Moving the synthetic scene changes its relative texture base; finalize its
    // absent references before recording producer facts for the retained bytes.
    let pruned = converter::mesh::MeshConverter::prune_dangling_texture_uris(&source).unwrap();
    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0].glb, relative);
    let bytes = fs::read(source.join(relative)).unwrap();
    let mut proof = derived_body_proof(&bytes);
    proof.recipe = recipe.into();
    if recipe == "body-compose-v2" {
        proof.inputs.insert(
            "recipes/body-compose-v2/context.json".into(),
            hash_bytes(b"[[[\"body\",null],[\"hands\",null],[\"feet\",null]],[]]"),
        );
    }
    manifest
        .derived_outputs
        .insert(relative.into(), proof.clone());
    manifest.save(&manifest_path).unwrap();
    let source_hash = hash_file(&manifest_path).unwrap();
    let mut previous = source.clone();
    for name in ["rebuilt", "repeated"] {
        let output = directory.path().join(name);
        let report = rebuild(&data, &previous, &output).await.unwrap();
        assert!(report.complete, "{:?}", report.warnings);
        assert!(
            !report
                .notices
                .iter()
                .any(|notice| notice.contains("unproven retained"))
        );
        assert_eq!(fs::read(output.join(relative)).unwrap(), bytes);
        let current = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert_eq!(current.derived_outputs.get(relative), Some(&proof));
        let catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("streaming-costs.json")).unwrap())
                .unwrap();
        assert!(catalog.to_string().contains(relative));
        previous = output;
    }
    assert_eq!(hash_file(&manifest_path).unwrap(), source_hash);
}

/// Malformed, unsupported, missing and corrupted optional outputs cannot block valid neighbors.
#[tokio::test]
async fn metadata_rebuild_omits_invalid_individual_derived_proofs() {
    for case in [
        "recipe",
        "size",
        "hash",
        "inputs",
        "missing",
        "malformed",
        "malformed-missing",
        "escaping",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let source = directory.path().join("source");
        let output = directory.path().join("rebuilt");
        generate(&data);
        convert(&data, &source).await;
        let manifest_path = source.join("conversion-manifest.json");
        let manifest = ConversionManifest::load(&manifest_path).unwrap();
        let neighbor = manifest
            .entries
            .values()
            .find(|entry| entry.output.ends_with(".glb"))
            .unwrap();
        let neighbor_hash = hash_file(&source.join(&neighbor.output)).unwrap();
        let bytes = fs::read(source.join(&neighbor.output)).unwrap();
        let relative = "meshes/actors/character/composed/default/male/body.glb";
        let mut proof = serde_json::to_value(derived_body_proof(&bytes)).unwrap();
        match case {
            "recipe" => proof["recipe"] = "future-body-v99".into(),
            "size" => proof["output_size"] = 1.into(),
            "hash" => proof["output_hash"] = "0".repeat(64).into(),
            "inputs" => proof["inputs"] = serde_json::json!({"wrong.nif": "0".repeat(64)}),
            "malformed" | "malformed-missing" => proof["output_size"] = "not an integer".into(),
            _ => {}
        }
        let mut document = serde_json::to_value(&manifest).unwrap();
        let proof_path = if case == "escaping" {
            "../escaped.glb"
        } else {
            relative
        };
        document["derived_outputs"] = serde_json::json!({proof_path: proof});
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();
        if !["missing", "malformed-missing", "escaping"].contains(&case) {
            fs::create_dir_all(source.join(relative).parent().unwrap()).unwrap();
            fs::write(source.join(relative), &bytes).unwrap();
        }
        let source_hash = hash_file(&manifest_path).unwrap();
        let result = rebuild(&data, &source, &output).await;
        assert!(result.is_ok(), "{case}: {result:?}");
        let report = result.unwrap();
        assert!(report.complete, "{case}: {:?}", report.warnings);
        assert_eq!(
            report
                .notices
                .iter()
                .filter(|notice| notice.contains("1 unproven retained assets"))
                .count(),
            1,
            "{case}: {:?}",
            report.notices
        );
        assert!(!output.join(relative).exists(), "{case}");
        assert!(!directory.path().join("escaped.glb").exists());
        assert_eq!(
            hash_file(&output.join(&neighbor.output)).unwrap(),
            neighbor_hash,
            "{case}"
        );
        assert!(
            ConversionManifest::load(&output.join("conversion-manifest.json"))
                .unwrap()
                .derived_outputs
                .is_empty(),
            "{case}"
        );
        assert_eq!(hash_file(&manifest_path).unwrap(), source_hash, "{case}");
    }
}

/// Adds a scoped volcanic model whose authored normal is absent but DLC02 source exists.
fn add_volcanic_source(data: &Path) {
    let model = data.join(VOLCANIC_MODEL);
    fs::create_dir_all(model.parent().unwrap()).unwrap();
    fs::write(
        model,
        nif::static_shape(&nif::StaticShape {
            name: "VolcanicReuseTriangle",
            positions: &[[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 3.0, 0.0]],
            normals: &[[0.0, 0.0, 1.0]; 3],
            uvs: &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: &[[0, 1, 2]],
            diffuse: layout::GENERATED_DIFFUSE_PATH,
            normal_texture: VOLCANIC_ORIGINAL,
        })
        .unwrap(),
    )
    .unwrap();
    let target = data.join(VOLCANIC_TARGET);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::copy(data.join(layout::GENERATED_NORMAL_PATH), target).unwrap();
}

/// Reads the external normal URI without reproducing the repair's path mapping.
fn volcanic_normal_uri(glb: &Path) -> Option<String> {
    converter::mesh::MeshConverter::glb_texture_dependencies(glb)
        .unwrap()
        .into_iter()
        .find(|dependency| dependency.uri.contains("volcanicashgrass02_n"))
        .map(|dependency| dependency.uri)
}

/// Frame the light's enable-parent target in the generated worldspace's first exterior cell.
fn enable_parent_target() -> Vec<u8> {
    let payload = [
        b"NAME".as_slice(),
        &4u16.to_le_bytes(),
        &3u32.to_le_bytes(),
        b"DATA".as_slice(),
        &24u16.to_le_bytes(),
        &[0; 24],
    ]
    .concat();
    let reference = [
        b"REFR".as_slice(),
        &(payload.len() as u32).to_le_bytes(),
        &0u32.to_le_bytes(),
        &0x1234u32.to_le_bytes(),
        &0u32.to_le_bytes(),
        &44u16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &payload,
    ]
    .concat();
    // The dummy writer assigns worldspace 1 and its first exterior CELL 0x10.
    // Preserve both parent contexts while adding a separate cell-children group.
    let cell_children = [
        b"GRUP".as_slice(),
        &(reference.len() as u32 + 24).to_le_bytes(),
        &0x10u32.to_le_bytes(),
        &8i32.to_le_bytes(),
        &[0; 8],
        &reference,
    ]
    .concat();
    [
        b"GRUP".as_slice(),
        &(cell_children.len() as u32 + 24).to_le_bytes(),
        &1u32.to_le_bytes(),
        &1i32.to_le_bytes(),
        &[0; 8],
        &cell_children,
    ]
    .concat()
}

/// Generate complete assets and a light whose enable-parent resolves to an authored reference.
fn generate(data: &Path) {
    layout::prepare_directory(data, false).unwrap();
    layout::generate(
        data,
        layout::DEFAULT_SEED,
        layout::Formats::parse("dds,nif,pex,esm,lodsettings").unwrap(),
    )
    .unwrap();
    let plugin = esm::Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    };
    let light = esm::Light {
        model_path: None,
        reference_flags: 0x800,
        enable_parent: Some((0x1234, 1)),
        ..esm::PRESET_LIGHT
    };
    let mut plugin_bytes = esm::plugin_with_lights(&plugin, &light).unwrap();
    plugin_bytes.extend(enable_parent_target());
    fs::write(data.join("Skyrim.esm"), plugin_bytes).unwrap();
    fs::write(
        data.join("Skyrim - Misc.bsa"),
        bsa::v105(
            &[
                Entry {
                    name: "LODSettings/GeneratedWorld.LOD",
                    data: &PACKED_SETTINGS,
                },
                Entry {
                    name: "misc/unrelated.txt",
                    data: b"not a setting",
                },
            ],
            bsa::Compression::None,
        )
        .unwrap(),
    )
    .unwrap();
}

async fn convert(data: &Path, output: &Path) -> converter::PipelineReport {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(common::cpu_lod_config(data, output), tx)
        .await
        .unwrap();
    drain.await.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    report
}

async fn rebuild(
    data: &Path,
    source: &Path,
    output: &Path,
) -> color_eyre::Result<converter::PipelineReport> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result =
        AssetPipeline::rebuild_metadata_async(common::cpu_lod_config(data, output), source, tx)
            .await;
    drain.await.unwrap();
    result
}

/// Recreate the legacy schema-15 producer proof alongside its historical database shape.
fn schema_four_source(source: &Path) {
    fs::write(source.join("record-reader.json"), br#"{"mode":"legacy"}"#).unwrap();
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(source.join("conversion-manifest.json")).unwrap())
            .unwrap();
    manifest.schema_version = 15;
    manifest.asset_configuration_hashes.clear();
    let mut legacy_config = PipelineConfig::new(".", ".");
    legacy_config.record_reader = converter::config::RecordReader::Legacy;
    manifest.configuration_hash =
        converter::cache::configuration_hash_for_schema(&legacy_config, 15).unwrap();
    manifest
        .save(&source.join("conversion-manifest.json"))
        .unwrap();
    let connection = Connection::open(source.join("skyrim_world.db")).unwrap();
    connection
        .execute_batch(
            "UPDATE schema_info SET version=4;
        ALTER TABLE \"references\" DROP COLUMN header_flags;
        ALTER TABLE \"references\" DROP COLUMN enable_parent_id;
        ALTER TABLE \"references\" DROP COLUMN enable_parent_flags;
        ALTER TABLE worldspaces DROP COLUMN lod_origin_x;
        ALTER TABLE worldspaces DROP COLUMN lod_origin_y;",
        )
        .unwrap();
}

fn native_configuration_hash(config: &PipelineConfig, zstd_level: u32) -> String {
    converter::cache::hash_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "schema": 16,
            "texture_etc1s_quality": config.texture_fallback_quality,
            "texture_uastc_level": config.texture_uastc_level,
            "texture_zstd_level": zstd_level,
            "script_abi_version": config.script_abi_version,
        }))
        .unwrap(),
    )
}

/// A schema-16 native producer cannot be relabeled as current by metadata rebuild: its KTX2
/// textures predate the current layout, so the source must be regenerated by normal conversion.
#[tokio::test]
async fn native_pre_producer_28_configuration_is_rejected_not_relabeled() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    convert(&data, &source).await;
    let config = common::cpu_lod_config(&data, &output);
    let producer_hash = native_configuration_hash(&config, 6);
    assert_eq!(
        producer_hash,
        "ebc4fe2f7d531e796e6a5e22b70a1f6a877c01e963b54b416b789d0f64312427"
    );
    let path = source.join("conversion-manifest.json");
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest.schema_version = 16;
    manifest.asset_configuration_hashes.clear();
    manifest.configuration_hash = producer_hash;
    manifest.save(&path).unwrap();
    fs::write(source.join("record-reader.json"), br#"{"mode":"legacy"}"#).unwrap();
    let error = rebuild(&data, &source, &output).await.unwrap_err();
    assert!(
        format!("{error:?}").contains("predate the current texture layout"),
        "{error:?}"
    );
    assert!(!output.exists());
}

/// Changed native settings fail against the exact recorded legacy producer identity.
#[tokio::test]
async fn metadata_rebuild_rejects_changed_native_producer_settings() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    generate(&data);
    convert(&data, &source).await;
    let path = source.join("conversion-manifest.json");
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (index, retained) in [false, true].into_iter().enumerate() {
        let output = directory.path().join(format!("rejected-{index}"));
        let changed = native_configuration_hash(&common::cpu_lod_config(&data, &output), 7);
        if retained {
            manifest.schema_version = converter::cache::CONVERTER_SCHEMA_VERSION;
            let mut legacy_config = PipelineConfig::new(&data, &output);
            legacy_config.record_reader = converter::config::RecordReader::Legacy;
            manifest.configuration_hash =
                converter::cache::configuration_hash(&legacy_config).unwrap();
            manifest.retained_mesh_schema_version =
                Some(converter::cache::CONVERTER_SCHEMA_VERSION);
            manifest.asset_configuration_hashes.clear();
            manifest.retained_asset_configuration_hash = Some(changed);
            fs::write(
                source.join("record-reader.json"),
                serde_json::to_vec(&converter::esm::inhouse::reader_identity(
                    converter::config::RecordReader::Legacy,
                ))
                .unwrap(),
            )
            .unwrap();
            fs::write(
                source.join("metadata-rebuild.json"),
                br#"{"retained_asset_record_reader":{"mode":"legacy"}}"#,
            )
            .unwrap();
        } else {
            manifest.schema_version = 16;
            manifest.asset_configuration_hashes.clear();
            manifest.configuration_hash = changed;
            fs::write(source.join("record-reader.json"), br#"{"mode":"legacy"}"#).unwrap();
        }
        manifest.save(&path).unwrap();
        let error = rebuild(&data, &source, &output).await.unwrap_err();
        let expected = if retained {
            "retained producer configuration does not match rebuild settings"
        } else {
            "predate the current texture layout"
        };
        assert!(format!("{error:?}").contains(expected), "{error:?}");
        assert!(!output.exists());
    }
}

#[tokio::test]
async fn metadata_rebuild_reuses_bytes_and_recovers_authoritative_flags() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    convert(&data, &source).await;
    let old_manifest = hash_file(&source.join("conversion-manifest.json")).unwrap();
    let old_db = hash_file(&source.join("skyrim_world.db")).unwrap();
    let source_manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(source.join("conversion-manifest.json")).unwrap())
            .unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.complete);
    assert_eq!(report.converted, 0);
    assert!(report.cache_hits > 0 && report.lod_chunks > 0);
    assert_eq!(
        hash_file(&source.join("conversion-manifest.json")).unwrap(),
        old_manifest
    );
    assert_eq!(hash_file(&source.join("skyrim_world.db")).unwrap(), old_db);
    for entry in source_manifest.entries.values() {
        assert_eq!(
            hash_file(&output.join(&entry.output)).unwrap(),
            entry.output_hash
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                fs::metadata(source.join(&entry.output)).unwrap().ino(),
                fs::metadata(output.join(&entry.output)).unwrap().ino()
            );
        }
    }
    let connection = Connection::open(output.join("skyrim_world.db")).unwrap();
    let flags: (u32, u32, u32) = connection.query_row(
        "SELECT header_flags, enable_parent_id, enable_parent_flags FROM \"references\" WHERE header_flags=2048", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(flags, (0x800, 0x1234, 1));
    let version: u32 = connection
        .query_row("SELECT version FROM schema_info", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, shared::WORLD_DATABASE_SCHEMA_VERSION);
    let provenance: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("metadata-rebuild.json")).unwrap()).unwrap();
    assert_eq!(
        provenance["source_converter_schema"],
        converter::cache::CONVERTER_SCHEMA_VERSION
    );
    assert_eq!(
        provenance["source_retained_mesh_schema"],
        converter::cache::CONVERTER_SCHEMA_VERSION
    );
    assert_eq!(provenance["source_manifest_hash"], old_manifest);
    assert_eq!(
        hash_file(&output.join("metadata-source-conversion-manifest.json")).unwrap(),
        old_manifest
    );
    assert!(output.join("lod-manifest.json").is_file());
}

/// Metadata output from a current-producer source keeps its GLBs as normal cache hits, and a
/// repeated rebuild regenerates nothing that the normal route would not regenerate.
#[tokio::test]
async fn metadata_rebuild_of_current_source_keeps_meshes_as_cache_hits() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    let repeated = directory.path().join("derived-again");
    generate(&data);
    convert(&data, &source).await;
    rebuild(&data, &source, &output).await.unwrap();
    rebuild(&data, &output, &repeated).await.unwrap();
    let retained: ConversionManifest =
        serde_json::from_slice(&fs::read(repeated.join("conversion-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        retained.retained_mesh_schema_version,
        Some(converter::cache::CONVERTER_SCHEMA_VERSION)
    );
    let meshes = retained
        .entries
        .values()
        .filter(|entry| entry.output.ends_with(".glb"))
        .count();
    assert!(meshes > 0);
    let eligible = ConversionManifest::load(&repeated.join("conversion-manifest.json")).unwrap();
    assert_eq!(
        eligible
            .entries
            .values()
            .filter(|entry| entry.output.ends_with(".glb"))
            .count(),
        meshes,
        "current-producer GLBs remain reusable"
    );
    let report = convert(&data, &repeated).await;
    let current: ConversionManifest =
        serde_json::from_slice(&fs::read(repeated.join("conversion-manifest.json")).unwrap())
            .unwrap();
    let extracted = current
        .archives
        .values()
        .map(|archive| archive.files.len())
        .sum::<usize>();
    assert_eq!(report.converted, extracted as u64);
    assert_eq!(report.cache_hits, retained.entries.len() as u64);
    assert_eq!(current.retained_mesh_schema_version, None);
}

/// Invalid source/retained producer pairs are stamped faithfully and rejected.
#[tokio::test]
async fn metadata_rebuild_rejects_unsupported_mesh_provenance() {
    // Only producer-27 meshes may survive into the current schema; producers 24 and 26 never do.
    let current = converter::cache::CONVERTER_SCHEMA_VERSION;
    for (schema, mesh_schema) in [
        (current, 24),
        (current, 26),
        (current, 25),
        (current, current + 1),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let source = directory.path().join("source");
        let output = directory.path().join("derived");
        generate(&data);
        convert(&data, &source).await;
        if schema == 15 {
            schema_four_source(&source);
        }
        let mut manifest: ConversionManifest =
            serde_json::from_slice(&fs::read(source.join("conversion-manifest.json")).unwrap())
                .unwrap();
        manifest.schema_version = schema;
        manifest.retained_mesh_schema_version = Some(mesh_schema);
        manifest.asset_configuration_hashes.clear();
        manifest
            .save(&source.join("conversion-manifest.json"))
            .unwrap();
        let error = rebuild(&data, &source, &output)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unsupported retained mesh cache contract"),
            "{error}"
        );
        assert!(!output.exists());
    }
}

#[tokio::test]
async fn markerless_legacy_metadata_never_promotes_retained_meshes() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let legacy = directory.path().join("legacy-derived");
    let output = directory.path().join("derived-again");
    generate(&data);
    convert(&data, &source).await;
    rebuild(&data, &source, &legacy).await.unwrap();
    let manifest_path = legacy.join("conversion-manifest.json");
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.retained_mesh_schema_version = None;
    manifest.save(&manifest_path).unwrap();
    fs::remove_file(legacy.join("metadata-source-conversion-manifest.json")).unwrap();
    let eligible = ConversionManifest::load(&manifest_path).unwrap();
    assert!(
        eligible
            .entries
            .values()
            .all(|entry| !entry.output.ends_with(".glb")),
        "markerless metadata-only output must not become a mesh cache hit"
    );
    let error = rebuild(&data, &legacy, &output)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("missing retained mesh provenance"),
        "{error}"
    );
    assert!(!output.exists());
}

#[tokio::test]
async fn metadata_rebuild_rejects_changed_plugins_and_assets_without_publication() {
    for corrupt_plugin in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let source = directory.path().join("source");
        let output = directory.path().join("derived");
        generate(&data);
        convert(&data, &source).await;
        if corrupt_plugin {
            fs::write(data.join("Skyrim.esm"), b"different plugin").unwrap();
        } else {
            let manifest =
                ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
            let asset = &manifest.entries.values().next().unwrap().output;
            fs::write(source.join(asset), b"corrupt asset").unwrap();
        }
        let error = rebuild(&data, &source, &output)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!output.exists());
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }
}

#[tokio::test]
async fn metadata_rebuild_requires_new_disjoint_output() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    generate(&data);
    convert(&data, &source).await;
    let original = hash_file(&source.join("conversion-manifest.json")).unwrap();
    for output in [&source, &source.join("nested"), &data.join("nested")] {
        assert!(rebuild(&data, &source, output).await.is_err());
    }
    let existing = directory.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("sentinel"), b"keep").unwrap();
    assert!(rebuild(&data, &source, &existing).await.is_err());
    assert_eq!(fs::read(existing.join("sentinel")).unwrap(), b"keep");
    assert_eq!(
        hash_file(&source.join("conversion-manifest.json")).unwrap(),
        original
    );
}

#[tokio::test]
async fn metadata_rebuild_preserves_package_snapshot_after_unrelated_data_asset_changes() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    convert(&data, &source).await;
    let manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(source.join("conversion-manifest.json")).unwrap())
            .unwrap();
    fs::write(
        data.join(layout::GENERATED_MODEL_PATH),
        b"changed NIF override",
    )
    .unwrap();
    fs::write(
        data.join(layout::GENERATED_NORMAL_PATH),
        b"changed normal DDS override",
    )
    .unwrap();
    fs::write(data.join("scripts/generated.pex"), b"changed PEX override").unwrap();
    fs::write(data.join("meshes/new.nif"), b"new asset").unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.complete);
    assert_eq!(report.converted, 0);
    for entry in manifest.entries.values() {
        assert_eq!(
            hash_file(&output.join(&entry.output)).unwrap(),
            entry.output_hash
        );
        assert_eq!(
            hash_file(&source.join(&entry.output)).unwrap(),
            entry.output_hash
        );
    }
    assert!(!output.join("meshes/new.glb").exists());
}

#[tokio::test]
async fn metadata_rebuild_resolves_current_packed_settings_and_omits_stale_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    convert(&data, &source).await;
    fs::remove_file(data.join("lodsettings/GeneratedWorld.lod")).unwrap();
    fs::write(
        data.join("Unmatched - Misc.bsa"),
        bsa::v105(
            &[Entry::new(
                "LODSettings/GeneratedWorld.LOD",
                b"invalid unmatched override",
            )],
            bsa::Compression::None,
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(source.join("lod/stale.glb"), b"obsolete generation").unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.lod_chunks > 0);
    assert!(
        report
            .lod_warnings
            .iter()
            .any(|warning| warning.contains("no matching source-package plugin"))
    );
    assert!(!output.join("lod/stale.glb").exists());
    assert!(!output.join("vfs/misc/unrelated.txt").exists());
    let connection = Connection::open(output.join("skyrim_world.db")).unwrap();
    let origin: (i32, i32) = connection
        .query_row(
            "SELECT lod_origin_x,lod_origin_y FROM worldspaces WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(origin, (-4, -4));
    let indexed: u64 = connection
        .query_row("SELECT count(*) FROM lod_chunks_spatial", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(indexed, report.lod_chunks);
}

/// Package-matched selection must not admit a recovered-away longer owner's LOD settings.
#[tokio::test]
async fn metadata_rebuild_excludes_recovered_longer_archive_owner() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    fs::remove_file(data.join("lodsettings/GeneratedWorld.lod")).unwrap();

    // This is a usable plugin header with a missing dependency, not undecodable bytes.
    let missing_master = b"Missing.esm\0";
    let header = [
        b"MAST".as_slice(),
        &(missing_master.len() as u16).to_le_bytes(),
        missing_master,
        b"DATA".as_slice(),
        &8u16.to_le_bytes(),
        &[0; 8],
    ]
    .concat();
    fs::write(
        data.join("Skyrim - ZBroken.esp"),
        [
            b"TES4".as_slice(),
            &(header.len() as u32).to_le_bytes(),
            &[0; 16],
            &header,
        ]
        .concat(),
    )
    .unwrap();
    let mut excluded_settings = PACKED_SETTINGS;
    excluded_settings[..2].copy_from_slice(&(-8i16).to_le_bytes());
    excluded_settings[2..4].copy_from_slice(&(-8i16).to_le_bytes());
    fs::write(
        data.join("Skyrim - ZBroken.bsa"),
        bsa::v105(
            &[Entry::new(
                "LODSettings/GeneratedWorld.LOD",
                &excluded_settings,
            )],
            bsa::Compression::None,
        )
        .unwrap(),
    )
    .unwrap();

    let converted = convert(&data, &source).await;
    assert!(
        converted
            .notices
            .iter()
            .any(|warning| warning.contains("Missing.esm"))
    );
    let source_manifest_hash = hash_file(&source.join("conversion-manifest.json")).unwrap();
    let source_database_hash = hash_file(&source.join("skyrim_world.db")).unwrap();
    let manifest = ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
    {
        let connection = Connection::open(source.join("skyrim_world.db")).unwrap();
        converter::esm::exporter::validate_database(&connection).unwrap();
        let names: Vec<String> = connection
            .prepare("SELECT name FROM plugins ORDER BY priority")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(names, ["Skyrim.esm"]);
        let origin: (i32, i32) = connection
            .query_row(
                "SELECT lod_origin_x,lod_origin_y FROM worldspaces WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, (-4, -4));
    }

    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.complete && report.lod_chunks > 0);
    assert_eq!(report.converted, 0);
    for entry in manifest.entries.values() {
        assert_eq!(
            hash_file(&output.join(&entry.output)).unwrap(),
            entry.output_hash
        );
        assert_eq!(
            hash_file(&source.join(&entry.output)).unwrap(),
            entry.output_hash
        );
    }
    let connection = Connection::open(output.join("skyrim_world.db")).unwrap();
    converter::esm::exporter::validate_database(&connection).unwrap();
    let version: u32 = connection
        .query_row("SELECT version FROM schema_info", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, shared::WORLD_DATABASE_SCHEMA_VERSION);
    let origin: (i32, i32) = connection
        .query_row(
            "SELECT lod_origin_x,lod_origin_y FROM worldspaces WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(origin, (-4, -4));
    drop(connection);
    assert_eq!(
        hash_file(&source.join("conversion-manifest.json")).unwrap(),
        source_manifest_hash
    );
    assert_eq!(
        hash_file(&source.join("skyrim_world.db")).unwrap(),
        source_database_hash
    );
}

/// Verified reuse preserves the repaired URI and bytes despite changed current Data assets.
#[tokio::test]
async fn metadata_rebuild_preserves_repaired_volcanic_normal_uri() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    let output = directory.path().join("derived");
    generate(&data);
    add_volcanic_source(&data);
    convert(&data, &source).await;
    let uri = volcanic_normal_uri(&source.join(VOLCANIC_GLB)).unwrap();
    assert!(uri.contains("textures/dlc02/landscape/grass/volcanicashgrass02_n"));
    let manifest = ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
    assert!(
        manifest.entries[VOLCANIC_MODEL]
            .source_hash
            .ends_with(":volcanic-normal-repair:1:original=false:target=true")
    );
    let source_manifest_hash = hash_file(&source.join("conversion-manifest.json")).unwrap();
    let retained_hash = hash_file(&source.join(VOLCANIC_GLB)).unwrap();
    assert!(
        !source.join("vfs").exists(),
        "exercise normal runtime-package reuse"
    );

    // These changes would undo the repair during normal conversion. Reuse keeps SOURCE's bytes.
    let original = data.join(VOLCANIC_ORIGINAL);
    fs::create_dir_all(original.parent().unwrap()).unwrap();
    fs::copy(data.join(layout::GENERATED_NORMAL_PATH), original).unwrap();
    fs::remove_file(data.join(VOLCANIC_TARGET)).unwrap();
    fs::write(data.join(VOLCANIC_MODEL), b"changed current NIF override").unwrap();
    let report = rebuild(&data, &source, &output).await.unwrap();
    assert!(report.complete);
    assert_eq!(report.converted, 0);
    assert_eq!(volcanic_normal_uri(&output.join(VOLCANIC_GLB)), Some(uri));
    assert_eq!(
        hash_file(&output.join(VOLCANIC_GLB)).unwrap(),
        retained_hash
    );
    for entry in manifest.entries.values() {
        assert_eq!(
            hash_file(&output.join(&entry.output)).unwrap(),
            entry.output_hash
        );
    }
    let provenance: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("metadata-rebuild.json")).unwrap()).unwrap();
    assert_eq!(
        provenance["replay_verified_pruned_assets"],
        serde_json::json!([])
    );
    assert_eq!(
        hash_file(&source.join("conversion-manifest.json")).unwrap(),
        source_manifest_hash
    );
}

/// Legacy replay accepts old and scoped hashes using SOURCE's context and never applies repair.
#[tokio::test]
async fn metadata_rebuild_replays_volcanic_prunes_with_legacy_and_scoped_source_hashes() {
    for (scoped_hash, wrong_presence, corrupt_nif) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let source = directory.path().join("source");
        let output = directory.path().join("derived");
        let preprune = directory.path().join("preprune");
        generate(&data);
        add_volcanic_source(&data);
        let skeleton_key = "meshes/landscape/grass/skeleton.nif";
        fs::copy(
            data.join(layout::GENERATED_MODEL_PATH),
            data.join(skeleton_key),
        )
        .unwrap();
        convert(&data, &source).await;
        let historical_hash = format!(
            "{}:{}",
            hash_file(&data.join(VOLCANIC_MODEL)).unwrap(),
            hash_file(&data.join(skeleton_key)).unwrap()
        );
        let scoped_source_hash =
            format!("{historical_hash}:volcanic-normal-repair:1:original=false:target=true");
        let converted = ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
        assert_eq!(
            converted.entries[VOLCANIC_MODEL].source_hash,
            scoped_source_hash
        );

        // Build the historical raw GLB and retain a pure prune with no DLC02 repair available.
        let candidate = preprune.join(VOLCANIC_GLB);
        converter::mesh::MeshConverter::convert_nif_to_glb(
            data.join(VOLCANIC_MODEL),
            candidate.clone(),
        )
        .unwrap();
        let original_hash = hash_file(&candidate).unwrap();
        let original_size = fs::metadata(&candidate).unwrap().len();
        for dependency in converter::mesh::MeshConverter::glb_texture_dependencies(&candidate)
            .unwrap()
            .into_iter()
            .filter(|dependency| !dependency.uri.contains("volcanicashgrass02_n"))
        {
            let published = converter::asset_path::resolve_asset_uri(
                &source,
                &source.join(VOLCANIC_GLB),
                &dependency.uri,
            )
            .unwrap();
            let retained =
                converter::asset_path::resolve_asset_uri(&preprune, &candidate, &dependency.uri)
                    .unwrap();
            fs::create_dir_all(retained.parent().unwrap()).unwrap();
            fs::copy(published, retained).unwrap();
        }
        let pruned =
            converter::mesh::MeshConverter::prune_dangling_texture_uris(&preprune).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].removed_uris.len(), 1);
        let missing = converter::asset_path::resolve_asset_uri(
            &preprune,
            &candidate,
            &pruned[0].removed_uris[0],
        )
        .unwrap();
        let missing = missing
            .strip_prefix(&preprune)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(
            missing,
            "textures/landscape/grass/volcanicashgrass02_n.opensky-wrap0.ktx2"
        );
        assert!(volcanic_normal_uri(&candidate).is_none());
        fs::copy(&candidate, source.join(VOLCANIC_GLB)).unwrap();

        let retained_nif = source.join("vfs").join(VOLCANIC_MODEL);
        fs::create_dir_all(retained_nif.parent().unwrap()).unwrap();
        fs::copy(data.join(VOLCANIC_MODEL), &retained_nif).unwrap();
        fs::copy(
            data.join(skeleton_key),
            source.join("vfs").join(skeleton_key),
        )
        .unwrap();
        let mut manifest =
            ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
        let entry = manifest.entries.get_mut(VOLCANIC_MODEL).unwrap();
        entry.output_hash = original_hash;
        entry.output_size = original_size;
        // Both historical contracts hash the discovered skeleton before the scoped suffix.
        entry.source_hash = historical_hash;
        if scoped_hash {
            entry.source_hash.push_str(if wrong_presence {
                ":volcanic-normal-repair:1:original=true:target=true"
            } else {
                ":volcanic-normal-repair:1:original=false:target=true"
            });
        }
        manifest
            .pruned_texture_references
            .insert(VOLCANIC_GLB.into(), [missing].into());
        manifest
            .save(&source.join("conversion-manifest.json"))
            .unwrap();
        let source_manifest_hash = hash_file(&source.join("conversion-manifest.json")).unwrap();
        let retained_hash = hash_file(&source.join(VOLCANIC_GLB)).unwrap();
        if corrupt_nif {
            fs::write(&retained_nif, b"tampered retained source NIF").unwrap();
        }

        // Data's current presence bits differ; only the retained package can verify its hash.
        let original = data.join(VOLCANIC_ORIGINAL);
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::copy(data.join(layout::GENERATED_NORMAL_PATH), original).unwrap();
        fs::remove_file(data.join(VOLCANIC_TARGET)).unwrap();
        let result = rebuild(&data, &source, &output).await;
        if wrong_presence || corrupt_nif {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("pruned GLB source checksum mismatch"),
                "{error}"
            );
            assert!(!output.exists());
        } else {
            let report = result.unwrap();
            assert!(report.complete);
            assert_eq!(
                hash_file(&output.join(VOLCANIC_GLB)).unwrap(),
                retained_hash
            );
            assert!(volcanic_normal_uri(&output.join(VOLCANIC_GLB)).is_none());
            let updated =
                ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
            assert_eq!(updated.entries[VOLCANIC_MODEL].output_hash, retained_hash);
            let provenance: serde_json::Value =
                serde_json::from_slice(&fs::read(output.join("metadata-rebuild.json")).unwrap())
                    .unwrap();
            assert_eq!(
                provenance["replay_verified_pruned_assets"],
                serde_json::json!([VOLCANIC_GLB])
            );
        }
        assert_eq!(
            hash_file(&source.join(VOLCANIC_GLB)).unwrap(),
            retained_hash
        );
        assert_eq!(
            hash_file(&source.join("conversion-manifest.json")).unwrap(),
            source_manifest_hash
        );
    }
}

#[tokio::test]
async fn metadata_rebuild_v87_accepts_old_prune_hashes_only_after_exact_source_replay() {
    for (corrupt, with_skeleton, with_vfs) in [
        (false, false, true),
        (true, false, true),
        (false, true, true),
        (false, false, false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let source = directory.path().join("source");
        let output = directory.path().join("derived");
        generate(&data);
        if with_skeleton {
            // Static meshes also record a discovered skeleton's dependency hash.
            fs::copy(
                data.join(layout::GENERATED_MODEL_PATH),
                data.join("meshes/skeleton.nif"),
            )
            .unwrap();
        }
        fs::remove_file(data.join(layout::GENERATED_NORMAL_PATH)).unwrap();
        convert(&data, &source).await;
        // Historical packs kept verified raw VFS files; main's runtime packs do not.
        let legacy_meshes = source.join("vfs/meshes");
        fs::create_dir_all(&legacy_meshes).unwrap();
        fs::copy(
            data.join(layout::GENERATED_MODEL_PATH),
            legacy_meshes.join("generated.nif"),
        )
        .unwrap();
        if with_skeleton {
            fs::copy(
                data.join("meshes/skeleton.nif"),
                legacy_meshes.join("skeleton.nif"),
            )
            .unwrap();
        }
        let mut manifest =
            ConversionManifest::load(&source.join("conversion-manifest.json")).unwrap();
        let key = layout::GENERATED_MODEL_PATH;
        let candidate = directory.path().join("preprune/meshes/generated.glb");
        converter::mesh::MeshConverter::convert_nif_to_glb(data.join(key), candidate.clone())
            .unwrap();
        let entry = manifest.entries.get_mut(key).unwrap();
        entry.output_hash = hash_file(&candidate).unwrap();
        entry.output_size = fs::metadata(&candidate).unwrap().len();
        manifest
            .save(&source.join("conversion-manifest.json"))
            .unwrap();
        if corrupt {
            fs::write(source.join("meshes/generated.glb"), b"tampered").unwrap();
        }
        if !with_vfs {
            fs::remove_dir_all(source.join("vfs")).unwrap();
        }
        let retained = hash_file(&source.join("meshes/generated.glb")).unwrap();
        let result = rebuild(&data, &source, &output).await;
        if !with_vfs {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("legacy pruned GLB replay requires raw source NIF"),
                "{error}"
            );
            assert!(!output.exists());
        } else if corrupt {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("checksum mismatch")
            );
            assert!(!output.exists());
        } else {
            assert!(result.unwrap().complete);
            assert_eq!(
                hash_file(&output.join("meshes/generated.glb")).unwrap(),
                retained
            );
            let updated =
                ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
            assert_eq!(updated.entries[key].output_hash, retained);
            let provenance: serde_json::Value =
                serde_json::from_slice(&fs::read(output.join("metadata-rebuild.json")).unwrap())
                    .unwrap();
            assert_eq!(
                provenance["replay_verified_pruned_assets"],
                serde_json::json!(["meshes/generated.glb"])
            );
        }
        assert_eq!(
            hash_file(&source.join("meshes/generated.glb")).unwrap(),
            retained
        );
    }
}

/// Historical producer 17/18 and the separate 25 candidates are ambiguous for rebuild.
#[tokio::test]
async fn v91_metadata_rebuild_rejects_ambiguous_lod_and_lighting_producers() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let source = directory.path().join("source");
    generate(&data);
    convert(&data, &source).await;
    let path = source.join("conversion-manifest.json");
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for schema in [17, 18, 25] {
        manifest.schema_version = schema;
        if schema != converter::cache::CONVERTER_SCHEMA_VERSION {
            manifest.asset_configuration_hashes.clear();
        }
        manifest.save(&path).unwrap();
        let output = directory.path().join(format!("derived-{schema}"));
        let error = rebuild(&data, &source, &output)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("predate the current texture layout"),
            "{error}"
        );
        assert!(!output.exists());
    }
}
