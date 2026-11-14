//! Opt-in authored cow/chicken idle contract probe for issue #471.
//! It extracts eight inputs into task-owned scratch and never changes a packaged dataset.

use converter::{
    animation::{TransformSample, decode_clip_with_skeleton, write_clip_glb},
    archive::ArchiveExtractor,
    compose::{BodyPart, BodyPartKind, compose_body},
    mesh::MeshConverter,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn glb_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"glTF");
    let length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    assert_eq!(&bytes[16..20], b"JSON");
    serde_json::from_slice(&bytes[20..20 + length]).unwrap()
}

fn target_paths(document: &Value) -> BTreeMap<usize, Vec<String>> {
    fn visit(
        document: &Value,
        index: usize,
        mut path: Vec<String>,
        found: &mut BTreeMap<usize, Vec<String>>,
    ) {
        let node = &document["nodes"][index];
        let Some(name) = node["name"].as_str() else {
            return;
        };
        if name == "Creation-to-glTF basis" {
            path.clear();
        }
        path.push(name.to_owned());
        if path[0] == "Creation-to-glTF basis" {
            found.insert(index, path.clone());
        }
        if let Some(children) = node["children"].as_array() {
            for child in children {
                visit(
                    document,
                    child.as_u64().unwrap() as usize,
                    path.clone(),
                    found,
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    let scene = document["scene"].as_u64().unwrap_or(0) as usize;
    for node in document["scenes"][scene]["nodes"].as_array().unwrap() {
        visit(
            document,
            node.as_u64().unwrap() as usize,
            Vec::new(),
            &mut found,
        );
    }
    found
}

fn differs(a: &TransformSample, b: &TransformSample) -> bool {
    a.translation
        .iter()
        .zip(b.translation)
        .any(|(a, b)| (a - b).abs() > 1e-5)
        || a.scale
            .iter()
            .zip(b.scale)
            .any(|(a, b)| (a - b).abs() > 1e-5)
        || (1.0
            - a.rotation
                .iter()
                .zip(b.rotation)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                .abs())
            > 1e-6
}

#[test]
#[ignore = "requires MUDCRAB_SKYRIM_DATA and task-owned MUDCRAB_CREATURE_TEST_ROOT"]
fn retail_cow_and_chicken_idle_contract() {
    let data =
        PathBuf::from(std::env::var_os("MUDCRAB_SKYRIM_DATA").expect("source Data directory"));
    let scratch = PathBuf::from(
        std::env::var_os("MUDCRAB_CREATURE_TEST_ROOT").expect("task-owned scratch directory"),
    );
    fs::create_dir_all(&scratch).unwrap();
    // A native playtest can reuse an existing conversion with these two verified clips.
    // Require a new absolute directory so this opt-in export cannot overwrite a dataset.
    let clip_export = std::env::var_os("MUDCRAB_CREATURE_CLIP_EXPORT").map(PathBuf::from);
    if let Some(export) = &clip_export {
        assert!(export.is_absolute(), "clip export must be an absolute path");
        fs::create_dir(export).expect("clip export directory must not already exist");
    }
    let directory = tempfile::tempdir_in(scratch).unwrap();
    let staging = directory.path();
    let profiles = [
        ("cow", "meshes/actors/cow", "highlandcow"),
        ("chicken", "meshes/actors/ambient/chicken", "chicken"),
    ];
    let paths: BTreeSet<PathBuf> = profiles
        .iter()
        .flat_map(|(_, root, body)| {
            [
                format!("{root}/character assets/{body}.nif"),
                format!("{root}/character assets/skeleton.nif"),
                format!("{root}/character assets/skeleton.hkx"),
                format!("{root}/animations/mt_idle.hkx"),
            ]
            .map(PathBuf::from)
        })
        .collect();
    let mut extracted = BTreeSet::new();
    for archive in ["Skyrim - Animations.bsa", "Skyrim - Meshes0.bsa"] {
        for file in ArchiveExtractor::extract_paths(&data.join(archive), staging, &paths).unwrap() {
            extracted.insert(file.path);
        }
    }
    assert_eq!(extracted, paths, "every authored input must exist");

    for (name, root, body) in profiles {
        let assets = staging.join(root).join("character assets");
        let body_path = assets.join(format!("{body}.nif"));
        let skeleton_path = assets.join("skeleton.nif");
        let model = compose_body(
            &[BodyPart {
                path: &body_path,
                kind: BodyPartKind::Body,
                skin_textures: None,
            }],
            &skeleton_path,
        )
        .unwrap();
        let bones = &model.skeletal_meshes.first().expect("skinned body").bones;
        assert!(
            model
                .skeletal_meshes
                .iter()
                .all(|mesh| &mesh.bones == bones)
        );
        let skeleton = fs::read(assets.join("skeleton.hkx")).unwrap();
        let source = fs::read(staging.join(root).join("animations/mt_idle.hkx")).unwrap();
        let mut clip = decode_clip_with_skeleton(&source, &skeleton).unwrap();
        let output = staging.join(format!("{name}-idle.glb"));
        let binding = write_clip_glb(&mut clip, bones, "mt_idle", &output).unwrap();

        // Check against the standalone body route that streaming actually draws, rather than
        // assuming the single-part composition used to obtain the bone tree has the same paths.
        let body_output = staging.join(format!("{name}-body.glb"));
        MeshConverter::convert_nif_to_glb_with_skeleton(
            &body_path,
            &body_output,
            Some(&skeleton_path),
        )
        .unwrap();
        let body_paths: BTreeSet<_> = target_paths(&glb_json(&body_output))
            .into_values()
            .collect();
        let document = glb_json(&output);
        let clip_paths = target_paths(&document);
        let channels = document["animations"][0]["channels"].as_array().unwrap();
        assert!(!channels.is_empty());
        for channel in channels {
            let node = channel["target"]["node"].as_u64().unwrap() as usize;
            assert!(
                body_paths.contains(&clip_paths[&node]),
                "{name}: {:?}",
                clip_paths[&node]
            );
        }
        let moving = clip
            .tracks
            .iter()
            .filter(|track| {
                track.bone_name.is_some()
                    && track
                        .samples
                        .iter()
                        .any(|sample| differs(&track.samples[0], sample))
            })
            .count();
        println!(
            "{}",
            json!({
                "species": name, "original_skeleton_name": clip.original_skeleton_name,
                "duration_seconds": clip.duration_seconds, "frames": clip.frame_count,
                "tracks": clip.tracks.len(), "bound": binding.bound(),
                "aliased": binding.aliased, "skipped": binding.skipped,
                "moving_bound_tracks": moving, "channels": channels.len(),
                "all_channel_paths_match_standalone_body": true,
            })
        );
        assert!(
            moving > 0,
            "{name}: the selected idle must move a bound body bone"
        );
        if let Some(export) = &clip_export {
            let destination = export.join(root).join("animations/mt_idle.glb");
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(&output, &destination).unwrap();
            fs::write(
                export.join(format!("{name}-receipt.json")),
                serde_json::to_vec_pretty(&json!({
                    "body_source": format!("{root}/character assets/{body}.nif"),
                    "clip_scene": format!("{root}/animations/mt_idle.glb"),
                    "clip_sha256": converter::cache::hash_file(&destination).unwrap(),
                    "bound": binding.bound(), "moving_bound_tracks": moving,
                    "all_channel_paths_match_standalone_body": true,
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }
}
