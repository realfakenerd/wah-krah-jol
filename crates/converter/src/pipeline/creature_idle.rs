//! Optional idle-only outputs for explicitly verified standalone creature bodies.

use super::{PipelineReport, derived_input_hashes, validate_animation_output_parent};
use crate::{
    animation,
    cache::{CREATURE_IDLE_RECIPE, ConversionManifest, DerivedOutput, hash_bytes},
    mesh,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use shared::creature_idle::CREATURE_IDLES;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn remove(output: &Path) -> Result<()> {
    match fs::remove_file(output) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).wrap_err_with(|| format!("failed to remove {}", output.display()))
        }
        _ => Ok(()),
    }
}

/// The runtime addresses nodes by the complete named path below the basis wrapper.
fn paths(document: &gltf::Document) -> BTreeMap<usize, Vec<String>> {
    let mut found = BTreeMap::new();
    let Some(scene) = document
        .default_scene()
        .or_else(|| document.scenes().next())
    else {
        return found;
    };
    let mut pending: Vec<_> = scene.nodes().map(|node| (node, Vec::new())).collect();
    while let Some((node, mut path)) = pending.pop() {
        let Some(name) = node.name() else { continue };
        if name == "Creation-to-glTF basis" {
            path.clear();
        }
        path.push(name.to_owned());
        if path[0] == "Creation-to-glTF basis" {
            found.insert(node.index(), path.clone());
        }
        pending.extend(node.children().map(|child| (child, path.clone())));
    }
    found
}

pub(super) fn validate_targets(clip: &Path, body: &Path) -> Result<()> {
    let clip = gltf::Gltf::open(clip)?;
    let body = gltf::Gltf::open(body)?;
    let clip_paths = paths(&clip.document);
    let body_paths: BTreeSet<_> = paths(&body.document).into_values().collect();
    let mut channels = 0;
    for animation in clip.animations() {
        for channel in animation.channels() {
            let path = clip_paths.get(&channel.target().node().index());
            ensure!(
                path.is_some_and(|path| body_paths.contains(path)),
                "creature idle target {path:?} does not exist in the converted body"
            );
            channels += 1;
        }
    }
    ensure!(channels > 0, "creature idle has no animation channels");
    Ok(())
}

pub(super) fn convert(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    fail_fast: bool,
    report: &mut PipelineReport,
    manifest: &mut ConversionManifest,
) -> Result<()> {
    // Preflight all outputs before invalidating any proof or unlinking any symlink.
    for profile in CREATURE_IDLES {
        let output = staging.join(profile.clip_scene);
        validate_animation_output_parent(&output)?;
        match fs::symlink_metadata(&output) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_dir(),
                "refusing directory at creature idle output {}",
                output.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let winning = |key: &str| {
        let logical = staging.join("vfs").join(key);
        sources.get(&logical).cloned().unwrap_or(logical)
    };
    let relevance = CREATURE_IDLES.map(|profile| {
        staging.join(profile.clip_scene).is_file()
            || manifest.derived_outputs.contains_key(profile.clip_scene)
            || manifest.failures.contains_key(profile.clip_scene)
            || profile.inputs().iter().any(|key| winning(key).is_file())
    });
    for profile in CREATURE_IDLES {
        let output = staging.join(profile.clip_scene);
        match fs::symlink_metadata(&output) {
            Ok(metadata) if !metadata.file_type().is_file() => remove(&output)?,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        manifest.derived_outputs.remove(profile.clip_scene);
        manifest.failures.remove(profile.clip_scene);
    }

    for (profile, relevant) in CREATURE_IDLES.into_iter().zip(relevance) {
        let output = staging.join(profile.clip_scene);
        let converted = (|| -> Result<_> {
            for input in profile.inputs() {
                ensure!(
                    winning(input).is_file(),
                    "missing creature idle input {input}"
                );
            }
            let inputs = derived_input_hashes(
                staging,
                sources,
                profile.inputs().map(|key| staging.join("vfs").join(key)),
            )?;
            let model = mesh::build_skinned_model(
                &winning(profile.body_source),
                &winning(profile.skeleton_nif),
            )?;
            let bones = &model
                .skeletal_meshes
                .first()
                .ok_or_else(|| eyre!("creature body has no skeletal mesh"))?
                .bones;
            let mut clip = animation::decode_clip_with_skeleton(
                &fs::read(winning(profile.clip_source))?,
                &fs::read(winning(profile.skeleton_hkx))?,
            )?;
            let candidate = tempfile::tempdir()?;
            let candidate_path = candidate.path().join("idle.glb");
            let binding = animation::write_clip_glb(&mut clip, bones, "mt_idle", &candidate_path)?;
            // The ordinary mesh resolver can choose a different skeleton for a modded body.
            // Do not publish an idle unless its paths fit the body that actually converted.
            validate_targets(&candidate_path, &staging.join(profile.body_scene))?;
            let bytes = fs::read(candidate_path)?;
            if !fs::read(&output).is_ok_and(|existing| existing == bytes) {
                mesh::write_glb_atomic(&output, &bytes)?;
            }
            Ok((bytes, inputs, binding))
        })();
        match converted {
            Ok((bytes, inputs, binding)) => {
                manifest.derived_outputs.insert(
                    profile.clip_scene.to_owned(),
                    DerivedOutput {
                        recipe: CREATURE_IDLE_RECIPE.to_owned(),
                        inputs,
                        output_size: bytes.len() as u64,
                        output_hash: hash_bytes(&bytes),
                    },
                );
                report.artifacts.push(profile.clip_scene.into());
                log::info!(
                    "{}: {} bound idle tracks, {} skipped",
                    profile.clip_scene,
                    binding.bound(),
                    binding.skipped.len()
                );
            }
            Err(error) => {
                remove(&output)?;
                if relevant {
                    let message =
                        format!("{}: skipping creature idle: {error:#}", profile.clip_scene);
                    if fail_fast {
                        for profile in CREATURE_IDLES {
                            manifest.derived_outputs.remove(profile.clip_scene);
                            remove(&staging.join(profile.clip_scene))?;
                        }
                        return Err(eyre!(message));
                    }
                    log::warn!("{message}");
                    report.notices.push(message);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stale(staging: &Path, manifest: &mut ConversionManifest) {
        for profile in CREATURE_IDLES {
            let output = staging.join(profile.clip_scene);
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            fs::write(output, b"stale clip").unwrap();
            manifest
                .derived_outputs
                .insert(profile.clip_scene.into(), DerivedOutput::default());
            manifest
                .failures
                .insert(profile.clip_scene.into(), "old failure".into());
        }
    }

    #[test]
    fn creature_idle_empty_inputs_are_silent() {
        let directory = tempfile::tempdir().unwrap();
        let mut report = PipelineReport::default();
        convert(
            directory.path(),
            &BTreeMap::new(),
            true,
            &mut report,
            &mut ConversionManifest::default(),
        )
        .unwrap();
        assert!(report.notices.is_empty());
        assert!(report.artifacts.is_empty());
    }

    #[test]
    fn creature_idle_failure_cleans_all_stale_outputs_and_markers() {
        for fail_fast in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut manifest = ConversionManifest::default();
            stale(directory.path(), &mut manifest);
            let result = convert(
                directory.path(),
                &BTreeMap::new(),
                fail_fast,
                &mut PipelineReport::default(),
                &mut manifest,
            );
            assert_eq!(result.is_err(), fail_fast);
            for profile in CREATURE_IDLES {
                assert!(!directory.path().join(profile.clip_scene).exists());
                assert!(!manifest.derived_outputs.contains_key(profile.clip_scene));
                assert!(!manifest.failures.contains_key(profile.clip_scene));
            }
        }
    }

    #[test]
    fn creature_idle_directory_preflight_preserves_outputs_and_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = ConversionManifest::default();
        stale(directory.path(), &mut manifest);
        let blocked = directory.path().join(CREATURE_IDLES[1].clip_scene);
        fs::remove_file(&blocked).unwrap();
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("keep"), b"owned by caller").unwrap();
        assert!(
            convert(
                directory.path(),
                &BTreeMap::new(),
                false,
                &mut PipelineReport::default(),
                &mut manifest
            )
            .is_err()
        );
        assert_eq!(
            fs::read(directory.path().join(CREATURE_IDLES[0].clip_scene)).unwrap(),
            b"stale clip"
        );
        assert_eq!(fs::read(blocked.join("keep")).unwrap(), b"owned by caller");
        assert_eq!(manifest.derived_outputs.len(), 2);
        assert_eq!(manifest.failures.len(), 2);
    }

    #[test]
    fn creature_idle_proof_requires_its_complete_raw_input_contract() {
        for profile in CREATURE_IDLES {
            let mut proof = DerivedOutput {
                recipe: CREATURE_IDLE_RECIPE.into(),
                inputs: profile
                    .inputs()
                    .map(|key| (key.into(), hash_bytes(b"input")))
                    .into(),
                output_size: 1,
                output_hash: hash_bytes(b"x"),
            };
            assert!(crate::metadata::supported_derived_proof(
                profile.clip_scene,
                &proof
            ));
            assert!(!crate::metadata::supported_derived_proof(
                "meshes/actors/custom/animations/mt_idle.glb",
                &proof
            ));
            proof.inputs.remove(profile.skeleton_hkx);
            assert!(!crate::metadata::supported_derived_proof(
                profile.clip_scene,
                &proof
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn creature_idle_output_symlink_is_unlinked_without_touching_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("keep");
        fs::write(&target, b"keep").unwrap();
        let output = directory.path().join(CREATURE_IDLES[0].clip_scene);
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &output).unwrap();
        convert(
            directory.path(),
            &BTreeMap::new(),
            false,
            &mut PipelineReport::default(),
            &mut ConversionManifest::default(),
        )
        .unwrap();
        assert!(!output.exists());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    #[ignore = "requires MUDCRAB_SKYRIM_DATA and task-owned MUDCRAB_CREATURE_TEST_ROOT"]
    fn retail_creature_idles_publish_validate_and_clean_up() {
        let data =
            PathBuf::from(std::env::var_os("MUDCRAB_SKYRIM_DATA").expect("source Data directory"));
        let scratch = PathBuf::from(
            std::env::var_os("MUDCRAB_CREATURE_TEST_ROOT").expect("task-owned scratch"),
        );
        fs::create_dir_all(&scratch).unwrap();
        let directory = tempfile::tempdir_in(scratch).unwrap();
        let staging = directory.path();
        let vfs = staging.join("vfs");
        let paths = CREATURE_IDLES
            .into_iter()
            .flat_map(|profile| profile.inputs().map(PathBuf::from))
            .collect();
        let mut extracted = BTreeSet::new();
        for archive in ["Skyrim - Animations.bsa", "Skyrim - Meshes0.bsa"] {
            for file in
                crate::archive::ArchiveExtractor::extract_paths(&data.join(archive), &vfs, &paths)
                    .unwrap()
            {
                extracted.insert(file.path);
            }
        }
        assert_eq!(paths, extracted);
        for profile in CREATURE_IDLES {
            let output = staging.join(profile.body_scene);
            fs::create_dir_all(output.parent().unwrap()).unwrap();
            crate::mesh::MeshConverter::convert_nif_to_glb_with_skeleton(
                &vfs.join(profile.body_source),
                &output,
                Some(&vfs.join(profile.skeleton_nif)),
            )
            .unwrap();
        }
        let mut manifest = ConversionManifest::default();
        let mut report = PipelineReport::default();
        convert(staging, &BTreeMap::new(), false, &mut report, &mut manifest).unwrap();
        assert!(report.notices.is_empty(), "{:?}", report.notices);
        assert_eq!(manifest.derived_outputs.len(), 2);
        let times = CREATURE_IDLES.map(|profile| {
            fs::metadata(staging.join(profile.clip_scene))
                .unwrap()
                .modified()
                .unwrap()
        });
        for profile in CREATURE_IDLES {
            let proof = &manifest.derived_outputs[profile.clip_scene];
            assert!(crate::metadata::supported_derived_proof(
                profile.clip_scene,
                proof
            ));
            for input in profile.inputs() {
                assert_eq!(
                    proof.inputs[input],
                    crate::cache::hash_file(&vfs.join(input)).unwrap()
                );
            }
            assert_eq!(
                proof.output_hash,
                crate::cache::hash_file(&staging.join(profile.clip_scene)).unwrap()
            );
        }
        convert(
            staging,
            &BTreeMap::new(),
            false,
            &mut PipelineReport::default(),
            &mut manifest,
        )
        .unwrap();
        assert_eq!(
            times,
            CREATURE_IDLES.map(|profile| fs::metadata(staging.join(profile.clip_scene))
                .unwrap()
                .modified()
                .unwrap())
        );

        // A different converted hierarchy must invalidate a previously valid idle.
        fs::copy(
            staging.join(CREATURE_IDLES[1].body_scene),
            staging.join(CREATURE_IDLES[0].body_scene),
        )
        .unwrap();
        let mut report = PipelineReport::default();
        convert(staging, &BTreeMap::new(), false, &mut report, &mut manifest).unwrap();
        assert!(!staging.join(CREATURE_IDLES[0].clip_scene).exists());
        assert!(
            !manifest
                .derived_outputs
                .contains_key(CREATURE_IDLES[0].clip_scene)
        );
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("does not exist in the converted body"))
        );

        // Fail-fast still removes the other profile's retained output and proof.
        assert!(
            convert(
                staging,
                &BTreeMap::new(),
                true,
                &mut PipelineReport::default(),
                &mut manifest
            )
            .is_err()
        );
        assert!(manifest.derived_outputs.is_empty());
        assert!(
            CREATURE_IDLES
                .iter()
                .all(|profile| !staging.join(profile.clip_scene).exists())
        );
    }
}
