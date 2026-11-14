//! Decoding of Havok 2010.2 64-bit animation clips and skeletons.
//!
//! Only the Skyrim SE layout is supported: 64-bit pointers, three sections, and
//! `hk_2010.2.0-r1`. The decoded clip is plain data; GLB emission belongs to a later stage.

pub mod authoring;
mod classes;
pub mod glb;
pub mod idle_defaults;
mod packfile;
mod spline;

pub use glb::{is_animation_output, write_clip_glb};

use classes::{Binding, Skeleton, SplineAnimation};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use packfile::Packfile;
use spline::Decoded;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub struct AnimationClip {
    /// Havok hkaAnimationBinding blend hint: 0 is normal, 1 is additive.
    pub blend_hint: u8,
    /// The packfile carries no clip name, so this is empty. Callers set it from the asset path.
    pub name: String,
    pub original_skeleton_name: String,
    pub duration_seconds: f32,
    pub frame_count: u32,
    pub frames_per_second: f32,
    /// Indexed by track index.
    pub tracks: Vec<TransformTrack>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformTrack {
    /// Resolved through the binding map; identity when that map is empty.
    pub bone_index: u16,
    /// Filled in when decoded with a skeleton.
    pub bone_name: Option<String>,
    /// One sample per integer frame, `0..frame_count`.
    pub samples: Vec<TransformSample>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformSample {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkeletonBones {
    pub name: String,
    pub bone_names: Vec<String>,
    pub parent_indices: Vec<i16>,
    /// The first bone whose parent index is negative, if the skeleton declares a root.
    pub root_index: Option<usize>,
}

impl SkeletonBones {
    pub fn root_name(&self) -> Option<&str> {
        self.root_index
            .and_then(|index| self.bone_names.get(index))
            .map(String::as_str)
    }
}

/// Decodes a clip without a skeleton; every track keeps `bone_name = None`.
pub fn decode_clip(bytes: &[u8]) -> Result<AnimationClip> {
    decode(bytes, None)
}

/// Decodes a clip and names every track from the skeleton whose name matches the clip's
/// `originalSkeletonName`.
pub fn decode_clip_with_skeleton(bytes: &[u8], skeleton_bytes: &[u8]) -> Result<AnimationClip> {
    decode(bytes, Some(skeleton_bytes))
}

/// Decodes the first skeleton in a skeleton packfile.
pub fn decode_skeleton(bytes: &[u8]) -> Result<SkeletonBones> {
    let packfile = Packfile::parse(bytes)?;
    let skeletons = read_skeletons(&packfile)?;
    let skeleton = skeletons
        .first()
        .ok_or_else(|| eyre!("skeleton container has no skeletons"))?;
    Ok(skeleton_bones(skeleton))
}

/// How a clip's animated bones matched the bone names of the body it plays on.
/// Duplicate names and tracks whose target node is already claimed are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoneBindingReport {
    /// Tracks whose bone name matched a body node exactly.
    pub exact: usize,
    /// Tracks renamed to a node that differs only in case: `(clip name, body name)`.
    pub aliased: Vec<(String, String)>,
    /// Tracks that do not uniquely own a body node, sorted by clip name.
    pub skipped: Vec<String>,
}

impl BoneBindingReport {
    /// Number of tracks that address a body node after binding.
    pub fn bound(&self) -> usize {
        self.exact + self.aliased.len()
    }
}

/// Binds a decoded clip to the bone names of the body it will play on.
///
/// Exact names win. A name that matches exactly one body node once case is
/// ignored is rewritten to that node's spelling, which is how the attachment
/// bones are stored: `QUIVER`/`SHIELD`/`WEAPON` in `skeleton.nif` against
/// `Quiver`/`Shield`/`Weapon` in `skeleton.hkx`. A bone with no body node, or
/// with more than one case-insensitive candidate, is cleared and reported
/// rather than guessed at.
pub fn bind_clip_to_bones(
    clip: &mut AnimationClip,
    body_bone_names: &BTreeSet<String>,
) -> BoneBindingReport {
    let mut report = BoneBindingReport::default();
    let mut claimed = BTreeSet::new();

    // Reserve exact targets first so an alias earlier in clip order cannot take them.
    for track in &mut clip.tracks {
        let Some(name) = track.bone_name.as_ref() else {
            continue;
        };
        if !body_bone_names.contains(name) {
            continue;
        }
        if claimed.insert(name.clone()) {
            report.exact += 1;
        } else {
            report.skipped.push(name.clone());
            track.bone_name = None;
        }
    }

    for track in &mut clip.tracks {
        let Some(name) = track.bone_name.clone() else {
            continue;
        };
        if body_bone_names.contains(&name) {
            continue;
        }
        let folded = name.to_ascii_lowercase();
        let mut candidates = body_bone_names
            .iter()
            .filter(|body| body.to_ascii_lowercase() == folded);
        match (candidates.next(), candidates.next()) {
            (Some(only), None) if claimed.insert(only.clone()) => {
                track.bone_name = Some(only.clone());
                report.aliased.push((name, only.clone()));
            }
            _ => {
                track.bone_name = None;
                report.skipped.push(name);
            }
        }
    }

    report.aliased.sort();
    report.skipped.sort();
    report
}

fn decode(bytes: &[u8], skeleton_bytes: Option<&[u8]>) -> Result<AnimationClip> {
    let packfile = Packfile::parse(bytes)?;
    let (animation, binding, decoded) = decode_parts(&packfile)?;

    let skeleton = match skeleton_bytes {
        Some(bytes) => {
            let skeleton_packfile = Packfile::parse(bytes)?;
            let skeletons = read_skeletons(&skeleton_packfile)?;
            Some(select_skeleton(&skeletons, &binding.original_skeleton_name)?.clone())
        }
        None => None,
    };

    build_clip(animation, binding, decoded, skeleton.as_ref())
}

fn decode_parts(packfile: &Packfile) -> Result<(SplineAnimation, Binding, Decoded)> {
    let container = animation_container(packfile)?;
    ensure!(
        container.animations.len() == 1,
        "expected one animation, found {}",
        container.animations.len()
    );
    ensure!(
        container.bindings.len() == 1,
        "expected one binding, found {}",
        container.bindings.len()
    );

    let animation_at = container.animations[0];
    let animation_object = packfile.object_at(animation_at)?;
    ensure!(
        animation_object.class == "hkaSplineCompressedAnimation",
        "animation is {}, only hkaSplineCompressedAnimation is supported",
        animation_object.class
    );
    let animation = classes::spline_animation(packfile, animation_at)?;

    let mut binding = classes::binding(packfile, container.bindings[0])?;
    ensure!(
        binding.animation == Some(animation_at),
        "binding points at {:?}, animation is at {animation_at:#x}",
        binding.animation
    );
    if animation.extracted_motion.is_some() {
        log::warn!("clip carries extracted motion; only its transform tracks are decoded");
    }

    let track_count = usize::try_from(animation.num_transform_tracks).map_err(|_| {
        eyre!(
            "clip has {} transform tracks",
            animation.num_transform_tracks
        )
    })?;
    if binding.transform_track_to_bone.is_empty() {
        binding.transform_track_to_bone = (0..track_count)
            .map(|index| {
                i16::try_from(index)
                    .map_err(|_| eyre!("track index {index} does not fit the binding map"))
            })
            .collect::<Result<Vec<_>>>()?;
    } else {
        ensure!(
            binding.transform_track_to_bone.len() == track_count,
            "binding maps {} tracks but the animation has {track_count}",
            binding.transform_track_to_bone.len()
        );
    }

    let decoded = Decoded::new(&animation)?;
    Ok((animation, binding, decoded))
}

fn build_clip(
    animation: SplineAnimation,
    binding: Binding,
    decoded: Decoded,
    skeleton: Option<&Skeleton>,
) -> Result<AnimationClip> {
    let frame_count = u32::try_from(animation.num_frames)
        .map_err(|_| eyre!("clip has {} frames", animation.num_frames))?;
    ensure!(frame_count > 0, "clip has no frames");
    ensure!(
        animation.duration.is_finite() && animation.duration >= 0.0,
        "clip duration {} is invalid",
        animation.duration
    );
    ensure!(
        animation.frame_duration.is_finite() && animation.frame_duration > 0.0,
        "clip frame duration {} is invalid",
        animation.frame_duration
    );

    let mut tracks = Vec::with_capacity(binding.transform_track_to_bone.len());
    for (track_index, &bone_index) in binding.transform_track_to_bone.iter().enumerate() {
        let bone_index = u16::try_from(bone_index)
            .map_err(|_| eyre!("track {track_index} has bone index {bone_index}"))?;
        let bone_name = match skeleton {
            Some(skeleton) => {
                let name = skeleton.bone_names.get(usize::from(bone_index)).ok_or_else(|| {
                    eyre!(
                        "track {track_index} bone index {bone_index} is out of range for skeleton {:?} with {} bones",
                        skeleton.name,
                        skeleton.bone_names.len()
                    )
                })?;
                Some(name.clone())
            }
            None => None,
        };
        let samples = (0..frame_count as usize)
            .map(|frame| decoded.sample(track_index, frame))
            .collect::<Result<Vec<_>>>()?;
        tracks.push(TransformTrack {
            bone_index,
            bone_name,
            samples,
        });
    }

    Ok(AnimationClip {
        blend_hint: binding.blend_hint,
        name: String::new(),
        original_skeleton_name: binding.original_skeleton_name,
        duration_seconds: animation.duration,
        frame_count,
        frames_per_second: 1.0 / animation.frame_duration,
        tracks,
    })
}

fn animation_container(packfile: &Packfile) -> Result<classes::AnimationContainer> {
    let variants = classes::root_level_container(packfile)?;
    let variant = variants
        .iter()
        .find(|variant| variant.class_name == "hkaAnimationContainer")
        .ok_or_else(|| eyre!("packfile has no hkaAnimationContainer variant"))?;
    let at = variant
        .variant
        .ok_or_else(|| eyre!("hkaAnimationContainer variant is a null pointer"))?;
    classes::animation_container(packfile, at)
}

fn read_skeletons(packfile: &Packfile) -> Result<Vec<Skeleton>> {
    let container = animation_container(packfile)?;
    container
        .skeletons
        .iter()
        .map(|&at| classes::skeleton(packfile, at))
        .collect()
}

fn select_skeleton<'a>(skeletons: &'a [Skeleton], name: &str) -> Result<&'a Skeleton> {
    skeletons
        .iter()
        .find(|skeleton| skeleton.name == name)
        .ok_or_else(|| {
            let available: Vec<&str> = skeletons
                .iter()
                .map(|skeleton| skeleton.name.as_str())
                .collect();
            eyre!("clip names skeleton {name:?}, but the skeleton packfile has {available:?}")
        })
}

fn skeleton_bones(skeleton: &Skeleton) -> SkeletonBones {
    SkeletonBones {
        name: skeleton.name.clone(),
        bone_names: skeleton.bone_names.clone(),
        parent_indices: skeleton.parent_indices.clone(),
        root_index: skeleton
            .parent_indices
            .iter()
            .position(|&parent| parent < 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_wormhole_nif::nif_file::NifFile;
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    #[test]
    fn selects_the_skeleton_named_by_the_clip() {
        let skeletons = vec![
            Skeleton {
                name: "Ragdoll".to_owned(),
                parent_indices: vec![-1],
                bone_names: vec!["COM".to_owned()],
            },
            Skeleton {
                name: "NPC Root [Root]".to_owned(),
                parent_indices: vec![-1],
                bone_names: vec!["NPC Root [Root]".to_owned()],
            },
        ];
        let selected = select_skeleton(&skeletons, "NPC Root [Root]").expect("skeleton exists");
        assert_eq!(selected.name, "NPC Root [Root]");
    }

    #[test]
    fn missing_skeleton_names_the_available_ones() {
        let skeletons = vec![Skeleton {
            name: "Ragdoll".to_owned(),
            parent_indices: vec![-1],
            bone_names: vec!["COM".to_owned()],
        }];
        let error = select_skeleton(&skeletons, "NPC Root [Root]")
            .expect_err("no matching skeleton")
            .to_string();
        assert!(error.contains("NPC Root [Root]"), "got {error:?}");
        assert!(error.contains("Ragdoll"), "got {error:?}");
    }

    fn clip_with_bones(names: &[&str]) -> AnimationClip {
        AnimationClip {
            blend_hint: 0,
            name: String::new(),
            original_skeleton_name: "NPC Root [Root]".to_owned(),
            duration_seconds: 1.0,
            frame_count: 1,
            frames_per_second: 30.0,
            tracks: names
                .iter()
                .map(|name| TransformTrack {
                    bone_index: 0,
                    bone_name: Some((*name).to_owned()),
                    samples: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn binds_clip_bones_to_body_nodes_by_exact_then_case_folded_name() {
        let mut clip = clip_with_bones(&["Spine", "QUIVER", "Quiver", "Missing", "weapon"]);
        let body: BTreeSet<String> = ["Spine", "Quiver", "SHIELD", "WEAPON"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let report = bind_clip_to_bones(&mut clip, &body);

        assert_eq!(report.exact, 2);
        assert_eq!(
            report.aliased,
            vec![("weapon".to_owned(), "WEAPON".to_owned()),]
        );
        assert_eq!(
            report.skipped,
            vec!["Missing".to_owned(), "QUIVER".to_owned()]
        );
        assert_eq!(report.bound(), 3);
        assert_eq!(clip.tracks[1].bone_name, None);
        assert_eq!(clip.tracks[4].bone_name.as_deref(), Some("WEAPON"));
        assert_eq!(clip.tracks[3].bone_name, None);
    }

    #[test]
    fn exact_binding_wins_alias_in_either_track_order_and_keeps_distinct_nodes() {
        for names in [["QUIVER", "Quiver"], ["Quiver", "QUIVER"]] {
            let mut clip = clip_with_bones(&names);
            clip.tracks[0].samples = vec![TransformSample {
                translation: [1.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            }];
            clip.tracks[1].samples = vec![TransformSample {
                translation: [2.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            }];
            let body: BTreeSet<String> = ["Quiver", "Shield"]
                .into_iter()
                .map(str::to_owned)
                .collect();

            let report = bind_clip_to_bones(&mut clip, &body);

            assert_eq!(report.exact, 1);
            assert!(report.aliased.is_empty());
            assert_eq!(report.skipped, vec!["QUIVER"]);
            let exact = clip
                .tracks
                .iter()
                .find(|track| track.bone_name.as_deref() == Some("Quiver"))
                .unwrap();
            let expected = if names[0] == "Quiver" { 1.0 } else { 2.0 };
            assert_eq!(exact.samples[0].translation[0], expected);
            let distinct = clip_with_bones(&["Quiver", "Shield"]);
            let distinct_report = bind_clip_to_bones(&mut distinct.clone(), &body);
            assert_eq!(distinct_report.bound(), 2);
        }
    }

    #[test]
    fn duplicate_exact_tracks_are_skipped_after_first_mapping() {
        let mut clip = clip_with_bones(&["Spine", "Spine"]);
        let body: BTreeSet<String> = ["Spine"].into_iter().map(str::to_owned).collect();
        let report = bind_clip_to_bones(&mut clip, &body);
        assert_eq!(report.exact, 1);
        assert_eq!(report.skipped, vec!["Spine"]);
        assert_eq!(clip.tracks[0].bone_name.as_deref(), Some("Spine"));
        assert_eq!(clip.tracks[1].bone_name, None);
    }

    #[test]
    fn ambiguous_case_folded_match_is_skipped() {
        let mut clip = clip_with_bones(&["quiver"]);
        let body: BTreeSet<String> = ["Quiver", "QUIVER"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let report = bind_clip_to_bones(&mut clip, &body);

        assert_eq!(report.exact, 0);
        assert!(report.aliased.is_empty());
        assert_eq!(report.skipped, vec!["quiver".to_owned()]);
        assert_eq!(clip.tracks[0].bone_name, None);
    }

    fn retail_root() -> PathBuf {
        std::env::var_os("MUDCRAB_ANIMATION_DIR")
            .map(PathBuf::from)
            .expect("MUDCRAB_ANIMATION_DIR must point at the extracted character directory")
    }

    fn skeleton_nif_path(root: &Path) -> PathBuf {
        let direct = root.join("character assets/skeleton.nif");
        if direct.is_file() {
            return direct;
        }
        // The animation extraction ships only .hkx files; the matching skeleton NIF sits in
        // the sibling mesh extraction of the same asset tree.
        root.ancestors()
            .map(|base| {
                base.join("bsa-meshes/meshes/actors/character/character assets/skeleton.nif")
            })
            .find(|candidate| candidate.is_file())
            .unwrap_or(direct)
    }

    #[test]
    #[ignore = "requires MUDCRAB_ANIMATION_DIR pointing at the extracted character directory"]
    fn retail_mt_idle_decodes_and_matches_skeleton_nif() {
        let root = retail_root();
        let skeleton_bytes =
            std::fs::read(root.join("character assets/skeleton.hkx")).expect("read skeleton.hkx");
        let male_bytes =
            std::fs::read(root.join("animations/male/mt_idle.hkx")).expect("read male mt_idle.hkx");
        let mut male = decode_clip_with_skeleton(&male_bytes, &skeleton_bytes)
            .expect("decode male mt_idle.hkx");

        println!(
            "male mt_idle: duration {:.6}s, {} frames, {} tracks, {:.3} fps",
            male.duration_seconds,
            male.frame_count,
            male.tracks.len(),
            male.frames_per_second
        );
        assert!(
            (male.duration_seconds - 9.133_333).abs() <= 1e-5,
            "male duration {}",
            male.duration_seconds
        );
        assert_eq!(male.frame_count, 275);
        assert_eq!(male.tracks.len(), 99);
        for track in &male.tracks {
            assert_eq!(
                track.samples.len(),
                275,
                "track {} has {} samples",
                track.bone_index,
                track.samples.len()
            );
            assert!(
                track.bone_name.is_some(),
                "track {} has no bone name",
                track.bone_index
            );
        }

        let first = male.tracks[0].samples[0];
        for sample in &male.tracks[0].samples {
            assert_eq!(
                sample.translation, first.translation,
                "NPC Root translation moves across frames"
            );
            assert_eq!(
                sample.rotation, first.rotation,
                "NPC Root rotation moves across frames"
            );
        }
        for track in &male.tracks {
            for sample in &track.samples {
                let norm = sample
                    .rotation
                    .iter()
                    .map(|value| value * value)
                    .sum::<f32>()
                    .sqrt();
                assert!(
                    (norm - 1.0).abs() <= 1e-6,
                    "track {} rotation norm {norm}",
                    track.bone_index
                );
            }
        }

        // Structural facts the decoder relies on: the binding map is empty (identity) and the
        // clip carries no extracted motion.
        let packfile = Packfile::parse(&male_bytes).expect("parse male mt_idle.hkx");
        let container = animation_container(&packfile).expect("animation container");
        let animation = classes::spline_animation(&packfile, container.animations[0])
            .expect("spline animation");
        assert!(
            animation.extracted_motion.is_none(),
            "mt_idle unexpectedly carries extracted motion"
        );
        let binding = classes::binding(&packfile, container.bindings[0]).expect("binding");
        assert!(
            binding.transform_track_to_bone.is_empty(),
            "mt_idle is expected to use the identity binding map"
        );
        assert_eq!(male.tracks[0].bone_name.as_deref(), Some("NPC Root [Root]"));

        let female_bytes = std::fs::read(root.join("animations/female/mt_idle.hkx"))
            .expect("read female mt_idle.hkx");
        let female = decode_clip_with_skeleton(&female_bytes, &skeleton_bytes)
            .expect("decode female mt_idle.hkx");
        println!(
            "female mt_idle: duration {:.6}s, {} frames, {} tracks, {:.3} fps",
            female.duration_seconds,
            female.frame_count,
            female.tracks.len(),
            female.frames_per_second
        );
        assert_eq!(female.tracks.len(), 99);
        assert!(female.duration_seconds > 0.0);

        let nif_path = skeleton_nif_path(&root);
        let nif = NifFile::open(
            nif_path
                .to_str()
                .expect("skeleton NIF path must be valid UTF-8"),
        )
        .expect("parse skeleton.nif");
        let nif_names: BTreeSet<String> = nif
            .get_nodes()
            .iter()
            .filter_map(|node| {
                nif.header
                    .get_string(node.av.object.name as usize)
                    .ok()
                    .map(str::to_owned)
            })
            .collect();
        let report = bind_clip_to_bones(&mut male, &nif_names);
        assert_eq!(report.exact, 93, "exact bone matches");
        assert_eq!(
            report.aliased,
            vec![
                ("Quiver".to_owned(), "QUIVER".to_owned()),
                ("Shield".to_owned(), "SHIELD".to_owned()),
                ("Weapon".to_owned(), "WEAPON".to_owned()),
            ]
        );
        assert_eq!(
            report.skipped,
            vec![
                "x_NPC LookNode [Look]".to_owned(),
                "x_NPC Rotate [Rot ]".to_owned(),
                "x_NPC Translate [Pos ]".to_owned(),
            ]
        );
        assert_eq!(report.bound(), 96);

        for track in &male.tracks {
            if let Some(name) = &track.bone_name {
                assert!(
                    nif_names.contains(name),
                    "bound bone {name} is not a body node"
                );
            }
        }

        let animated: BTreeSet<String> = male
            .tracks
            .iter()
            .filter_map(|track| track.bone_name.clone())
            .collect();
        let unanimated: Vec<&str> = nif_names
            .iter()
            .filter(|name| !animated.contains(*name))
            .map(String::as_str)
            .collect();
        assert_eq!(unanimated, ["CharacterBumper", "NPC", "skeleton.nif"]);
    }
}
