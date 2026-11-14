//! Writes a decoded clip as an animation-only GLB over a body's bone tree.
//!
//! The body owns the hierarchy: a channel target is a node path, so the clip can only address
//! a bone if every ancestor of that bone exists too. The emitted node layout therefore mirrors
//! the composed body's exporter — one node per body bone in name order, children sorted, and a
//! single `Creation-to-glTF basis` wrapper whose index comes after the bones.

use super::{AnimationClip, BoneBindingReport, bind_clip_to_bones};
use crate::mesh::write_glb_atomic;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use gltf::json::{
    Accessor, Animation, Buffer, Extras, Index, Node, Root, Scene, Value,
    accessor::{ComponentType, GenericComponentType, Type},
    animation::{Channel, Interpolation, Property, Sampler, Target},
    buffer::View,
    scene::UnitQuaternion,
    validation::{Checked, USize64},
};
use project_wormhole_nif::model::all::{BoneData, BoneTransform, BoneTree};
use project_wormhole_shared::glam;
use project_wormhole_shared::prelude::{BSMatrix3, BSVec3};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Whether a staging-relative, `/`-separated path is an emitted animation clip.
///
/// Clips have no source NIF at their own path, so the staging cleanups that drop
/// outputs without a matching source must leave them alone.
pub fn is_animation_output(relative: &str) -> bool {
    relative == shared::actor_idle_catalog::PATH
        || (relative.starts_with("meshes/actor-idles/clips/") && relative.ends_with(".glb"))
        || (relative.starts_with("meshes/actors/character/animations/")
            && relative.ends_with(".glb"))
        || shared::creature_idle::CREATURE_IDLES
            .iter()
            .any(|profile| relative == profile.clip_scene)
}

/// Writes `clip` as a GLB whose nodes are `body_bones`, binding the clip's track names first.
///
/// Returns the binding report so callers can record which tracks were case-aliased or skipped.
pub fn write_clip_glb(
    clip: &mut AnimationClip,
    body_bones: &BoneTree,
    clip_name: &str,
    output: &Path,
) -> Result<BoneBindingReport> {
    let body_bone_names: BTreeSet<String> = body_bones.bones.keys().cloned().collect();
    let report = bind_clip_to_bones(clip, &body_bone_names);
    let glb = encode_clip_glb(clip, body_bones, clip_name)?;
    write_glb_atomic(output, &glb)?;
    Ok(report)
}

/// Reads the skin hierarchy the runtime will actually instantiate. This avoids
/// rebuilding a potentially different tree through a second NIF skeleton resolver.
/// All joint ancestors must have unique names and the converter's basis wrapper.
pub fn body_bones(scene: &Path) -> Result<BoneTree> {
    let body = gltf::Gltf::open(scene)?;
    let scene = body
        .default_scene()
        .or_else(|| body.scenes().next())
        .ok_or_else(|| eyre!("body has no scene"))?;
    let nodes: Vec<_> = body.nodes().collect();
    let mut parents = BTreeMap::new();
    for node in &nodes {
        for child in node.children() {
            ensure!(
                parents.insert(child.index(), node.index()).is_none(),
                "body node has multiple parents"
            );
        }
    }
    let roots: BTreeSet<_> = scene.nodes().map(|node| node.index()).collect();
    let wrappers: Vec<_> = nodes
        .iter()
        .filter(|node| {
            roots.contains(&node.index()) && node.name() == Some("Creation-to-glTF basis")
        })
        .collect();
    ensure!(
        wrappers.len() == 1,
        "body requires exactly one basis wrapper at its scene root"
    );
    let wrapper = wrappers[0].index();
    let mut required = BTreeSet::new();
    let mut include = |index| -> Result<()> {
        let mut at = index;
        let mut visited = BTreeSet::new();
        while at != wrapper {
            ensure!(visited.insert(at), "body hierarchy contains a cycle");
            required.insert(at);
            at = *parents
                .get(&at)
                .ok_or_else(|| eyre!("body bone is outside its basis wrapper"))?;
        }
        Ok(())
    };
    for joint in body
        .nodes()
        .filter(|node| node.mesh().is_some())
        .filter_map(|node| node.skin())
        .flat_map(|skin| skin.joints())
    {
        include(joint.index())?;
    }
    // Unweighted attachment bones can drive rigid child meshes too. Keep their
    // authored hierarchy without inventing joints or dropping a leaf's motion.
    let mut pending: Vec<_> = nodes[wrapper].children().collect();
    let mut visited = BTreeSet::new();
    while let Some(node) = pending.pop() {
        ensure!(
            visited.insert(node.index()),
            "body hierarchy contains a cycle"
        );
        if node.mesh().is_none() && node.name().is_some() {
            include(node.index())?;
        }
        pending.extend(node.children());
    }
    ensure!(
        !required.is_empty(),
        "body has no named bones below its basis wrapper"
    );
    let mut tree = BoneTree::new();
    for &index in &required {
        let node = &nodes[index];
        let name = node
            .name()
            .ok_or_else(|| eyre!("body skin ancestor has no name"))?;
        let children = node
            .children()
            .filter(|child| required.contains(&child.index()))
            .map(|child| {
                child
                    .name()
                    .map(str::to_owned)
                    .ok_or_else(|| eyre!("body skin ancestor has no name"))
            })
            .collect::<Result<_>>()?;
        let (translation, rotation, scale) = node.transform().decomposed();
        ensure!(
            translation
                .iter()
                .chain(&rotation)
                .chain(&scale)
                .all(|value| value.is_finite()),
            "body bone has a nonfinite transform"
        );
        ensure!(
            (scale[0] - scale[1]).abs() < 1e-6 && (scale[0] - scale[2]).abs() < 1e-6,
            "body bone has nonuniform scale"
        );
        let transform = BoneTransform::new(
            Some(BSVec3(glam::Vec3::from_array(translation))),
            Some(BSMatrix3(glam::Mat3::from_quat(glam::Quat::from_array(
                rotation,
            )))),
            Some(scale[0]),
        );
        tree.add_bone(
            name,
            BoneData {
                transform,
                children,
            },
        )
        .map_err(|message| eyre!(message))?;
    }
    Ok(tree)
}

fn encode_clip_glb(
    clip: &AnimationClip,
    body_bones: &BoneTree,
    clip_name: &str,
) -> Result<Vec<u8>> {
    ensure!(
        clip.blend_hint == 0,
        "standalone GLB requires a normal clip; blend hint {} needs an explicit base pose",
        clip.blend_hint
    );
    ensure!(
        !body_bones.bones.is_empty(),
        "cannot write a clip without body bones"
    );
    let frame_count = usize::try_from(clip.frame_count)
        .map_err(|_| eyre!("clip has {} frames", clip.frame_count))?;
    ensure!(frame_count > 0, "clip has no frames");
    ensure!(
        clip.frames_per_second.is_finite() && clip.frames_per_second > 0.0,
        "clip frame rate {} is invalid",
        clip.frames_per_second
    );
    let mut bound_tracks = 0;
    for track in &clip.tracks {
        let Some(name) = track.bone_name.as_deref() else {
            continue;
        };
        ensure!(
            body_bones.bones.contains_key(name),
            "clip track {} bound to bone {name}, which the body does not have",
            track.bone_index
        );
        ensure!(
            track.samples.len() == frame_count,
            "clip track {} ({name}) has {} samples but the clip has {frame_count} frames",
            track.bone_index,
            track.samples.len()
        );
        bound_tracks += 1;
    }
    ensure!(bound_tracks > 0, "clip has no bound transform tracks");

    let mut root = Root {
        scene: Some(Index::new(0)),
        ..Root::default()
    };
    root.scenes.push(Scene {
        extensions: None,
        extras: Extras::default(),
        name: None,
        nodes: Vec::new(),
    });
    root.buffers.push(Buffer {
        byte_length: USize64(0),
        name: None,
        uri: None,
        extensions: None,
        extras: Extras::default(),
    });

    let mut bin = Vec::new();
    let bone_nodes = push_bone_nodes(&mut root, body_bones)?;
    push_wrapper(&mut root);
    let input = push_input_accessor(&mut root, &mut bin, clip, frame_count);

    let mut channels = Vec::new();
    let mut samplers = Vec::new();
    let mut values: Vec<f32> = Vec::with_capacity(frame_count * 4);
    for track in &clip.tracks {
        let Some(name) = track.bone_name.as_deref() else {
            continue;
        };
        let &node = bone_nodes.get(name).ok_or_else(|| {
            eyre!(
                "clip track {} bound to bone {name}, which the body does not have",
                track.bone_index
            )
        })?;
        values.clear();
        values.extend(track.samples.iter().flat_map(|sample| sample.translation));
        push_sampler(
            &mut root,
            &mut bin,
            &mut channels,
            &mut samplers,
            input,
            node,
            Property::Translation,
            Type::Vec3,
            frame_count,
            &values,
        );

        values.clear();
        values.extend(track.samples.iter().flat_map(|sample| sample.rotation));
        push_sampler(
            &mut root,
            &mut bin,
            &mut channels,
            &mut samplers,
            input,
            node,
            Property::Rotation,
            Type::Vec4,
            frame_count,
            &values,
        );

        values.clear();
        values.extend(track.samples.iter().flat_map(|sample| sample.scale));
        push_sampler(
            &mut root,
            &mut bin,
            &mut channels,
            &mut samplers,
            input,
            node,
            Property::Scale,
            Type::Vec3,
            frame_count,
            &values,
        );
    }

    root.animations.push(Animation {
        extensions: None,
        extras: Extras::default(),
        channels,
        name: Some(clip_name.to_owned()),
        samplers,
    });
    root.buffers[0].byte_length = USize64(bin.len() as u64);

    pack_glb(&root, bin)
}

/// Emits one node per bone in name order and wires the sorted child lists.
fn push_bone_nodes<'a>(
    root: &mut Root,
    body_bones: &'a BoneTree,
) -> Result<BTreeMap<&'a str, Index<Node>>> {
    let mut bone_nodes = BTreeMap::new();
    for (name, bone) in &body_bones.bones {
        let translation = bone
            .transform
            .get_translation()
            .map(|value| value.0.to_array());
        let rotation = bone.transform.get_rotation().map(|value| {
            let quaternion = glam::Quat::from_mat3(&value.0);
            UnitQuaternion([quaternion.x, quaternion.y, quaternion.z, quaternion.w])
        });
        let scale = bone
            .transform
            .get_scale()
            .map(|value| [value, value, value]);

        let node = Index::push(
            &mut root.nodes,
            Node {
                camera: None,
                children: None,
                extensions: None,
                extras: Extras::default(),
                matrix: None,
                mesh: None,
                name: Some(name.clone()),
                rotation,
                scale,
                translation,
                skin: None,
                weights: None,
            },
        );
        bone_nodes.insert(name.as_str(), node);
    }

    let mut parented: BTreeSet<&str> = BTreeSet::new();
    for (name, bone) in &body_bones.bones {
        let parent = bone_nodes[name.as_str()];
        let mut children: Vec<&String> = bone.children.iter().collect();
        children.sort();
        let mut child_indices = Vec::with_capacity(children.len());
        for child in children {
            let Some(&child_index) = bone_nodes.get(child.as_str()) else {
                return Err(eyre!(
                    "bone {name} lists child {child}, which the bone tree does not contain"
                ));
            };
            child_indices.push(child_index);
            parented.insert(child.as_str());
        }
        if !child_indices.is_empty() {
            root.nodes[parent.value()].children = Some(child_indices);
        }
    }

    for name in body_bones.bones.keys() {
        if !parented.contains(name.as_str()) {
            root.scenes[0].nodes.push(bone_nodes[name.as_str()]);
        }
    }

    Ok(bone_nodes)
}

/// Appends the single root that carries the Creation-to-runtime rotation.
fn push_wrapper(root: &mut Root) {
    let children = std::mem::take(&mut root.scenes[0].nodes);
    let wrapper = Index::push(
        &mut root.nodes,
        Node {
            camera: None,
            children: Some(children),
            extensions: None,
            extras: Extras::default(),
            matrix: None,
            mesh: None,
            name: Some("Creation-to-glTF basis".to_owned()),
            rotation: Some(UnitQuaternion(
                shared::coordinates::CREATION_TO_RUNTIME_ROTATION,
            )),
            scale: None,
            translation: None,
            skin: None,
            weights: None,
        },
    );
    root.scenes[0].nodes.push(wrapper);
}

/// Writes the shared keyframe times: `[0, 1/fps, … (frames - 1)/fps]`.
fn push_input_accessor(
    root: &mut Root,
    bin: &mut Vec<u8>,
    clip: &AnimationClip,
    frame_count: usize,
) -> Index<Accessor> {
    let times: Vec<f32> = (0..frame_count)
        .map(|frame| frame as f32 / clip.frames_per_second)
        .collect();
    let last = times.last().copied().unwrap_or(0.0);
    push_accessor(
        root,
        bin,
        Type::Scalar,
        frame_count,
        &times,
        Some(Value::Array(vec![Value::from(0.0_f32)])),
        Some(Value::Array(vec![Value::from(last)])),
    )
}

#[allow(clippy::too_many_arguments)]
fn push_sampler(
    root: &mut Root,
    bin: &mut Vec<u8>,
    channels: &mut Vec<Channel>,
    samplers: &mut Vec<Sampler>,
    input: Index<Accessor>,
    node: Index<Node>,
    path: Property,
    type_: Type,
    count: usize,
    values: &[f32],
) {
    let output = push_accessor(root, bin, type_, count, values, None, None);
    let sampler = Index::push(
        samplers,
        Sampler {
            extensions: None,
            extras: Extras::default(),
            input,
            interpolation: Checked::Valid(Interpolation::Linear),
            output,
        },
    );
    channels.push(Channel {
        sampler,
        target: Target {
            extensions: None,
            extras: Extras::default(),
            node,
            path: Checked::Valid(path),
        },
        extensions: None,
        extras: Extras::default(),
    });
}

fn push_accessor(
    root: &mut Root,
    bin: &mut Vec<u8>,
    type_: Type,
    count: usize,
    values: &[f32],
    min: Option<Value>,
    max: Option<Value>,
) -> Index<Accessor> {
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let offset = bin.len();
    for value in values {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let length = bin.len() - offset;
    let view = Index::push(
        &mut root.buffer_views,
        View {
            buffer: Index::new(0),
            byte_length: USize64(length as u64),
            byte_offset: Some(USize64(offset as u64)),
            byte_stride: None,
            extensions: None,
            extras: Extras::default(),
            name: None,
            target: None,
        },
    );
    Index::push(
        &mut root.accessors,
        Accessor {
            buffer_view: Some(view),
            byte_offset: None,
            count: USize64(count as u64),
            component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
            extensions: None,
            extras: Extras::default(),
            type_: Checked::Valid(type_),
            min,
            max,
            name: None,
            normalized: false,
            sparse: None,
        },
    )
}

/// Packs JSON and BIN the way the vendor's `to_glb` does: both chunks 4-byte aligned, the JSON
/// chunk padded with spaces and the BIN chunk with zeros.
fn pack_glb(root: &Root, bin_data: Vec<u8>) -> Result<Vec<u8>> {
    let mut json = root
        .to_string()
        .wrap_err("failed to serialize clip glTF JSON")?
        .into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = bin_data;
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let total_len = 12usize
        .saturating_add(8)
        .saturating_add(json.len())
        .saturating_add(8)
        .saturating_add(bin.len());
    let mut glb = Vec::with_capacity(total_len);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_len as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    Ok(glb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_wormhole_nif::model::all::{BoneData, BoneTransform};
    use project_wormhole_shared::prelude::BSVec3;
    use std::path::{Path, PathBuf};

    use crate::animation::{TransformSample, TransformTrack, decode_clip_with_skeleton};

    #[test]
    fn exported_body_skin_supplies_the_exact_clip_tree() {
        let mut clip = fixture_clip();
        let bones = fixture_bones();
        bind_clip_to_bones(&mut clip, &bones.bones.keys().cloned().collect());
        let bytes = encode_clip_glb(&clip, &bones, "idle").unwrap();
        let parsed = gltf::Gltf::from_slice(&bytes).unwrap();
        let mut json = serde_json::to_value(parsed.document.into_json()).unwrap();
        json["skins"] = serde_json::json!([{"joints":[0,1,2]}]);
        json["meshes"] = serde_json::json!([{"primitives":[{"attributes":{"POSITION":1}}]}]);
        json["accessors"][1]["min"] = serde_json::json!([0, 0, 0]);
        json["accessors"][1]["max"] = serde_json::json!([1, 2, 3]);
        json["nodes"][0]["mesh"] = serde_json::json!(0);
        json["nodes"][0]["skin"] = serde_json::json!(0);
        let root: Root = serde_json::from_value(json).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body.glb");
        std::fs::write(&path, pack_glb(&root, parsed.blob.unwrap()).unwrap()).unwrap();
        assert_eq!(super::body_bones(&path).unwrap(), bones);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            super::body_bones(&path).unwrap(),
            bones,
            "rigid attachment trees do not require skin weights"
        );
    }

    #[test]
    fn additive_clips_require_a_base_pose_before_standalone_publication() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("clip.glb");
        std::fs::write(&output, b"keep").unwrap();
        let mut clip = fixture_clip();
        clip.blend_hint = 1;
        let error = write_clip_glb(&mut clip, &fixture_bones(), "idle", &output).unwrap_err();
        assert!(error.to_string().contains("needs an explicit base pose"));
        assert_eq!(std::fs::read(output).unwrap(), b"keep");
    }

    fn bone(transform: BoneTransform, children: &[&str]) -> BoneData {
        BoneData {
            transform,
            children: children.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn fixture_bones() -> BoneTree {
        let mut tree = BoneTree::new();
        tree.add_bone(
            "Root",
            bone(
                BoneTransform::new(
                    Some(BSVec3(glam::Vec3::new(1.0, 2.0, 3.0))),
                    None,
                    Some(2.0),
                ),
                &["ChildA", "ChildB"],
            ),
        )
        .expect("add root");
        tree.add_bone("ChildA", bone(BoneTransform::default(), &[]))
            .expect("add child a");
        tree.add_bone("ChildB", bone(BoneTransform::default(), &[]))
            .expect("add child b");
        tree
    }

    fn sample(frame: f32) -> TransformSample {
        TransformSample {
            translation: [frame, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
        }
    }

    fn fixture_clip() -> AnimationClip {
        AnimationClip {
            blend_hint: 0,
            name: String::new(),
            original_skeleton_name: "fixture".to_owned(),
            duration_seconds: 2.0 / 30.0,
            frame_count: 3,
            frames_per_second: 30.0,
            tracks: vec![
                TransformTrack {
                    bone_index: 0,
                    bone_name: Some("ChildA".to_owned()),
                    samples: vec![sample(0.0), sample(1.0), sample(2.0)],
                },
                TransformTrack {
                    bone_index: 1,
                    bone_name: Some("x_NPC Ghost".to_owned()),
                    samples: vec![sample(0.0), sample(1.0), sample(2.0)],
                },
            ],
        }
    }

    fn read_glb(path: &Path) -> (Root, Vec<u8>) {
        let bytes = std::fs::read(path).expect("read GLB");
        assert!(bytes.len() >= 20, "GLB is too short");
        assert_eq!(&bytes[..4], b"glTF", "GLB magic");
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        assert_eq!(&bytes[16..20], b"JSON", "JSON chunk type");
        let json = &bytes[20..20 + json_len];
        let view_start = 20 + json_len;
        let bin_len =
            u32::from_le_bytes(bytes[view_start..view_start + 4].try_into().unwrap()) as usize;
        assert_eq!(
            &bytes[view_start + 4..view_start + 8],
            b"BIN\0",
            "BIN chunk type"
        );
        let bin = &bytes[view_start + 8..view_start + 8 + bin_len];
        (
            Root::from_slice(json).expect("parse GLB JSON"),
            bin.to_vec(),
        )
    }

    fn node_named<'a>(nodes: &'a [Node], name: &str) -> (usize, &'a Node) {
        nodes
            .iter()
            .enumerate()
            .find(|(_, node)| node.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("node {name} is missing"))
    }

    fn parents(nodes: &[Node]) -> Vec<Option<usize>> {
        let mut parents = vec![None; nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            for child in node.children.as_deref().unwrap_or_default() {
                parents[child.value()] = Some(index);
            }
        }
        parents
    }

    fn accessor_values(root: &Root, bin: &[u8], index: usize) -> Vec<f32> {
        let accessor = &root.accessors[index];
        let view = &root.buffer_views[accessor
            .buffer_view
            .expect("accessor has a buffer view")
            .value()];
        let offset = view.byte_offset.map_or(0, |value| value.0 as usize)
            + accessor.byte_offset.map_or(0, |value| value.0 as usize);
        let components = match &accessor.type_ {
            Checked::Valid(Type::Scalar) => 1,
            Checked::Valid(Type::Vec2) => 2,
            Checked::Valid(Type::Vec3) => 3,
            Checked::Valid(Type::Vec4) => 4,
            _ => panic!("unexpected accessor type"),
        };
        let count = accessor.count.0 as usize;
        bin[offset..offset + count * components * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect()
    }

    #[test]
    fn writes_body_hierarchy_with_channels_for_bound_tracks_only() {
        let directory = tempfile::tempdir().expect("temp dir");
        let output = directory.path().join("fixture.glb");
        let mut clip = fixture_clip();
        let report =
            write_clip_glb(&mut clip, &fixture_bones(), "fixture", &output).expect("write clip");

        assert_eq!(report.exact, 1);
        assert!(report.aliased.is_empty());
        assert_eq!(report.skipped, vec!["x_NPC Ghost".to_owned()]);

        let (root, bin) = read_glb(&output);
        assert_eq!(root.scenes.len(), 1);
        assert_eq!(root.nodes.len(), 4, "three bones plus the wrapper");

        let wrapper_index = root.scenes[0].nodes[0].value();
        let wrapper = &root.nodes[wrapper_index];
        assert_eq!(wrapper.name.as_deref(), Some("Creation-to-glTF basis"));
        assert_eq!(
            wrapper.rotation.expect("wrapper rotation").0,
            shared::coordinates::CREATION_TO_RUNTIME_ROTATION,
        );

        for name in ["Root", "ChildA", "ChildB"] {
            let matching = root
                .nodes
                .iter()
                .filter(|node| node.name.as_deref() == Some(name))
                .count();
            assert_eq!(matching, 1, "bone {name} must appear exactly once");
        }
        for node in &root.nodes {
            assert!(
                !node.name.as_deref().unwrap_or_default().contains(':'),
                "node names must stay plain"
            );
        }

        assert_eq!(root.animations.len(), 1);
        let animation = &root.animations[0];
        assert_eq!(animation.name.as_deref(), Some("fixture"));
        assert_eq!(
            animation.channels.len(),
            3,
            "animated bone has three channels"
        );
        assert_eq!(animation.samplers.len(), 3);

        let (animated_index, _) = node_named(&root.nodes, "ChildA");
        let (root_index, _) = node_named(&root.nodes, "Root");
        let (unanimated_index, _) = node_named(&root.nodes, "ChildB");
        for channel in &animation.channels {
            assert_eq!(channel.target.node.value(), animated_index);
        }
        assert!(
            !animation
                .channels
                .iter()
                .any(|channel| channel.target.node.value() == unanimated_index),
            "an unanimated bone must not get a channel"
        );

        let parent_of = parents(&root.nodes);
        assert_eq!(parent_of[animated_index], Some(root_index));
        assert_eq!(parent_of[root_index], Some(wrapper_index));

        let input = animation.samplers[0].input.value();
        assert_eq!(root.accessors[input].count.0, 3);
        let times = accessor_values(&root, &bin, input);
        assert_eq!(times.len(), 3);
        assert!((times[0] - 0.0).abs() <= 1e-6);
        let max = root.accessors[input]
            .max
            .as_ref()
            .expect("input max")
            .as_array()
            .expect("array")[0]
            .as_f64()
            .expect("number");
        assert!((max - 2.0 / 30.0).abs() <= 1e-6, "input max {max}");

        for sampler in &animation.samplers {
            assert_eq!(root.accessors[sampler.output.value()].count.0, 3);
        }

        let second = directory.path().join("fixture-again.glb");
        write_clip_glb(&mut fixture_clip(), &fixture_bones(), "fixture", &second)
            .expect("write the same clip again");
        assert_eq!(
            std::fs::read(&output).expect("read first GLB"),
            std::fs::read(&second).expect("read second GLB"),
            "identical input must produce identical bytes"
        );
    }

    #[test]
    fn exact_track_wins_alias_and_channels_are_unique_per_node_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let output = directory.path().join("priority.glb");
        let mut clip = fixture_clip();
        clip.tracks = vec![
            TransformTrack {
                bone_index: 0,
                bone_name: Some("childa".to_owned()),
                samples: vec![sample(9.0); 3],
            },
            TransformTrack {
                bone_index: 1,
                bone_name: Some("ChildA".to_owned()),
                samples: vec![sample(2.0); 3],
            },
            TransformTrack {
                bone_index: 2,
                bone_name: Some("ChildB".to_owned()),
                samples: vec![sample(3.0); 3],
            },
        ];

        let report =
            write_clip_glb(&mut clip, &fixture_bones(), "priority", &output).expect("write clip");
        assert_eq!(report.exact, 2);
        assert_eq!(report.skipped, vec!["childa"]);

        let (root, bin) = read_glb(&output);
        let animation = &root.animations[0];
        assert_eq!(animation.channels.len(), 6, "two nodes each emit TRS");
        let targets: BTreeSet<_> = animation
            .channels
            .iter()
            .map(|channel| {
                (
                    channel.target.node.value(),
                    format!("{:?}", channel.target.path),
                )
            })
            .collect();
        assert_eq!(
            targets.len(),
            animation.channels.len(),
            "node/path targets are unique"
        );

        let (child_a, _) = node_named(&root.nodes, "ChildA");
        let translation = animation
            .channels
            .iter()
            .find(|channel| {
                channel.target.node.value() == child_a
                    && channel.target.path == Checked::Valid(Property::Translation)
            })
            .expect("ChildA translation channel");
        let sampler = &animation.samplers[translation.sampler.value()];
        assert_eq!(accessor_values(&root, &bin, sampler.output.value())[0], 2.0);
    }

    fn animation_root() -> PathBuf {
        std::env::var_os("MUDCRAB_ANIMATION_DIR")
            .map(PathBuf::from)
            .expect("MUDCRAB_ANIMATION_DIR must point at the extracted character directory")
    }

    fn actor_nif_root() -> PathBuf {
        std::env::var_os("MUDCRAB_ACTOR_NIF_DIR")
            .map(PathBuf::from)
            .expect("MUDCRAB_ACTOR_NIF_DIR must point at the extracted actors directory")
    }

    fn body_bones(actor_root: &Path, body_name: &str) -> BoneTree {
        use project_wormhole_nif::nif_file::{NifFile, nif_to_model};

        let character = actor_root.join("character/character assets");
        let body = NifFile::open(
            character
                .join(body_name)
                .to_str()
                .expect("body path is UTF-8"),
        )
        .expect("parse body NIF");
        let skeleton = NifFile::open(
            character
                .join("skeleton.nif")
                .to_str()
                .expect("skeleton path is UTF-8"),
        )
        .expect("parse skeleton NIF");
        let model = nif_to_model(&body, Some(&skeleton)).expect("build body model");
        model
            .skeletal_meshes
            .first()
            .expect("body has a skeletal mesh")
            .bones
            .clone()
    }

    fn assert_descendant(parent_of: &[Option<usize>], wrapper: usize, node: usize, label: &str) {
        let mut current = Some(node);
        while let Some(index) = current {
            if index == wrapper {
                return;
            }
            current = parent_of[index];
        }
        panic!("{label} node {node} is not a descendant of the wrapper");
    }

    #[test]
    #[ignore = "requires MUDCRAB_ANIMATION_DIR and MUDCRAB_ACTOR_NIF_DIR"]
    fn retail_mt_idle_writes_channel_per_bound_bone() {
        let animation_root = animation_root();
        let actor_root = actor_nif_root();
        let skeleton_bytes = std::fs::read(animation_root.join("character assets/skeleton.hkx"))
            .expect("read skeleton.hkx");

        let male_bytes = std::fs::read(animation_root.join("animations/male/mt_idle.hkx"))
            .expect("read male mt_idle.hkx");
        let mut male =
            decode_clip_with_skeleton(&male_bytes, &skeleton_bytes).expect("decode male clip");
        let male_bones = body_bones(&actor_root, "malebody_1.nif");
        assert_eq!(male_bones.bones.len(), 99);

        let directory = tempfile::tempdir().expect("temp dir");
        let male_output = directory.path().join("mt_idle.glb");
        let report = write_clip_glb(&mut male, &male_bones, "mt_idle", &male_output)
            .expect("write male clip");

        assert_eq!(report.exact, 93);
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

        let (root, bin) = read_glb(&male_output);
        assert_eq!(root.nodes.len(), 100, "99 bones plus the wrapper");
        for name in male_bones.bones.keys() {
            let matching = root
                .nodes
                .iter()
                .filter(|node| node.name.as_deref() == Some(name.as_str()))
                .count();
            assert_eq!(matching, 1, "bone {name} must appear exactly once");
        }

        let wrapper = root.scenes[0].nodes[0].value();
        assert_eq!(
            root.nodes[wrapper].name.as_deref(),
            Some("Creation-to-glTF basis")
        );
        assert_eq!(
            root.nodes[wrapper].rotation.expect("wrapper rotation").0,
            shared::coordinates::CREATION_TO_RUNTIME_ROTATION,
        );

        let animation = &root.animations[0];
        assert_eq!(animation.name.as_deref(), Some("mt_idle"));
        assert_eq!(
            animation.channels.len(),
            288,
            "96 bound tracks x 3 channels"
        );
        assert_eq!(animation.samplers.len(), 288);

        let parent_of = parents(&root.nodes);
        for channel in &animation.channels {
            assert_descendant(
                &parent_of,
                wrapper,
                channel.target.node.value(),
                "channel target",
            );
        }

        let input = animation.samplers[0].input.value();
        let times = accessor_values(&root, &bin, input);
        let max_time = times.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            (max_time - 9.133_333).abs() <= 1e-4,
            "male max input time {max_time}"
        );
        assert_eq!(times.len(), 275);

        let female_bytes = std::fs::read(animation_root.join("animations/female/mt_idle.hkx"))
            .expect("read female mt_idle.hkx");
        let mut female =
            decode_clip_with_skeleton(&female_bytes, &skeleton_bytes).expect("decode female clip");
        let female_bones = body_bones(&actor_root, "femalebody_1.nif");
        let female_output = directory.path().join("female_mt_idle.glb");
        let female_report = write_clip_glb(&mut female, &female_bones, "mt_idle", &female_output)
            .expect("write female clip");
        let (female_root, female_bin) = read_glb(&female_output);
        let female_animation = &female_root.animations[0];
        let female_input = female_animation.samplers[0].input.value();
        let female_times = accessor_values(&female_root, &female_bin, female_input);
        let female_max = female_times.iter().copied().fold(f32::MIN, f32::max);
        println!(
            "female mt_idle: {} channels, duration {:.6}s, {} bones",
            female_animation.channels.len(),
            female_max,
            female_bones.bones.len()
        );
        assert_eq!(female_animation.channels.len(), female_report.bound() * 3);
    }
}
