//! Standing idle projection for effective actor race/sex/body contracts.
//! Discovery uses winning RACE -> project -> character -> HKX rig links. The
//! body remains the ordinary mesh export; publication requires its complete paths.

use super::{PipelineReport, validate_animation_output_parent};
use crate::{
    animation::{self, authoring},
    cache::{ConversionManifest, DerivedOutput, hash_bytes},
    mesh,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use rusqlite::Connection;
use shared::actor_idle_catalog::{
    ActorIdleBinding, ActorIdleCatalog, PATH, UnresolvedIdle, VERSION,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub(crate) const RECIPE: &str = "authored-actor-idle-v2";
pub(crate) const CATALOG_RECIPE: &str = "actor-idle-catalog-v1";
pub(crate) const CONTEXT: &str = "recipes/actor-idle-catalog-v1/context.json";

const TREE_PREFIX: &str = "recipes/authored-actor-idle-v2/tree/";

fn tree_context(bones: &project_wormhole_nif::model::all::BoneTree) -> (String, String) {
    let nodes: Vec<_> = bones
        .bones
        .iter()
        .map(|(name, bone)| {
            let mut children: Vec<_> = bone.children.iter().collect();
            children.sort();
            serde_json::json!({"name":name,"children":children,
            "translation":bone.transform.get_translation().map(|value| value.0.to_array()),
            "rotation":bone.transform.get_rotation().map(|value| value.0.to_cols_array()),
            "scale":bone.transform.get_scale()})
        })
        .collect();
    let hash = hash_bytes(&serde_json::to_vec(&nodes).expect("finite body transforms"));
    (format!("{TREE_PREFIX}{hash}.json"), hash)
}

fn output_key(inputs: &BTreeMap<String, String>) -> String {
    // Geometry and texture variants can share a clip only when every named bone,
    // reference transform and parent path agrees. The retained proof still names
    // the first actual body input; the catalog context authenticates every binding.
    let identity: BTreeMap<_, _> = inputs
        .iter()
        .filter(|(key, _)| !key.ends_with(".glb"))
        .collect();
    format!(
        "meshes/actor-idles/clips/{}.glb",
        hash_bytes(&serde_json::to_vec(&identity).expect("string inputs"))
    )
}

pub(crate) fn supported_proof(path: &str, proof: &DerivedOutput) -> bool {
    if path == PATH && proof.recipe == CATALOG_RECIPE {
        return proof.inputs.len() == 1 && proof.inputs.contains_key(CONTEXT);
    }
    proof.recipe == RECIPE
        && proof.inputs.len() == 4
        && proof
            .inputs
            .keys()
            .filter(|key| shared::actor_idle_catalog::canonical_mesh_key(key, ".glb"))
            .count()
            == 1
        && proof
            .inputs
            .keys()
            .filter(|key| shared::actor_idle_catalog::canonical_mesh_key(key, ".hkx"))
            .count()
            == 2
        && proof
            .inputs
            .iter()
            .filter(|(key, value)| {
                key.strip_prefix(TREE_PREFIX)
                    .and_then(|suffix| suffix.strip_suffix(".json"))
                    .is_some_and(|hash| hash == value.as_str())
            })
            .count()
            == 1
        && output_key(&proof.inputs) == path
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).wrap_err_with(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn preflight(staging: &Path, path: &Path) -> Result<()> {
    validate_animation_output_parent(path)?;
    for parent in path
        .ancestors()
        .skip(1)
        .take_while(|parent| *parent != staging)
    {
        match fs::symlink_metadata(parent) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "idle output parent is not an owned directory: {}",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_dir(),
            "refusing directory at idle output {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ContractSet {
    contracts: Vec<Contract>,
    rejected: Vec<UnresolvedIdle>,
    /// Bodies whose RACE project is the shared character project. They stay on the human
    /// and child paths (#470) and are counted here, not silently dropped from the denominator.
    character_project_bodies: usize,
}

#[derive(Debug)]
struct Contract {
    race: u32,
    sex: String,
    body: String,
    skeleton: Option<String>,
    project: Option<String>,
}

/// Effective race/sex/body contracts, one per canonical body key.
///
/// `actor_model_parts.model_path` keeps authored case and separators, so SQL `DISTINCT`
/// alone can return the same body twice. A path that cannot name a mesh is reported,
/// never fatal: one malformed modded record must not stop the whole conversion.
fn contracts(connection: &Connection) -> Result<ContractSet> {
    let mut races = BTreeMap::new();
    let mut statement =
        connection.prepare("SELECT form_id,data FROM records WHERE record_type='RACE'")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (id, bytes) = row?;
        let race =
            rkyv::from_bytes::<shared::records::ArchivedRecordData, rkyv::rancor::Error>(&bytes)
                .map_err(|error| eyre!("RACE {id:08X}: {error}"))?;
        races.insert(id, authoring::read_race_authoring(&race.subrecords));
    }
    let mut statement = connection.prepare("SELECT DISTINCT a.race_id,a.sex,p.model_path FROM actor_models a JOIN actor_model_parts p ON p.npc_id=a.id WHERE p.slot='body' ORDER BY a.race_id,a.sex,p.model_path")?;
    let mut set = ContractSet::default();
    let mut seen = BTreeSet::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (race, sex, authored) = row?;
        ensure!(
            matches!(sex.as_str(), "male" | "female"),
            "invalid actor sex {sex}"
        );
        let body = match shared::actor_idle_catalog::canonical_body_path(&authored, "nif") {
            Ok(body) => body,
            Err(error) => {
                set.rejected.push(UnresolvedIdle {
                    race_id: race,
                    sex,
                    body_scene: authored,
                    reason: format!("invalid authored body path: {error:#}"),
                });
                continue;
            }
        };
        if !seen.insert((race, sex.clone(), body.clone())) {
            continue;
        }
        let authoring = match races.get(&race) {
            Some(Ok(authoring)) => Some(authoring),
            Some(Err(error)) => {
                set.rejected.push(UnresolvedIdle {
                    race_id: race,
                    sex,
                    body_scene: Path::new(&body)
                        .with_extension("glb")
                        .to_string_lossy()
                        .into_owned(),
                    reason: format!("invalid effective RACE authoring: {error:#}"),
                });
                continue;
            }
            None => None,
        };
        let (skeleton, project) = authoring
            .map(|row| {
                if sex == "female" {
                    (row.female_skeleton.clone(), row.female_project.clone())
                } else {
                    (row.male_skeleton.clone(), row.male_project.clone())
                }
            })
            .unwrap_or_default();
        // Human gait and child-body composition belong to their existing contracts (#470).
        if project
            .as_deref()
            .is_some_and(|path| path.starts_with("meshes/actors/character/"))
        {
            set.character_project_bodies += 1;
            continue;
        }
        set.contracts.push(Contract {
            race,
            sex,
            body,
            skeleton,
            project,
        });
    }
    Ok(set)
}

fn proof(recipe: &str, inputs: BTreeMap<String, String>, bytes: &[u8]) -> DerivedOutput {
    DerivedOutput {
        recipe: recipe.into(),
        inputs,
        output_size: bytes.len() as u64,
        output_hash: hash_bytes(bytes),
    }
}

fn differs(a: &animation::TransformSample, b: &animation::TransformSample) -> bool {
    a.translation
        .iter()
        .zip(b.translation)
        .chain(a.scale.iter().zip(b.scale))
        .any(|(a, b)| (a - b).abs() > 1e-5)
        || 1.0
            - a.rotation
                .iter()
                .zip(b.rotation)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                .abs()
            > 1e-6
}

fn moving(track: &animation::TransformTrack) -> bool {
    track.samples.first().is_some_and(|first| {
        track
            .samples
            .iter()
            .skip(1)
            .any(|sample| differs(first, sample))
    })
}

#[derive(Clone)]
struct ProjectPlan {
    rig: String,
    default: String,
    clips: BTreeSet<String>,
    idles: BTreeSet<String>,
    metadata: BTreeMap<String, String>,
    issues: Vec<String>,
}

fn plan_project(
    project: &str,
    mut read: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<ProjectPlan> {
    let mut metadata = BTreeMap::new();
    let mut read = |key: &str| {
        let bytes = read(key)?;
        metadata.insert(key.to_owned(), hash_bytes(&bytes));
        Ok(bytes)
    };
    let characters = authoring::discover_project(project, &mut read)?;
    ensure!(
        characters.len() == 1,
        "project has {} characters; selection is ambiguous",
        characters.len()
    );
    let character = &characters[0];
    let mut found = authoring::idle_sources(project, character, &mut read);
    let default = if let Some(source) = animation::idle_defaults::source(project) {
        authoring::resolve_reference(project, source)?
    } else {
        ensure!(
            found.clips.len() == 1,
            "authored project has no unambiguous default idle: {project}"
        );
        found.clips.iter().next().unwrap().clone()
    };
    ensure!(
        character.animations.contains(&default),
        "default idle is not declared by the authored character: {default}"
    );
    found.clips.insert(default.clone());
    Ok(ProjectPlan {
        rig: character.rig.clone(),
        default,
        clips: character.animations.iter().cloned().collect(),
        idles: found.clips,
        metadata,
        issues: found.issues,
    })
}

fn publish_clip(
    source: &str,
    rig: &[u8],
    bytes: &[u8],
    bones: &project_wormhole_nif::model::all::BoneTree,
    candidate: &Path,
    body: &Path,
    output: &Path,
) -> Result<shared::actor_idle_catalog::ActorIdleClip> {
    let mut clip = animation::decode_clip_with_skeleton(bytes, rig).map_err(|error| {
        if format!("{error:#}").contains("clip names skeleton \"PairedRoot\"") {
            eyre!("not_standalone: paired source requires its authored paired rig")
        } else {
            error
        }
    })?;
    ensure!(
        clip.blend_hint == 0,
        "not_standalone: additive graph layer requires an explicit blended base pose"
    );
    let had_motion = clip.tracks.iter().any(moving);
    let binding = animation::write_clip_glb(&mut clip, bones, "mt_idle", candidate)?;
    let motion_frame = clip
        .tracks
        .iter()
        .filter(|track| track.bone_name.is_some())
        .filter_map(|track| {
            track
                .samples
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, sample)| differs(&track.samples[0], sample))
                .map(|(index, _)| index)
        })
        .min()
        .map(|frame| frame as u32);
    let moving_bound_tracks = clip
        .tracks
        .iter()
        .filter(|track| track.bone_name.is_some() && moving(track))
        .count();
    ensure!(
        !had_motion || moving_bound_tracks > 0,
        "not_applicable: all moving tracks in {source} address bones absent from this body"
    );
    super::creature_idle::validate_targets(candidate, body)?;
    mesh::write_glb_atomic(output, &fs::read(candidate)?)?;
    Ok(shared::actor_idle_catalog::ActorIdleClip {
        scene: String::new(),
        frame_count: clip.frame_count,
        frames_per_second: clip.frames_per_second,
        motion_frame,
        bound_tracks: binding.bound(),
        moving_bound_tracks,
        skipped_tracks: binding.skipped,
    })
}

pub(super) fn convert(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    fail_fast: bool,
    report: &mut PipelineReport,
    manifest: &mut ConversionManifest,
) -> Result<()> {
    use shared::actor_idle_catalog::{ActorIdleClip, IdleClipIssue};
    let catalog_path = staging.join(PATH);
    let old: Vec<_> = manifest
        .derived_outputs
        .iter()
        .filter(|(key, proof)| {
            key.as_str() == PATH
                || (key.starts_with("meshes/actor-idles/clips/") && proof.recipe == RECIPE)
        })
        .map(|(key, _)| key.clone())
        .collect();
    preflight(staging, &catalog_path)?;
    for key in &old {
        ensure!(
            key == PATH || shared::actor_idle_catalog::canonical_mesh_key(key, ".glb"),
            "invalid owned idle output {key}"
        );
        preflight(staging, &staging.join(key))?;
    }
    let database = staging.join("skyrim_world.db");
    let set = if database.is_file() {
        let connection = Connection::open(&database)?;
        let actor_tables: u32 = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('actor_models','actor_model_parts')",[],|row| row.get(0))?;
        if actor_tables == 2 {
            authoring::project_races(&connection)?;
            contracts(&connection)?
        } else {
            Default::default()
        }
    } else {
        Default::default()
    };
    let ContractSet {
        contracts: rows,
        rejected,
        character_project_bodies,
    } = set;
    if fail_fast && let Some(issue) = rejected.first() {
        return Err(eyre!(
            "RACE {:08X} {} {}: {}",
            issue.race_id,
            issue.sex,
            issue.body_scene,
            issue.reason
        ));
    }
    let winning = |key: &str| {
        let logical = staging.join("vfs").join(key);
        sources.get(&logical).cloned().unwrap_or(logical)
    };
    let mut projects: BTreeMap<String, std::result::Result<ProjectPlan, String>> = BTreeMap::new();
    let mut prepared = Vec::new();
    for row in rows {
        // `body` is already canonical. Do not percent-decode its literal '%' again.
        let body_scene = Path::new(&row.body)
            .with_extension("glb")
            .to_string_lossy()
            .into_owned();
        let discovered = (|| -> Result<_> {
            let project = row
                .project
                .as_deref()
                .ok_or_else(|| eyre!("missing effective RACE project"))?;
            ensure!(
                row.skeleton.is_some(),
                "missing effective RACE skeleton NIF"
            );
            let plan = projects
                .entry(project.into())
                .or_insert_with(|| {
                    plan_project(project, |key| Ok(fs::read(winning(key))?))
                        .map_err(|error| format!("{error:#}"))
                })
                .as_ref()
                .map_err(|error| eyre!(error.clone()))?
                .clone();
            let bones = animation::glb::body_bones(&staging.join(&body_scene))?;
            let tree = tree_context(&bones);
            let body_hash = crate::cache::hash_file(&staging.join(&body_scene))?;
            let rig_hash = crate::cache::hash_file(&winning(&plan.rig))?;
            let mut inputs = BTreeMap::new();
            for source in &plan.clips {
                let value = crate::cache::hash_file(&winning(source))
                    .map(|hash| {
                        BTreeMap::from([
                            (source.clone(), hash),
                            (plan.rig.clone(), rig_hash.clone()),
                            (body_scene.clone(), body_hash.clone()),
                            tree.clone(),
                        ])
                    })
                    .map_err(|error| format!("{error:#}"));
                inputs.insert(source.clone(), value);
            }
            Ok((plan, inputs, bones))
        })();
        if let Ok((_, inputs, _)) = &discovered {
            for input in inputs.values().filter_map(|value| value.as_ref().ok()) {
                preflight(staging, &staging.join(output_key(input)))?;
            }
        }
        prepared.push((row, body_scene, discovered));
    }
    // Check every destination before invalidating any producer proof or runtime catalog.
    for key in &old {
        remove(&staging.join(key))?;
        manifest.derived_outputs.remove(key);
    }
    remove(&catalog_path)?;
    let mut catalog = ActorIdleCatalog {
        version: VERSION,
        unresolved: rejected,
        ..Default::default()
    };
    let mut published: BTreeMap<String, ActorIdleClip> = BTreeMap::new();
    let mut context = Vec::new();
    for (row, body_scene, discovered) in prepared {
        let attempt = (|| -> Result<ActorIdleBinding> {
            let (plan, inputs, bones) = discovered?;
            catalog.discovery_issues.extend(plan.issues.iter().cloned());
            {
                use shared::actor_idle_catalog::canonical_mesh_key as key;
                // Reject identity keys before publishing any clip for this body.
                ensure!(
                    key(&body_scene, ".glb")
                        && row.project.as_deref().is_some_and(|path| key(path, ".hkx"))
                        && row
                            .skeleton
                            .as_deref()
                            .is_some_and(|path| key(path, ".nif"))
                        && key(&plan.rig, ".hkx")
                        && key(&plan.default, ".hkx"),
                    "invalid actor idle identity key"
                );
            }
            let rig = fs::read(winning(&plan.rig))?;
            let candidate = tempfile::tempdir()?;
            let candidate_path = candidate.path().join("clip.glb");
            let mut clips = BTreeMap::new();
            // Establish the default first. A failed default never leaves a partial actor binding.
            let order = std::iter::once(&plan.default)
                .chain(plan.clips.iter().filter(|source| **source != plan.default));
            for source in order {
                let attempt = (|| -> Result<ActorIdleClip> {
                    let inputs = inputs[source]
                        .as_ref()
                        .map_err(|message| eyre!(message.clone()))?;
                    ensure!(
                        inputs.len() == 4,
                        "pose requires body, rig, source and tree context inputs"
                    );
                    let key = output_key(inputs);
                    if let Some(clip) = published.get(&key) {
                        return Ok(clip.clone());
                    }
                    let bytes = fs::read(winning(source))?;
                    let mut clip = publish_clip(
                        source,
                        &rig,
                        &bytes,
                        &bones,
                        &candidate_path,
                        &staging.join(&body_scene),
                        &staging.join(&key),
                    )?;
                    clip.scene = key.clone();
                    if let Err(message) = shared::actor_idle_catalog::validate_clip(source, &clip) {
                        // Never retain an output whose catalog entry would be rejected.
                        remove(&staging.join(&key))?;
                        return Err(eyre!("invalid published clip {source}: {message}"));
                    }
                    let bytes = fs::read(staging.join(&key))?;
                    manifest
                        .derived_outputs
                        .insert(key.clone(), proof(RECIPE, inputs.clone(), &bytes));
                    report.artifacts.push(key.clone().into());
                    published.insert(key, clip.clone());
                    Ok(clip)
                })();
                match attempt {
                    Ok(clip) => {
                        clips.insert(source.clone(), clip);
                    }
                    Err(error) if source == &plan.default => return Err(error),
                    Err(error) => {
                        let reason = format!("{error:#}");
                        if fail_fast
                            && !reason.starts_with("not_applicable:")
                            && !reason.starts_with("not_standalone:")
                        {
                            return Err(error);
                        }
                        catalog.clip_issues.push(IdleClipIssue {
                            race_id: row.race,
                            sex: row.sex.clone(),
                            body_scene: body_scene.clone(),
                            source: source.clone(),
                            not_applicable: reason.starts_with("not_applicable:")
                                || reason.starts_with("not_standalone:"),
                            identified_as_idle: plan.idles.contains(source),
                            reason,
                        });
                    }
                }
            }
            let default = &clips[&plan.default];
            let binding = ActorIdleBinding {
                race_id: row.race,
                sex: row.sex.clone(),
                body_scene: body_scene.clone(),
                project: row.project.clone().unwrap(),
                skeleton: plan.rig,
                race_skeleton_nif: row.skeleton.clone().unwrap(),
                source: plan.default,
                scene: default.scene.clone(),
                bound_tracks: default.bound_tracks,
                moving_bound_tracks: default.moving_bound_tracks,
                skipped_tracks: default.skipped_tracks.clone(),
                clips: clips.clone(),
            };
            // Check this body alone: one malformed modded key becomes an unresolved
            // body, not an invalid catalog that would abort the whole conversion.
            ActorIdleCatalog {
                version: VERSION,
                bindings: vec![binding.clone()],
                ..Default::default()
            }
            .validate()
            .map_err(|message| eyre!(message))?;
            context.push(serde_json::json!({"race":row.race,"sex":row.sex,"body":row.body,"skeleton":row.skeleton,"project":row.project,"metadata":plan.metadata,"inputs":inputs,"clips":clips}));
            Ok(binding)
        })();
        match attempt {
            Ok(binding) => catalog.bindings.push(binding),
            Err(error) => {
                let reason = format!("{error:#}");
                // A scene without skinning or compatible named targets cannot be
                // evaluated by this pose pass. This does not establish whether the
                // source is intentionally static or its converted hierarchy is missing.
                let no_targets = reason == "clip has no bound transform tracks"
                    && gltf::Gltf::open(staging.join(&body_scene))
                        .is_ok_and(|body| body.skins().next().is_none());
                if fail_fast && !no_targets {
                    for key in published.keys() {
                        remove(&staging.join(key))?;
                        manifest.derived_outputs.remove(key);
                    }
                    return Err(eyre!(
                        "RACE {:08X} {} {body_scene}: {reason}",
                        row.race,
                        row.sex
                    ));
                }
                let issue = UnresolvedIdle {
                    race_id: row.race,
                    sex: row.sex,
                    body_scene,
                    reason,
                };
                if no_targets {
                    catalog.excluded.push(issue);
                } else {
                    catalog.unresolved.push(issue);
                }
            }
        }
    }
    if catalog.bindings.is_empty() && catalog.unresolved.is_empty() && catalog.excluded.is_empty() {
        return Ok(());
    }
    catalog.discovery_issues.sort();
    catalog.discovery_issues.dedup();
    catalog.validate().map_err(|message| eyre!(message))?;
    let bytes = serde_json::to_vec_pretty(&catalog)?;
    let context = hash_bytes(&serde_json::to_vec(&(
        VERSION,
        context,
        &catalog.unresolved,
        &catalog.excluded,
        &catalog.clip_issues,
        &catalog.discovery_issues,
    ))?);
    mesh::write_glb_atomic(&catalog_path, &bytes)?;
    manifest.derived_outputs.insert(
        PATH.into(),
        proof(
            CATALOG_RECIPE,
            BTreeMap::from([(CONTEXT.into(), context)]),
            &bytes,
        ),
    );
    report.artifacts.push(PATH.into());
    let message = format!(
        "actor idle coverage: {} bound bodies, {} clips, {} unresolved bodies, {} scenes without compatible targets, {} clip issues, {} discovery issues, {character_project_bodies} character-project bodies left to the human/child paths; see {PATH}",
        catalog.bindings.len(),
        published.len(),
        catalog.unresolved.len(),
        catalog.excluded.len(),
        catalog.clip_issues.len(),
        catalog.discovery_issues.len()
    );
    log::info!("{message}");
    if !catalog.unresolved.is_empty()
        || !catalog.clip_issues.is_empty()
        || !catalog.discovery_issues.is_empty()
    {
        report.notices.push(message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_path::{AssetKind, canonical_asset_path};
    use rusqlite::OpenFlags;

    #[test]
    fn generic_idle_proofs_bind_the_output_identity_to_body_rig_and_clip() {
        let inputs = BTreeMap::from([
            ("meshes/body.glb".into(), "a".repeat(64)),
            ("meshes/rig.hkx".into(), "c".repeat(64)),
            ("meshes/idle.hkx".into(), "d".repeat(64)),
            (
                format!("{TREE_PREFIX}{}.json", "e".repeat(64)),
                "e".repeat(64),
            ),
        ]);
        let key = output_key(&inputs);
        let mut variant = inputs.clone();
        variant.remove("meshes/body.glb");
        variant.insert("meshes/variant.glb".into(), "f".repeat(64));
        assert_eq!(
            output_key(&variant),
            key,
            "identical bone trees share pose bytes across geometry variants"
        );
        let mut value = proof(RECIPE, inputs, b"clip");
        assert!(crate::metadata::supported_derived_proof(&key, &value));
        assert!(!crate::metadata::supported_derived_proof(
            "meshes/actor-idles/clips/other.glb",
            &value
        ));
        value
            .inputs
            .insert("meshes/../escape.nif".into(), "a".repeat(64));
        assert!(!crate::metadata::supported_derived_proof(&key, &value));
    }

    #[test]
    fn tree_context_tracks_parent_paths_and_reference_transforms() {
        use project_wormhole_nif::model::all::{BoneData, BoneTransform, BoneTree};
        let mut bones = BoneTree::new();
        bones
            .add_bone(
                "Root",
                BoneData {
                    transform: BoneTransform::default(),
                    children: std::collections::HashSet::from(["Child".into()]),
                },
            )
            .unwrap();
        bones
            .add_bone(
                "Child",
                BoneData {
                    transform: BoneTransform::default(),
                    children: Default::default(),
                },
            )
            .unwrap();
        let original = tree_context(&bones);
        assert_eq!(tree_context(&bones.clone()), original);
        bones.bones.get_mut("Root").unwrap().children.clear();
        assert_ne!(
            tree_context(&bones),
            original,
            "different target paths cannot share a clip"
        );
        bones
            .bones
            .get_mut("Root")
            .unwrap()
            .children
            .insert("Child".into());
        bones.bones.get_mut("Child").unwrap().transform = BoneTransform::new(None, None, Some(2.0));
        assert_ne!(
            tree_context(&bones),
            original,
            "different reference transforms cannot share a clip"
        );
    }

    fn actor_contract_fixture(parts: &[(u32, u32, &str, &str)]) -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE records(form_id INTEGER,record_type TEXT,data BLOB);
                 CREATE TABLE actor_models(id INTEGER PRIMARY KEY,race_id INTEGER,sex TEXT);
                 CREATE TABLE actor_model_parts(npc_id INTEGER,slot TEXT,model_path TEXT);",
            )
            .unwrap();
        for (npc, race, sex, path) in parts {
            connection
                .execute(
                    "INSERT INTO actor_models(id,race_id,sex) VALUES (?1,?2,?3)",
                    rusqlite::params![npc, race, sex],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO actor_model_parts(npc_id,slot,model_path) VALUES (?1,'body',?2)",
                    rusqlite::params![npc, path],
                )
                .unwrap();
        }
        connection
    }

    #[test]
    fn contracts_deduplicate_canonical_bodies_and_report_invalid_paths() {
        // SQL DISTINCT sees three authored strings. Two name the same converted body;
        // the third cannot name a mesh. None of them may abort the conversion.
        let connection = actor_contract_fixture(&[
            (1, 7, "male", r"Actors\Horse\Horse.nif"),
            (2, 7, "male", "actors/horse/horse.nif"),
            (3, 7, "female", "actors/horse/horse.nif"),
            (4, 7, "male", "../escape.nif"),
        ]);
        let set = contracts(&connection).unwrap();
        let keys: Vec<_> = set
            .contracts
            .iter()
            .map(|row| (row.race, row.sex.as_str(), row.body.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                (7, "female", "meshes/actors/horse/horse.nif"),
                (7, "male", "meshes/actors/horse/horse.nif"),
            ]
        );
        assert_eq!(set.rejected.len(), 1);
        assert_eq!(set.rejected[0].body_scene, "../escape.nif");
        assert!(set.rejected[0].reason.contains("traversal"));
        assert_eq!(set.character_project_bodies, 0);
    }

    #[test]
    fn authored_body_keys_match_mesh_publication_and_runtime() {
        for path in [
            r"Actors\\Horse\Horse.nif",
            " ./Actors//Horse/./Horse.NIF ",
            "Data/Meshes/Data/Meshes/Actors/Horse/Horse.nif",
            "Actors/%48orse/Horse.nif",
            "Actors/Horse%2520/Body.nif",
        ] {
            let source = shared::actor_idle_catalog::canonical_body_path(path, "nif").unwrap();
            let runtime = shared::actor_idle_catalog::canonical_body_path(path, "glb").unwrap();
            assert_eq!(
                source,
                canonical_asset_path(path, AssetKind::Mesh, "nif").unwrap()
            );
            assert_eq!(
                runtime,
                canonical_asset_path(path, AssetKind::Mesh, "glb").unwrap()
            );
            assert_eq!(
                Path::new(&source).with_extension("glb").to_string_lossy(),
                runtime
            );
        }
    }

    #[test]
    fn malformed_race_authoring_keeps_the_original_diagnostic() {
        for (tags, expected) in [
            (vec![*b"NAM3", *b"MODL"], "no sex selector"),
            (
                vec![*b"NAM3", *b"MNAM", *b"MODL", *b"MODL"],
                "duplicate race authoring model slot",
            ),
        ] {
            let connection = actor_contract_fixture(&[(1, 7, "male", "actors/custom/body.nif")]);
            let record = shared::records::ArchivedRecordData {
                subrecords: tags
                    .into_iter()
                    .map(|tag| shared::records::ArchivedSubRecord {
                        tag,
                        data: if tag == *b"MODL" {
                            b"actors/custom/project.hkx\0".to_vec()
                        } else {
                            Vec::new()
                        },
                    })
                    .collect(),
            };
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&record).unwrap();
            connection
                .execute(
                    "INSERT INTO records(form_id,record_type,data) VALUES(7,'RACE',?1)",
                    [bytes.as_slice()],
                )
                .unwrap();
            let set = contracts(&connection).unwrap();
            assert!(set.contracts.is_empty());
            assert_eq!(set.rejected.len(), 1);
            assert!(
                set.rejected[0].reason.contains(expected),
                "{}",
                set.rejected[0].reason
            );
        }
    }

    #[test]
    fn invalid_body_paths_are_unresolved_without_fail_fast_and_fatal_with_it() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("skyrim_world.db");
        actor_contract_fixture(&[(4, 7, "male", "../escape.nif")])
            .backup(
                rusqlite::MAIN_DB,
                &database,
                None::<fn(rusqlite::backup::Progress)>,
            )
            .unwrap();
        let mut manifest = ConversionManifest::default();
        let mut report = PipelineReport::default();
        convert(
            directory.path(),
            &BTreeMap::new(),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();
        let catalog: ActorIdleCatalog =
            serde_json::from_slice(&fs::read(directory.path().join(PATH)).unwrap()).unwrap();
        catalog.validate().unwrap();
        assert!(catalog.bindings.is_empty());
        assert_eq!(catalog.unresolved.len(), 1);
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("1 unresolved bodies"))
        );
        let error = convert(
            directory.path(),
            &BTreeMap::new(),
            true,
            &mut PipelineReport::default(),
            &mut manifest,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("invalid authored body path"));
    }

    #[test]
    fn published_clip_validation_matches_the_catalog_rules() {
        let clip = shared::actor_idle_catalog::ActorIdleClip {
            scene: "meshes/actor-idles/clips/a.glb".into(),
            frame_count: 2,
            frames_per_second: 30.0,
            motion_frame: Some(1),
            bound_tracks: 1,
            moving_bound_tracks: 1,
            skipped_tracks: Vec::new(),
        };
        assert!(shared::actor_idle_catalog::validate_clip("meshes/a/idle.hkx", &clip).is_ok());
        for source in [
            "meshes/a/idle#1.hkx",
            "meshes/a/Idle.hkx",
            "meshes/a/idle?.hkx",
        ] {
            assert!(shared::actor_idle_catalog::validate_clip(source, &clip).is_err());
        }
        let mut invalid = clip;
        invalid.frames_per_second = f32::NAN;
        assert!(shared::actor_idle_catalog::validate_clip("meshes/a/idle.hkx", &invalid).is_err());
    }

    #[test]
    fn diagnostic_archive_does_not_gain_a_runtime_animation_projection() {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("skyrim_world.db")).unwrap();
        connection
            .execute_batch("CREATE TABLE records(form_id INTEGER,record_type TEXT,data BLOB);")
            .unwrap();
        let mut manifest = ConversionManifest::default();
        convert(
            directory.path(),
            &BTreeMap::new(),
            true,
            &mut PipelineReport::default(),
            &mut manifest,
        )
        .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='race_animation_sources'",
                    [],
                    |row| row.get::<_, u32>(0)
                )
                .unwrap(),
            0
        );
        assert!(!directory.path().join(PATH).exists());
    }

    #[test]
    fn no_database_retires_only_owned_idle_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let output = "meshes/actor-idles/clips/old.glb";
        fs::create_dir_all(directory.path().join("meshes/actor-idles/clips")).unwrap();
        fs::write(directory.path().join(output), b"stale").unwrap();
        fs::write(
            directory.path().join("meshes/actor-idles/clips/keep.txt"),
            b"caller owned",
        )
        .unwrap();
        let mut manifest = ConversionManifest::default();
        manifest
            .derived_outputs
            .insert(output.into(), proof(RECIPE, BTreeMap::new(), b"stale"));
        convert(
            directory.path(),
            &BTreeMap::new(),
            true,
            &mut PipelineReport::default(),
            &mut manifest,
        )
        .unwrap();
        assert!(!directory.path().join(output).exists());
        assert!(manifest.derived_outputs.is_empty());
        assert!(
            directory
                .path()
                .join("meshes/actor-idles/clips/keep.txt")
                .exists()
        );
    }

    #[test]
    fn catalog_directory_preflight_preserves_old_outputs() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(PATH)).unwrap();
        let output = "meshes/actor-idles/clips/old.glb";
        fs::create_dir_all(directory.path().join("meshes/actor-idles/clips")).unwrap();
        fs::write(directory.path().join(output), b"stale").unwrap();
        let mut manifest = ConversionManifest::default();
        manifest
            .derived_outputs
            .insert(output.into(), proof(RECIPE, BTreeMap::new(), b"stale"));
        assert!(
            convert(
                directory.path(),
                &BTreeMap::new(),
                true,
                &mut PipelineReport::default(),
                &mut manifest
            )
            .is_err()
        );
        assert!(directory.path().join(output).exists());
        assert!(manifest.derived_outputs.contains_key(output));
    }

    /// Reproject body slots from the immutable pack's actor-related canonical
    /// records and exact preserved winning header flags. Admit additions only
    /// where the prior catalog's effective race/sex/skin identity agrees.
    #[cfg(unix)]
    fn refresh_creature_parts(original: &Connection, fixture: &Connection) -> Vec<u32> {
        use crate::esm::records::RawRecord;
        let mut records = std::collections::HashMap::new();
        let mut statement = original.prepare("SELECT r.form_id,r.record_type,r.load_order,r.data,h.flags FROM records r JOIN inhouse_source_records h ON h.form_id=r.form_id AND h.load_order=r.load_order AND h.record_type=r.record_type WHERE r.record_type IN ('NPC_','RACE','ARMO','ARMA','LVLN')").unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, u32>(4)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (id, kind, load_order, bytes, flags) = row.unwrap();
            let data =
                rkyv::from_bytes::<shared::records::ArchivedRecordData, rkyv::rancor::Error>(
                    &bytes,
                )
                .unwrap();
            records.insert(
                id,
                RawRecord {
                    form_id: id,
                    record_type: kind.as_bytes().try_into().unwrap(),
                    flags,
                    subrecords: data
                        .subrecords
                        .into_iter()
                        .map(|field| (field.tag.to_vec(), field.data))
                        .collect(),
                    cell_form_id: None,
                    worldspace_form_id: None,
                    load_order,
                },
            );
        }
        let expected_headers: usize = original.query_row("SELECT COUNT(*) FROM records WHERE record_type IN ('NPC_','RACE','ARMO','ARMA','LVLN')",[],|row| row.get(0)).unwrap();
        assert_eq!(
            records.len(),
            expected_headers,
            "every actor record needs an exact preserved header"
        );
        let projected =
            crate::actor_models::resolve_actor_models(&records, &std::collections::HashMap::new());
        let models: std::collections::HashMap<_, _> =
            projected.models.iter().map(|row| (row.id, row)).collect();
        let mut added = Vec::new();
        for part in &projected.parts {
            if part.slot != crate::actor_models::ActorModelSlot::Body {
                continue;
            }
            let row = models[&part.npc_id];
            let original = fixture.query_row("SELECT race_id,sex,skin_armo_id,EXISTS(SELECT 1 FROM actor_model_parts WHERE npc_id=a.id AND slot='body') FROM actor_models a WHERE id=?1",[part.npc_id],|record| Ok((record.get::<_,u32>(0)?,record.get::<_,String>(1)?,record.get::<_,Option<u32>>(2)?,record.get::<_,bool>(3)?))).unwrap();
            if original.3
                || (original.0, original.1.as_str(), original.2)
                    != (row.race_id, row.sex, row.skin_armo_id)
            {
                continue;
            }
            let race = records.get(&row.race_id).unwrap();
            let authoring = authoring::read_race_authoring_fields(
                race.subrecords
                    .iter()
                    .map(|(tag, data)| (tag.as_slice(), data.as_slice())),
            )
            .unwrap();
            let project = if row.sex == "female" {
                authoring.female_project
            } else {
                authoring.male_project
            };
            if project.is_none_or(|project| project.starts_with("meshes/actors/character/")) {
                continue;
            }
            fixture.execute("INSERT INTO actor_model_parts(npc_id,slot,model_path,armature_race_id,skin_texture_set_id) VALUES (?1,'body',?2,?3,?4)",rusqlite::params![part.npc_id,part.model_path,part.armature_race_id,part.skin_texture_set_id]).unwrap();
            fixture.execute("UPDATE actor_models SET resolution=CASE WHEN head_model IS NULL THEN 'body' ELSE 'body_head' END WHERE id=?1",[part.npc_id]).unwrap();
            added.push(part.npc_id);
        }
        added
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires original Data, extracted HKX corpus, immutable playtest pack, and new output"]
    fn retail_actor_idle_body_coverage() {
        let metadata = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_AUTHORING_INPUT").unwrap());
        let clips = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_CLIPS_INPUT").unwrap());
        let pack = PathBuf::from(std::env::var_os("MUDCRAB_ACTOR_IDLE_PACK").unwrap());
        let output = PathBuf::from(std::env::var_os("MUDCRAB_IDLE_AUTHORING_OUTPUT").unwrap());
        fs::create_dir(&output).expect("new task-owned coverage directory");
        let manifest_bytes = fs::read(pack.join("conversion-manifest.json")).unwrap();

        let db_hash = crate::cache::hash_file(&pack.join("skyrim_world.db")).unwrap();
        let connection = Connection::open_with_flags(
            pack.join("skyrim_world.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let fixture = Connection::open(output.join("skyrim_world.db")).unwrap();
        fixture
            .execute(
                "ATTACH DATABASE ?1 AS original",
                [format!(
                    "file:{}?mode=ro",
                    pack.join("skyrim_world.db").to_string_lossy()
                )],
            )
            .unwrap();
        fixture.execute_batch("CREATE TABLE records AS SELECT * FROM original.records WHERE record_type='RACE'; CREATE TABLE actor_models AS SELECT * FROM original.actor_models; CREATE TABLE actor_model_parts AS SELECT * FROM original.actor_model_parts; DETACH DATABASE original;").unwrap();
        let added = refresh_creature_parts(&connection, &fixture);
        let set = contracts(&fixture).unwrap();
        assert!(set.rejected.is_empty(), "{:?}", set.rejected);
        let rows = set.contracts;
        drop(fixture);
        let expected = rows.len();
        let mut sources = BTreeMap::new();
        for folder in [&metadata, &clips] {
            for entry in walkdir::WalkDir::new(folder) {
                let entry = entry.unwrap();
                if entry.file_type().is_file()
                    && entry.path().extension().is_some_and(|ext| ext == "hkx")
                {
                    sources.insert(
                        output
                            .join("vfs")
                            .join(entry.path().strip_prefix(folder).unwrap()),
                        entry.path().to_owned(),
                    );
                }
            }
        }
        let mut body_hashes = BTreeMap::new();
        for row in &rows {
            let scene = canonical_asset_path(&row.body, AssetKind::Mesh, "glb").unwrap();
            if !pack.join(&scene).is_file() {
                continue;
            }
            let destination = output.join(&scene);
            if destination.exists() {
                continue;
            }
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(pack.join(&scene), &destination).unwrap();
            body_hashes.insert(
                scene.clone(),
                crate::cache::hash_file(&pack.join(scene)).unwrap(),
            );
        }
        let mut manifest = ConversionManifest::default();
        let mut report = PipelineReport::default();
        convert(&output, &sources, false, &mut report, &mut manifest).unwrap();
        fs::write(
            output.join("idle-proofs.json"),
            serde_json::to_vec_pretty(&manifest.derived_outputs).unwrap(),
        )
        .unwrap();
        fs::write(output.join("coverage-inputs.json"), serde_json::to_vec_pretty(&serde_json::json!({"head":"64108a1c38b0a854d4f82d7031ea50b57c336a5a","pack":pack,"manifest_sha256":hash_bytes(&manifest_bytes),"database_sha256":db_hash,"body_sha256":body_hashes,"new_body_bases":added,"fixture_scope":"actor-related canonical records plus exact preserved winning header flags; only matching existing effective identities admitted","notices":report.notices})).unwrap()).unwrap();
        assert_eq!(
            crate::cache::hash_file(&pack.join("skyrim_world.db")).unwrap(),
            db_hash
        );
        assert_eq!(
            fs::read(pack.join("conversion-manifest.json")).unwrap(),
            manifest_bytes
        );
        for (scene, hash) in &body_hashes {
            assert_eq!(&crate::cache::hash_file(&pack.join(scene)).unwrap(), hash);
        }
        let catalog: ActorIdleCatalog =
            serde_json::from_slice(&fs::read(output.join(PATH)).unwrap()).unwrap();
        println!(
            "{expected} standing-idle body contracts; {} bound, {} unresolved; {}",
            catalog.bindings.len(),
            catalog.unresolved.len(),
            output.display()
        );
        assert_eq!(
            catalog.bindings.len() + catalog.unresolved.len() + catalog.excluded.len(),
            expected
        );
        assert!(
            catalog.unresolved.is_empty(),
            "standing-idle body coverage has unresolved contracts; inspect retained catalog"
        );
    }
}
