#![allow(unused_imports)]
use std::{io::Write, path::Path};

use crate::dev::*;

const OUT_DIR: &str = "./out/";

#[test]
#[ignore = "manual Fallout 4 fixture test; requires local Deathclaw assets"]
pub fn dev_test() {
    use log::*;
    use simplelog::*;
    use std::fs::File;

    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Debug,
            Config::default(),
            File::create("./nif.log").unwrap(),
        ),
    ])
    .unwrap();

    const PATH: &str = ".\\Meshes\\Actors\\Deathclaw\\Deathclaw.nif";
    const SKEL: &str = ".\\Meshes\\Actors\\Deathclaw\\CharacterAssets\\skeleton.nif";

    let mut file = NifFileV3::open(PATH).unwrap();

    debug!("{:#?}", file);

    for block in &file.raw_blocks {
        match block {
            NifBlock::BSShaderTextureSet(tset) => {
                debug!("{:#?}", tset);
            }
            _ => {}
        }
    }

    //debug!("{:#?}", file.header);

    let skel = NifFileV3::open(SKEL).unwrap();

    let (gltf, bin_data) = file.to_gltf("Deathclaw".to_string(), Some(&skel));

    let path = Path::new(OUT_DIR).join("deathclaw.gltf");
    let mut file = File::create(&path).unwrap();
    let mut bin = File::create(path.with_extension("bin")).unwrap();

    file.write_all(gltf.to_string_pretty().unwrap().as_bytes())
        .unwrap();
    bin.write_all(&bin_data).unwrap();
}

/*
//#[test]
pub fn test_all_in_archive() {
    use ba2::prelude::*;
    use simplelog::*;
    use std::fs::File;


    CombinedLogger::init(
        vec![
            TermLogger::new(LevelFilter::Info, Config::default(), TerminalMode::Mixed, ColorChoice::Auto),
            WriteLogger::new(LevelFilter::Debug, Config::default(), File::create("./nif.log").unwrap()),
        ]
    ).unwrap();

    if let Ok(mut ba2) = GeneralArchive::open("D:\\SteamLibrary\\steamapps\\common\\Fallout 4\\Data\\Fallout4 - Meshes.ba2") {

        let names = ba2.entries.clone();

        for (name, _entry) in names {
            if name.ends_with(".nif") {
                if let Ok(data) = &ba2.read_file(name.as_str()) {
                    if let Ok((_data, nif)) = NifFile::parse(&data) {

                        let full = OUT_DIR.to_string() + &name.replace(".nif", ".gltf");

                        let path = Path::new(&full);


                        info!("Writing \"{}\"", path.to_string_lossy().to_string());

                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

                        if let Ok(mut file) = std::fs::File::create(path.to_string_lossy().to_string()) {
                            if let Ok(model) = Model::try_from(nif) {
                                let (root, bin_data) = model
                                    .to_gltf(path.file_stem().unwrap().to_string_lossy().to_string())
                                    .unwrap();

                                let mut bin = std::fs::File::create(path.with_extension("bin")).unwrap();

                                file.write_all(root.to_string_pretty().unwrap().as_bytes()).unwrap();
                                bin.write_all(&bin_data).unwrap();
                            } else {
                                error!("Failed to convert \"{}\"", name);
                            }
                        } else {
                            error!("Failed to write \"{}\"", name);
                        }




                    } else {
                        error!("Failed to parse \"{}\"", name);
                    }
                } else {
                    debug!("Failed to read \"{}\"", name);
                }

            } else {
                debug!("Skipping \"{}\"", name);
            }
        }

    } else {
        panic!("Failed to open archive");
    }

}
*/

mod multi_bound {
    use project_wormhole_ba2::dev::MaxRef;
    use project_wormhole_shared::glam::{self, Mat3, Vec3};

    use crate::dev::*;

    /// The compact `NiAVObject` Skyrim writes on scene nodes and shapes.
    fn av_object_bytes(name: u32) -> Vec<u8> {
        av_object_bytes_with_transform(
            name,
            [0.0; 3],
            1.0,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        )
    }

    fn av_object_bytes_with_transform(
        name: u32,
        translation: [f32; 3],
        scale: f32,
        rotation: [f32; 9],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, name);
        push_u32(&mut bytes, u32::MAX); // extra data
        push_u32(&mut bytes, u32::MAX); // controller
        push_u32(&mut bytes, 0); // flags
        for value in translation {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in rotation {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&scale.to_le_bytes());
        push_u32(&mut bytes, u32::MAX); // collision object
        bytes
    }

    /// An `NiNode` with one child and an empty effect list.
    fn node_bytes(name: u32, child: u32) -> Vec<u8> {
        node_bytes_with_transform(
            name,
            child,
            [0.0; 3],
            1.0,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        )
    }

    fn node_bytes_with_transform(
        name: u32,
        child: u32,
        translation: [f32; 3],
        scale: f32,
        rotation: [f32; 9],
    ) -> Vec<u8> {
        let mut bytes = av_object_bytes_with_transform(name, translation, scale, rotation);
        push_u32(&mut bytes, 1); // children
        push_u32(&mut bytes, child);
        push_u32(&mut bytes, 0); // effects
        bytes
    }

    fn multi_bound_node_bytes(name: u32, child: u32, bound: u32, culling: Option<u32>) -> Vec<u8> {
        let mut bytes = node_bytes(name, child);
        push_u32(&mut bytes, bound);
        if let Some(culling) = culling {
            push_u32(&mut bytes, culling);
        }
        bytes
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn parse_block(bytes: &[u8], block_type: &str) -> NifBlock {
        let (_, block) = NifBlock::parse(bytes, block_type.to_string())
            .unwrap_or_else(|error| panic!("{block_type} did not parse: {error:?}"));
        block
    }

    fn nif_file(blocks: Vec<NifBlock>, strings: &[&str]) -> NifFile {
        let strings = strings
            .iter()
            .map(|value| SizedString32((*value).to_owned()))
            .collect::<Vec<_>>();
        let block_count = blocks.len() as u32;
        NifFile {
            header: NifHeader {
                file_desc: StringN {
                    value: String::new(),
                },
                nif_version: NifFileVersion(0x1402_0007),
                endian_type: Endianess::Little,
                user_version: 12,
                block_count,
                bethesda_version: 100,
                author: None,
                process_script: None,
                export_script: None,
                max_filepath: None,
                block_types: Vec::new(),
                block_type_index: vec![0; blocks.len()],
                block_size_index: vec![0; blocks.len()],
                string_count: strings.len() as u32,
                string_max_size: 0,
                strings,
                groups: Vec::new(),
            },
            blocks,
        }
    }

    #[test]
    fn parses_multi_bound_data_reference() {
        let data = 3u32.to_le_bytes();
        let (rest, bound) = BSMultiBound::parse(&data).unwrap();
        assert!(rest.is_empty());
        assert_eq!(bound.data, MaxRef(Some(3)));

        let null = u32::MAX.to_le_bytes();
        let (_, empty) = BSMultiBound::parse(&null).unwrap();
        assert_eq!(empty.data, MaxRef(None));

        assert!(BSMultiBound::parse(&[]).is_err());
    }

    #[test]
    fn parses_multi_bound_volumes() {
        let mut aabb = Vec::new();
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            aabb.extend_from_slice(&value.to_le_bytes());
        }
        let (rest, parsed) = BSMultiBoundAABB::parse(&aabb).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed.position.0, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(parsed.extent.0, Vec3::new(4.0, 5.0, 6.0));
        assert!(BSMultiBoundAABB::parse(&aabb[..20]).is_err());

        let mut obb = Vec::new();
        for value in 0..15 {
            obb.extend_from_slice(&(value as f32).to_le_bytes());
        }
        let (rest, parsed) = BSMultiBoundOBB::parse(&obb).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed.center.0, Vec3::new(0.0, 1.0, 2.0));
        assert_eq!(parsed.size.0, Vec3::new(3.0, 4.0, 5.0));
        // File matrices are row-major; the shared parser stores the transpose.
        assert_eq!(
            parsed.rotation.0,
            Mat3::from_cols_array(&[6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0]).transpose()
        );
        assert!(BSMultiBoundOBB::parse(&obb[..56]).is_err());
    }

    #[test]
    fn parses_multi_bound_node_with_and_without_a_culling_mode() {
        let bytes = multi_bound_node_bytes(1, 2, 3, Some(4));
        let (rest, node) = BSMultiBoundNode::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(node.node.name(), 1);
        assert_eq!(node.node.children, vec![2]);
        assert_eq!(node.bound, MaxRef(Some(3)));
        assert_eq!(node.culling_mode, Some(4));

        let bytes = multi_bound_node_bytes(1, 2, 3, None);
        let (rest, node) = BSMultiBoundNode::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(node.bound, MaxRef(Some(3)));
        assert_eq!(node.culling_mode, None);
    }

    #[test]
    fn dispatches_multi_bound_blocks_instead_of_falling_back() {
        let bytes = multi_bound_node_bytes(0, 1, 2, Some(3));
        assert!(matches!(
            parse_block(&bytes, "BSMultiBoundNode"),
            NifBlock::BSMultiBoundNode(_)
        ));
        assert!(matches!(
            parse_block(&[0u8; 4], "BSMultiBound"),
            NifBlock::BSMultiBound(_)
        ));
        assert!(matches!(
            parse_block(&[0; 24], "BSMultiBoundAABB"),
            NifBlock::BSMultiBoundAABB(_)
        ));
        assert!(matches!(
            parse_block(&[0; 60], "BSMultiBoundOBB"),
            NifBlock::BSMultiBoundOBB(_)
        ));
    }

    /// An empty `BSTriShape` payload: NiAVObject, bound, skin/shader/alpha
    /// references, vertex descriptor, zero counts and zero particle size.
    fn empty_shape_bytes() -> Vec<u8> {
        shape_bytes(0)
    }

    /// The same empty payload, declaring `num_triangles` triangles without
    /// storing geometry: a zero data size means there is nothing to read.
    fn shape_bytes(num_triangles: u16) -> Vec<u8> {
        shape_bytes_with_transform(num_triangles, [0.0; 3])
    }

    /// A `BSTriShape` whose own `NiAVObject` transform carries `translation`.
    fn shape_bytes_with_transform(num_triangles: u16, translation: [f32; 3]) -> Vec<u8> {
        let mut bytes = av_object_bytes_with_transform(
            u32::MAX,
            translation,
            1.0,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        );
        for value in [0.0f32, 0.0, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes()); // bounding sphere
        }
        push_u32(&mut bytes, u32::MAX); // skin
        push_u32(&mut bytes, u32::MAX); // shader property
        push_u32(&mut bytes, u32::MAX); // alpha property
        bytes.extend_from_slice(&0u64.to_le_bytes()); // vertex descriptor
        bytes.extend_from_slice(&num_triangles.to_le_bytes()); // triangles
        bytes.extend_from_slice(&0u16.to_le_bytes()); // vertices
        push_u32(&mut bytes, 0); // data size
        push_u32(&mut bytes, 0); // particle size (count of u16 values)
        bytes
    }

    fn sub_index_bytes(
        num_triangles: u16,
        declared_segments: u32,
        segments: &[(u8, u32, u32)],
    ) -> Vec<u8> {
        let mut bytes = shape_bytes(num_triangles);
        push_u32(&mut bytes, declared_segments);
        for (flag, value, primitives) in segments {
            bytes.push(*flag);
            push_u32(&mut bytes, *value);
            push_u32(&mut bytes, *primitives);
        }
        bytes
    }

    #[test]
    fn parses_the_measured_sub_index_segment_table() {
        let bytes = sub_index_bytes(42, 2, &[(0, 3, 12), (7, 4096, 30)]);
        let (rest, shape) = BSSubIndexTriShape::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(shape.num_segments, 2);
        assert_eq!(shape.segments.len(), 2);
        assert_eq!(shape.segments[0].flag, 0);
        assert_eq!(shape.segments[0].value, 3);
        assert_eq!(shape.segments[0].num_primitives, 12);
        assert_eq!(shape.segments[1].flag, 7);
        assert_eq!(shape.segments[1].value, 4096);
        assert_eq!(shape.segments[1].num_primitives, 30);
        assert_eq!(shape.bs_tri_shape.num_triangles, 42);
        assert_eq!(shape.bs_tri_shape.num_vertices, 0);
    }

    #[test]
    fn sub_index_segments_follow_the_particle_payload() {
        let mut bytes = sub_index_bytes(42, 1, &[(7, 4096, 42)]);
        let particle_offset = shape_bytes(42).len() - 4;
        bytes.splice(
            particle_offset..particle_offset + 4,
            [3, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
        );
        let (rest, shape) = BSSubIndexTriShape::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(shape.num_segments, 1);
        assert_eq!(shape.segments.len(), 1);
        assert_eq!(shape.segments[0].flag, 7);
        assert_eq!(shape.segments[0].value, 4096);
        assert_eq!(shape.segments[0].num_primitives, 42);
    }

    #[test]
    fn rejects_a_truncated_or_implausible_segment_table() {
        // Two segments declared, one record present.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(1, 2, &[(0, 0, 1)])).is_err());

        // A segment count larger than the block can hold must not allocate.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(0, 1_000_000, &[])).is_err());

        // The segments partition the shape's triangles, so their primitive
        // counts have to add up to it.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(42, 1, &[(0, 0, 41)])).is_err());
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(42, 0, &[])).is_err());
        let partitioned = sub_index_bytes(42, 2, &[(0, 0, 12), (0, 12, 30)]);
        let (rest, shape) = BSSubIndexTriShape::parse(&partitioned).unwrap();
        assert!(rest.is_empty());
        assert_eq!(shape.segments.len(), 2);
    }

    #[test]
    fn consumes_the_trailing_word_of_a_lod_shape() {
        // A `.btr` shape block is its geometry payload plus one zero word.
        let mut bytes = empty_shape_bytes();
        push_u32(&mut bytes, 0);
        let (rest, block) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert!(matches!(block, NifBlock::BSTriShape(_)));
        assert!(rest.is_empty());

        // An ordinary shape ends at its payload.
        let bytes = empty_shape_bytes();
        let (rest, _) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert!(rest.is_empty());

        // Anything else is left for the leftover warning.
        let mut bytes = empty_shape_bytes();
        push_u32(&mut bytes, 5);
        let (rest, _) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert_eq!(rest, &5u32.to_le_bytes());
    }

    #[test]
    fn distant_lod_vertex_colours_stay_inside_the_container() {
        let nif = nif_file(
            vec![
                parse_block(&node_bytes(0, 1), "NiNode"),
                parse_block(
                    &multi_bound_node_bytes(1, 2, 5, Some(3)),
                    "BSMultiBoundNode",
                ),
                parse_block(&shape_bytes(0), "BSTriShape"),
                parse_block(&node_bytes(3, 4), "NiNode"),
                parse_block(&shape_bytes(0), "BSTriShape"),
                parse_block(&[0u8; 4], "BSMultiBound"),
            ],
            &["root"],
        );
        // Only the container and its subtree keep vertex colours: the root node
        // above it and the second node's shape do not.
        assert_eq!(
            crate::nif_file::distant_lod_subtree_blocks(&nif),
            vec![false, true, true, false, false, false]
        );
    }

    #[test]
    fn multi_bound_nodes_behave_as_nodes() {
        let root = parse_block(
            &multi_bound_node_bytes(0, 1, 2, Some(3)),
            "BSMultiBoundNode",
        );
        let child = parse_block(
            &multi_bound_node_bytes(1, u32::MAX, 2, Some(3)),
            "BSMultiBoundNode",
        );
        assert!(root.as_node().unwrap().children == vec![1]);

        let mut nif = nif_file(vec![root, child], &["root", "child"]);
        assert_eq!(nif.get_node_pos("root"), Some(0));
        assert_eq!(nif.get_node_pos("child"), Some(1));
        assert_eq!(nif.get_nodes().len(), 2);

        // The root's child must reach the static scene as an ordinary node.
        let model = nif_to_static_model(&nif).unwrap();
        assert_eq!(model.static_nodes.len(), 2);
        let root_node = model
            .static_nodes
            .iter()
            .find(|node| node.name.as_deref() == Some("root"))
            .unwrap();
        assert_eq!(root_node.children, vec![1]);
        assert!(root_node.mesh.is_none());
    }

    fn ordered_node_bytes(name: u32, child: u32) -> Vec<u8> {
        ordered_node_bytes_with_transform(
            name,
            child,
            [0.0; 3],
            1.0,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        )
    }

    fn ordered_node_bytes_with_transform(
        name: u32,
        child: u32,
        translation: [f32; 3],
        scale: f32,
        rotation: [f32; 9],
    ) -> Vec<u8> {
        let mut bytes = node_bytes_with_transform(name, child, translation, scale, rotation);
        for value in [1.0f32, 2.0, 3.0, 4.0] {
            bytes.extend_from_slice(&value.to_le_bytes()); // alpha sort bound
        }
        bytes.push(1); // static bound
        bytes
    }

    fn billboard_node_bytes(name: u32, child: u32) -> Vec<u8> {
        let mut bytes = node_bytes(name, child);
        bytes.extend_from_slice(&3u16.to_le_bytes()); // billboard mode
        bytes
    }

    fn value_node_bytes(name: u32, child: u32) -> Vec<u8> {
        let mut bytes = node_bytes(name, child);
        bytes.extend_from_slice(&(-7i32).to_le_bytes()); // value
        bytes.push(2); // value node flags
        bytes
    }

    #[test]
    fn parses_ordered_billboard_and_value_nodes_with_their_trailing_fields() {
        let bytes = ordered_node_bytes(1, 2);
        let (rest, block) = NifBlock::parse(&bytes, "BSOrderedNode".to_string()).unwrap();
        assert!(rest.is_empty());
        let NifBlock::BSOrderedNode(ordered) = &block else {
            panic!("expected BSOrderedNode, got {block:?}");
        };
        assert_eq!(ordered.alpha_sort_bound, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ordered.static_bound, 1);
        assert_eq!(block.as_node().unwrap().children, vec![2]);
        for end in node_bytes(1, 2).len()..bytes.len() {
            assert!(NifBlock::parse(&bytes[..end], "BSOrderedNode".to_string()).is_err());
        }

        let bytes = billboard_node_bytes(1, 2);
        let (rest, block) = NifBlock::parse(&bytes, "NiBillboardNode".to_string()).unwrap();
        assert!(rest.is_empty());
        let NifBlock::NiBillboardNode(billboard) = &block else {
            panic!("expected NiBillboardNode, got {block:?}");
        };
        assert_eq!(billboard.billboard_mode, 3);
        assert_eq!(block.as_node().unwrap().children, vec![2]);
        for end in node_bytes(1, 2).len()..bytes.len() {
            assert!(NifBlock::parse(&bytes[..end], "NiBillboardNode".to_string()).is_err());
        }

        let bytes = value_node_bytes(1, 2);
        let (rest, block) = NifBlock::parse(&bytes, "BSValueNode".to_string()).unwrap();
        assert!(rest.is_empty());
        let NifBlock::BSValueNode(value) = &block else {
            panic!("expected BSValueNode, got {block:?}");
        };
        assert_eq!(value.value, -7);
        assert_eq!(value.value_node_flags, 2);
        assert_eq!(block.as_node().unwrap().children, vec![2]);
        for end in node_bytes(1, 2).len()..bytes.len() {
            assert!(NifBlock::parse(&bytes[..end], "BSValueNode".to_string()).is_err());
        }
    }

    #[test]
    fn shapes_under_ordered_billboard_and_value_nodes_keep_their_parents() {
        // root -> ordered -> billboard -> value -> shape, as in the forge's fire and glow.
        let quarter_turn = [0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let nif = nif_file(
            vec![
                // Skyrim places a model by replacing the root block's transform, and the
                // converter publishes the root without one. Keep it identity here so the
                // expected placement below never encodes a transform the model drops.
                parse_block(
                    &node_bytes_with_transform(0, 1, [0.0; 3], 1.0, identity),
                    "NiNode",
                ),
                parse_block(
                    &ordered_node_bytes_with_transform(1, 2, [10.0, 20.0, 30.0], 2.0, quarter_turn),
                    "BSOrderedNode",
                ),
                {
                    let mut bytes = node_bytes_with_transform(
                        2,
                        3,
                        [0.0, 3.0, 0.0],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                    );
                    bytes.extend_from_slice(&3u16.to_le_bytes());
                    parse_block(&bytes, "NiBillboardNode")
                },
                {
                    let mut bytes = node_bytes_with_transform(
                        3,
                        4,
                        [0.0, 0.0, 4.0],
                        1.0,
                        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                    );
                    bytes.extend_from_slice(&(-7i32).to_le_bytes());
                    bytes.push(2);
                    parse_block(&bytes, "BSValueNode")
                },
                parse_block(&shape_bytes(0), "BSTriShape"),
            ],
            &["root", "ordered", "billboard", "value"],
        );
        let model = nif_to_static_model(&nif).unwrap();
        let children = |block: u32| {
            model
                .static_nodes
                .iter()
                .find(|node| node.block_index == block)
                .unwrap_or_else(|| panic!("block {block} is not in the static scene"))
                .children
                .clone()
        };
        assert_eq!(children(0), vec![1]);
        assert_eq!(children(1), vec![2]);
        assert_eq!(children(2), vec![3]);
        assert_eq!(children(3), vec![4]);

        let (gltf, _) = model
            .to_gltf("ordered-node-regression".to_string())
            .unwrap();
        let node_index = |name: &str| {
            gltf.nodes
                .iter()
                .position(|node| node.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing from glTF export"))
        };
        let root_index = node_index("root");
        let ordered_index = node_index("ordered");
        let billboard_index = node_index("billboard");
        let value_index = node_index("value");
        let shape_index = gltf.nodes[value_index].children.as_ref().unwrap()[0].value();
        assert_eq!(
            gltf.nodes[root_index].children.as_ref().unwrap()[0].value(),
            ordered_index
        );
        assert_eq!(
            gltf.nodes[ordered_index].children.as_ref().unwrap()[0].value(),
            billboard_index
        );
        assert_eq!(
            gltf.nodes[billboard_index].children.as_ref().unwrap()[0].value(),
            value_index
        );
        let local = |index: usize| {
            let node = &gltf.nodes[index];
            glam::Mat4::from_scale_rotation_translation(
                node.scale
                    .as_ref()
                    .map(|scale| glam::Vec3::from_array(*scale))
                    .unwrap_or(glam::Vec3::ONE),
                node.rotation
                    .as_ref()
                    .map(|rotation| glam::Quat::from_array(rotation.0))
                    .unwrap_or(glam::Quat::IDENTITY),
                node.translation
                    .as_ref()
                    .map(|translation| glam::Vec3::from_array(*translation))
                    .unwrap_or(glam::Vec3::ZERO),
            )
        };
        let inherited = local(root_index)
            * local(ordered_index)
            * local(billboard_index)
            * local(value_index)
            * local(shape_index);
        // Shape at the origin through value (0, 0, 4) and billboard (0, 3, 0), then the ordered
        // node's quarter turn, double scale and (10, 20, 30) placement: (4, 20, 38).
        let placement = inherited.transform_point3(glam::Vec3::ZERO);
        assert!((placement - glam::Vec3::new(4.0, 20.0, 38.0)).length() < 1.0e-4);
    }

    /// The converter publishes an identity root, because a placed reference replaces the model
    /// root's transform; that drop is `clear_root_transform` in `crates/converter/src/mesh.rs`,
    /// not this crate. The model layer must keep the authored transform, so this test and the
    /// converter's `the_root_nodes_own_transform_moves_neither_render_nodes_nor_collision` pin
    /// the split: if either layer starts doing the other's job, one of them fails.
    #[test]
    fn the_model_layer_keeps_the_authored_root_transform() {
        let quarter_turn = [0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let nif = nif_file(
            vec![
                parse_block(
                    &node_bytes_with_transform(0, 1, [10.0, 20.0, 30.0], 2.0, quarter_turn),
                    "NiNode",
                ),
                // An empty shape keeps its node without a mesh, so the placement below is the
                // shape node's own transform composed under the root.
                parse_block(
                    &shape_bytes_with_transform(0, [1.0, 0.0, 0.0]),
                    "BSTriShape",
                ),
            ],
            &["root"],
        );

        let model = nif_to_static_model(&nif).unwrap();
        let root = model
            .static_nodes
            .iter()
            .find(|node| node.block_index == 0)
            .unwrap();
        assert_eq!(root.translation, Vec3::new(10.0, 20.0, 30.0));
        assert_eq!(root.scale, 2.0);
        assert_ne!(
            root.rotation,
            Mat3::IDENTITY,
            "the authored root rotation is kept"
        );

        let (gltf, _) = model.to_gltf("root-kept".to_string()).unwrap();
        let root_index = gltf
            .nodes
            .iter()
            .position(|node| node.name.as_deref() == Some("root"))
            .unwrap();
        let root_node = &gltf.nodes[root_index];
        assert_eq!(root_node.translation, Some([10.0, 20.0, 30.0]));
        assert_eq!(root_node.scale, Some([2.0, 2.0, 2.0]));

        let local = |index: usize| {
            let node = &gltf.nodes[index];
            glam::Mat4::from_scale_rotation_translation(
                node.scale
                    .as_ref()
                    .map(|scale| glam::Vec3::from_array(*scale))
                    .unwrap_or(glam::Vec3::ONE),
                node.rotation
                    .as_ref()
                    .map(|rotation| glam::Quat::from_array(rotation.0))
                    .unwrap_or(glam::Quat::IDENTITY),
                node.translation
                    .as_ref()
                    .map(|translation| glam::Vec3::from_array(*translation))
                    .unwrap_or(glam::Vec3::ZERO),
            )
        };
        let shape_index = gltf.nodes[root_index].children.as_ref().unwrap()[0].value();
        let placement = (local(root_index) * local(shape_index)).transform_point3(glam::Vec3::ZERO);
        // (1, 0, 0) turns to (0, 1, 0), doubles to (0, 2, 0), then translates to (10, 22, 30).
        assert!((placement - glam::Vec3::new(10.0, 22.0, 30.0)).length() < 1.0e-4);
    }
}

#[test]
#[ignore = "requires retail Skyrim actor NIFs from MUDCRAB_ACTOR_NIF_DIR"]
fn retail_male_body_retains_legacy_skin() {
    let directory = match std::env::var("MUDCRAB_ACTOR_NIF_DIR") {
        Ok(directory) => directory,
        Err(_) => {
            eprintln!("MUDCRAB_ACTOR_NIF_DIR is not set; skipping retail actor check");
            return;
        }
    };
    let path = format!("{directory}/character/character assets/malebody_1.nif");
    let bytes = std::fs::read(&path).expect("read malebody_1.nif");
    let (_, nif) = NifFile::parse(&bytes).expect("parse malebody_1.nif");
    assert_eq!(nif.blocks.len(), nif.header.block_count as usize);

    let (instance_index, instance) = nif
        .blocks
        .iter()
        .enumerate()
        .find_map(|(index, block)| match block {
            NifBlock::BsDismemberSkinInstance(instance) => Some((index, instance)),
            _ => None,
        })
        .expect("malebody_1.nif has no BSDismemberSkinInstance");
    let payload = &instance.skin_instance;
    assert!(!payload.bones.is_empty(), "skin instance has no bones");

    let data = match nif.blocks.get(payload.data as usize) {
        Some(NifBlock::NiSkinData(data)) => data,
        other => panic!("skin instance data reference is not NiSkinData: {other:?}"),
    };
    assert_eq!(
        data.bone_count as usize,
        payload.bones.len(),
        "NiSkinData bone count differs from the instance"
    );

    let partition = match nif.blocks.get(payload.skin_partition as usize) {
        Some(NifBlock::NiSkinPartition(partition)) => partition,
        other => panic!("skin partition reference is not NiSkinPartition: {other:?}"),
    };
    assert!(
        !partition.bone_palette.is_empty(),
        "partition palette is empty"
    );
    for bone in &partition.bone_palette {
        assert!(
            usize::from(*bone) < payload.bones.len(),
            "palette entry {bone} is outside the instance bone list"
        );
    }
    assert_eq!(
        partition.weights.len(),
        partition.vertex_data.len(),
        "one weight row per vertex was not retained"
    );

    let mut max_translation = 0.0f32;
    for bone in &data.bones {
        let translation = bone.skin_transform.translation.0;
        assert!(
            translation.is_finite(),
            "non-finite inverse bind translation"
        );
        assert!(
            translation.x.abs() <= 1000.0
                && translation.y.abs() <= 1000.0
                && translation.z.abs() <= 1000.0,
            "inverse bind translation outside ±1000: {translation:?}"
        );
        max_translation = max_translation
            .max(translation.x.abs())
            .max(translation.y.abs())
            .max(translation.z.abs());
    }

    println!(
        "dismember block={} bones={} partitions={} body_part_flags={} palette_len={} vertices={} weights={} max_abs_inverse_bind={:.3}",
        instance_index,
        payload.bones.len(),
        partition.partition_count,
        instance.body_part_flags.len(),
        partition.bone_palette.len(),
        partition.vertex_data.len(),
        partition.weights.len(),
        max_translation
    );
}

mod legacy_skin {
    use project_wormhole_ba2::dev::Bounds;
    use project_wormhole_shared::glam::{Mat3, Mat4, Quat, U16Vec3, U8Vec4, Vec3, Vec4};

    use crate::dev::*;

    fn av_object(name: u32) -> NiAVObject {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, name);
        push_u32(&mut bytes, u32::MAX); // extra data
        push_u32(&mut bytes, u32::MAX); // controller
        push_u32(&mut bytes, 0); // flags
        for value in [0.0f32, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        push_u32(&mut bytes, u32::MAX); // collision object
        let (rest, av) = NiAVObject::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        av
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn vertex_desc() -> BSVertexDesc {
        let (_, desc) = BSVertexDesc::parse(&0u64.to_le_bytes()).unwrap();
        desc
    }

    fn node(name: u32) -> NiNode {
        NiNode {
            av: av_object(name),
            children: Vec::new(),
        }
    }

    fn node_with_children(name: u32, children: Vec<u32>) -> NiNode {
        NiNode {
            av: av_object(name),
            children,
        }
    }

    fn transform(translation: Vec3) -> NiTransform {
        NiTransform {
            rotation: BSMatrix3(Mat3::IDENTITY),
            translation: BSVec3(translation),
            scale: 1.0,
        }
    }

    fn bone(translation: Vec3) -> NiSkinDataBoneData {
        NiSkinDataBoneData {
            skin_transform: transform(translation),
            bounding_sphere: Bounds {
                center: [0.0; 3],
                radius: 1.0,
            },
            vertex_count: 0,
            has_vertex_weights: false,
            vertex_weights: Vec::new(),
        }
    }

    fn skin_data(bones: Vec<NiSkinDataBoneData>) -> NiSkinData {
        NiSkinData {
            skin_transform: transform(Vec3::ZERO),
            has_vertex_weights: false,
            bone_count: bones.len() as u32,
            bones,
        }
    }

    fn partition(palette: Vec<u16>) -> NiSkinPartition {
        NiSkinPartition {
            vertex_desc: vertex_desc(),
            vertex_data: Vec::new(),
            triangles: Vec::new(),
            partition_count: 1,
            bone_palette: palette,
            weights_per_vertex: 4,
            weights: Vec::new(),
            bone_indices: Vec::new(),
            vertex_map: None,
            vertex_map_discarded: false,
            partition_flags: 0,
            palette_overflow: false,
        }
    }

    fn vertex(position: Vec3, joints: [u8; 4], weights: [f32; 4]) -> BSVertexData {
        BSVertexData {
            position: Some(position),
            bone_indices: Some(U8Vec4::from_array(joints)),
            bone_weights: Some(Vec4::from_array(weights)),
            ..Default::default()
        }
    }

    fn shape(name: u32, skin: u32, vertices: Vec<BSVertexData>) -> BSTriShape {
        BSTriShape {
            av: av_object(name),
            bounding_sphere: Bounds {
                center: [0.0; 3],
                radius: 1.0,
            },
            skin,
            shader_property: u32::MAX,
            alpha_property: u32::MAX,
            vertex_desc: vertex_desc(),
            num_triangles: 0,
            num_vertices: vertices.len() as u16,
            data_size: 0,
            vertex_data: vertices,
            triangles: Vec::new(),
        }
    }

    fn instance(bones: Vec<u32>, data: u32, partition: u32) -> NiSkinInstance {
        NiSkinInstance {
            data,
            skin_partition: partition,
            skeleton_root: 0,
            bones,
        }
    }

    fn dismember(instance: NiSkinInstance) -> BsDismemberSkinInstance {
        BsDismemberSkinInstance {
            skin_instance: instance,
            body_part_flags: vec![1],
            body_parts: vec![32],
        }
    }

    fn nif_file(blocks: Vec<NifBlock>, strings: &[&str]) -> NifFile {
        let strings = strings
            .iter()
            .map(|value| SizedString32((*value).to_owned()))
            .collect::<Vec<_>>();
        let block_count = blocks.len() as u32;
        NifFile {
            header: NifHeader {
                file_desc: StringN {
                    value: String::new(),
                },
                nif_version: NifFileVersion(0x1402_0007),
                endian_type: Endianess::Little,
                user_version: 12,
                block_count,
                bethesda_version: 100,
                author: None,
                process_script: None,
                export_script: None,
                max_filepath: None,
                block_types: Vec::new(),
                block_type_index: vec![0; blocks.len()],
                block_size_index: vec![0; blocks.len()],
                string_count: strings.len() as u32,
                string_max_size: 0,
                strings,
                groups: Vec::new(),
            },
            blocks,
        }
    }

    fn one_bone_fixture(joints: [u8; 4], weights: [f32; 4], bone_translation: Vec3) -> NifFile {
        nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(shape(1, 2, vec![vertex(Vec3::ZERO, joints, weights)])),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(bone_translation)])),
                NifBlock::NiSkinPartition(partition(vec![0])),
            ],
            &["BoneA", "Shape"],
        )
    }

    #[test]
    fn associates_shapes_with_their_own_skin_instance_block() {
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::NiNode(node(1)),
                NifBlock::BSTriShape(shape(
                    2,
                    5,
                    vec![vertex(Vec3::ZERO, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])],
                )),
                NifBlock::BSTriShape(shape(
                    3,
                    4,
                    vec![vertex(Vec3::X, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])],
                )),
                NifBlock::BsDismemberSkinInstance(dismember(instance(vec![1], 6, 8))),
                NifBlock::BsDismemberSkinInstance(dismember(instance(vec![0], 7, 9))),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(partition(vec![0])),
                NifBlock::NiSkinPartition(partition(vec![0])),
            ],
            &["BoneA", "BoneB", "ShapeA", "ShapeB"],
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        assert_eq!(model.skeletal_meshes.len(), 2);
        assert_eq!(model.skeletal_meshes[0].skin, vec!["BoneA".to_string()]);
        assert_eq!(model.skeletal_meshes[1].skin, vec!["BoneB".to_string()]);
        assert_eq!(
            model.skeletal_meshes[0]
                .mesh
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("ShapeA")
        );
        assert_eq!(
            model.skeletal_meshes[1]
                .mesh
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("ShapeB")
        );
    }

    /// Two bones behind a palette that swaps them, so an index read through
    /// the palette and the same index read directly name different joints.
    fn swapped_palette_fixture(shape_vertex: BSVertexData, partition: NiSkinPartition) -> NifFile {
        nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::NiNode(node(1)),
                NifBlock::BSTriShape(shape(2, 3, vec![shape_vertex])),
                NifBlock::NiSkinInstance(instance(vec![0, 1], 4, 5)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO), bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(partition),
            ],
            &["BoneA", "BoneB", "Shape"],
        )
    }

    #[test]
    fn reads_vertex_buffer_bone_indices_as_instance_bones() {
        let nif = swapped_palette_fixture(
            vertex(Vec3::ZERO, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
            partition(vec![1, 0]),
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        assert_eq!(model.skeletal_meshes[0].joints[0].to_array()[0], 0);
    }

    #[test]
    fn reads_partition_bone_indices_through_the_palette() {
        let mut partition = partition(vec![1, 0]);
        partition.weights = vec![vec![1.0, 0.0, 0.0, 0.0]];
        partition.bone_indices = vec![vec![0, 0, 0, 0]];
        let nif = swapped_palette_fixture(
            BSVertexData {
                position: Some(Vec3::ZERO),
                ..Default::default()
            },
            partition,
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        assert_eq!(model.skeletal_meshes[0].joints[0].to_array()[0], 1);
    }

    #[test]
    fn rejects_a_vertex_buffer_index_outside_the_instance_bones() {
        let nif = one_bone_fixture([5, 0, 0, 0], [1.0, 0.0, 0.0, 0.0], Vec3::ZERO);

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(error.contains("maps to joint 5"), "{error}");
        assert!(error.contains("shape block 1"), "{error}");
    }

    #[test]
    fn rejects_a_partition_index_outside_the_palette() {
        let mut partition = partition(vec![0]);
        partition.weights = vec![vec![1.0, 0.0, 0.0, 0.0]];
        partition.bone_indices = vec![vec![5, 0, 0, 0]];
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(shape(
                    1,
                    2,
                    vec![BSVertexData {
                        position: Some(Vec3::ZERO),
                        ..Default::default()
                    }],
                )),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(partition),
            ],
            &["BoneA", "Shape"],
        );

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(error.contains("palette entry 5"), "{error}");
        assert!(error.contains("shape block 1"), "{error}");
    }

    #[test]
    fn keeps_a_repeated_skeleton_node_under_its_first_parent_only() {
        let skeleton = nif_file(
            vec![
                NifBlock::NiNode(node_with_children(0, vec![1, 2])),
                NifBlock::NiNode(node_with_children(1, vec![3])),
                NifBlock::NiNode(node_with_children(2, vec![4])),
                NifBlock::NiNode(node(3)),
                NifBlock::NiNode(node(3)),
            ],
            &["Root", "ParentA", "ParentB", "Child"],
        );
        let mesh = one_bone_fixture([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0], Vec3::ZERO);

        let model = nif_to_model(&mesh, Some(&skeleton)).unwrap();

        let bones = &model.skeletal_meshes[0].bones.bones;
        assert!(bones["ParentA"].children.contains("Child"));
        assert!(
            !bones["ParentB"].children.contains("Child"),
            "a dropped duplicate must not keep a second parent"
        );
    }

    #[test]
    fn keeps_a_shared_skeleton_child_block_under_its_first_parent_only() {
        let skeleton = nif_file(
            vec![
                NifBlock::NiNode(node_with_children(0, vec![1, 2])),
                NifBlock::NiNode(node_with_children(1, vec![3])),
                NifBlock::NiNode(node_with_children(2, vec![3])),
                NifBlock::NiNode(node(3)),
            ],
            &["BoneA", "ParentA", "ParentB", "Child"],
        );
        let mesh = one_bone_fixture([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0], Vec3::ZERO);

        let model = nif_to_model(&mesh, Some(&skeleton)).unwrap();

        let bones = &model.skeletal_meshes[0].bones.bones;
        assert!(bones["ParentA"].children.contains("Child"));
        assert!(
            !bones["ParentB"].children.contains("Child"),
            "a shared child block must not keep a second parent"
        );
        model.to_gltf("shared-child".to_owned()).unwrap();
    }

    #[test]
    fn rejects_a_partition_whose_merged_palette_overflowed() {
        let mut partition = partition(vec![0]);
        partition.weights = vec![vec![1.0, 0.0, 0.0, 0.0]];
        partition.bone_indices = vec![vec![0, 0, 0, 0]];
        partition.palette_overflow = true;
        let nif = swapped_palette_fixture(
            BSVertexData {
                position: Some(Vec3::ZERO),
                ..Default::default()
            },
            partition,
        );

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(error.contains("more than 255"), "{error}");
        assert!(error.contains("shape block 2"), "{error}");
    }

    /// The exported glTF still skins the mesh: one skin, and the primitive carries
    /// `JOINTS_0` and `WEIGHTS_0`. An unskinned export would pass the model checks alone.
    fn assert_exports_one_skin(model: &crate::model::all::Model) {
        use gltf::json::validation::Checked;
        let (root, _) = model.to_gltf("padded".to_owned()).unwrap();
        assert_eq!(root.skins.len(), 1);
        let attributes = &root.meshes[0].primitives[0].attributes;
        assert!(attributes.contains_key(&Checked::Valid(gltf::Semantic::Joints(0))));
        assert!(attributes.contains_key(&Checked::Valid(gltf::Semantic::Weights(0))));
    }

    /// One bone, three vertices drawn by triangle (0, 1, 2), and `extra` appended
    /// after them. `extra_drawn` adds a second triangle that uses the extra vertex.
    fn padded_buffer_fixture(extra: BSVertexData, extra_drawn: bool) -> NifFile {
        let weighted = |position| vertex(position, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        let mut tri_shape = shape(
            1,
            2,
            vec![
                weighted(Vec3::ZERO),
                weighted(Vec3::X),
                weighted(Vec3::Y),
                extra,
            ],
        );
        tri_shape.triangles.push(U16Vec3::new(0, 1, 2));
        if extra_drawn {
            tri_shape.triangles.push(U16Vec3::new(1, 2, 3));
        }
        tri_shape.num_triangles = tri_shape.triangles.len() as u32;
        nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(tri_shape),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(partition(vec![0])),
            ],
            &["BoneA", "Shape"],
        )
    }

    #[test]
    fn rejects_a_drawn_vertex_whose_weights_sum_to_zero() {
        let unweighted = vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let nif = padded_buffer_fixture(unweighted, true);

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(
            error.contains("vertex 3 has a skin weight sum of 0"),
            "{error}"
        );
    }

    #[test]
    fn keeps_an_undrawn_zero_weight_padding_vertex_skinned_in_place() {
        // The retail pattern: all weights 0.0, indices (0, 0, 0, 0), no triangle.
        let padding = vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let nif = padded_buffer_fixture(padding, false);

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        let skinned = &model.skeletal_meshes[0];
        let mesh = skinned.mesh.as_ref().unwrap();
        assert_eq!(
            mesh.positions.len(),
            4,
            "the padding vertex keeps its index"
        );
        assert_eq!(mesh.positions[3], Vec3::Z);
        assert_eq!(skinned.weights[3], Vec4::new(1.0, 0.0, 0.0, 0.0));
        assert_eq!(skinned.joints[3], U8Vec4::ZERO);
        assert_exports_one_skin(&model);
    }

    #[test]
    fn keeps_a_padding_vertex_in_the_partition_shared_buffer_skinned() {
        // The retail layout: the shape carries no vertices, and positions, weights and
        // triangles all come from the NiSkinPartition's shared buffer.
        let weighted = |position| vertex(position, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        let mut shared = partition(vec![0]);
        shared.vertex_data = vec![
            weighted(Vec3::ZERO),
            weighted(Vec3::X),
            weighted(Vec3::Y),
            vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]),
        ];
        shared.triangles = vec![U16Vec3::new(0, 1, 2)];
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(shape(1, 2, Vec::new())),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(shared),
            ],
            &["BoneA", "Shape"],
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        let skinned = &model.skeletal_meshes[0];
        assert_eq!(skinned.mesh.as_ref().unwrap().positions[3], Vec3::Z);
        assert_eq!(skinned.weights[3], Vec4::new(1.0, 0.0, 0.0, 0.0));
        assert_exports_one_skin(&model);
    }

    #[test]
    fn rejects_an_undrawn_zero_weight_vertex_that_names_other_bones() {
        // Retail padding always names bone indices (0, 0, 0, 0); anything else is not padding.
        let other = vertex(Vec3::Z, [1, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let nif = padded_buffer_fixture(other, false);

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(error.contains("skin weight sum"), "{error}");
    }

    #[test]
    fn rejects_an_undrawn_zero_weight_vertex_that_a_partition_maps() {
        // A vertex a partition lists is skinned data, not padding, even when no triangle draws it.
        let mapped = vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let mut nif = padded_buffer_fixture(mapped, false);
        if let NifBlock::NiSkinPartition(partition) = &mut nif.blocks[4] {
            partition.vertex_map = Some(vec![0, 1, 2, 3]);
        } else {
            panic!("fixture block 4 is not NiSkinPartition");
        }

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(
            error.contains("vertex 3 has a skin weight sum of 0"),
            "{error}"
        );
    }

    #[test]
    fn binds_no_padding_when_partitions_disagreed_on_a_vertex_map() {
        // Mixed map presence drops the merged map, so a mapped vertex cannot be ruled out.
        let padding = vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let mut nif = padded_buffer_fixture(padding, false);
        if let NifBlock::NiSkinPartition(partition) = &mut nif.blocks[4] {
            partition.vertex_map_discarded = true;
        } else {
            panic!("fixture block 4 is not NiSkinPartition");
        }

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(
            error.contains("vertex 3 has a skin weight sum of 0"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_undrawn_zero_weight_vertex_that_a_partition_triangle_draws() {
        // The shape's own triangles skip vertex 3, but a partition triangle uses it.
        let padding = vertex(Vec3::Z, [0, 0, 0, 0], [0.0, 0.0, 0.0, 0.0]);
        let mut nif = padded_buffer_fixture(padding, false);
        if let NifBlock::NiSkinPartition(partition) = &mut nif.blocks[4] {
            partition.triangles = vec![U16Vec3::new(1, 2, 3)];
        } else {
            panic!("fixture block 4 is not NiSkinPartition");
        }

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(
            error.contains("vertex 3 has a skin weight sum of 0"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_undrawn_zero_weight_partition_row() {
        // Weights from partition rows, not the vertex buffer, are never padding.
        let position = |position| BSVertexData {
            position: Some(position),
            ..Default::default()
        };
        let mut tri_shape = shape(
            1,
            2,
            vec![
                position(Vec3::ZERO),
                position(Vec3::X),
                position(Vec3::Y),
                position(Vec3::Z),
            ],
        );
        tri_shape.triangles.push(U16Vec3::new(0, 1, 2));
        tri_shape.num_triangles = 1;
        let mut rows = partition(vec![0]);
        rows.weights = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 0.0],
        ];
        rows.bone_indices = vec![vec![0, 0, 0, 0]; 4];
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(tri_shape),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(rows),
            ],
            &["BoneA", "Shape"],
        );

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        assert!(
            error.contains("vertex 3 has a skin weight sum of 0"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_undrawn_vertex_with_a_negative_weight_sum() {
        // Only an exactly-zero padding vertex is bound; other bad weights still fail.
        let broken = vertex(Vec3::Z, [0, 0, 0, 0], [-1.0, 0.0, 0.0, 0.0]);
        let nif = padded_buffer_fixture(broken, false);

        let error = nif_to_model(&nif, Some(&nif)).unwrap_err();

        // The negative-weight guard runs before the sum check, so a substantive negative is
        // rejected for the weight itself rather than for the zero total it leaves behind.
        assert!(error.contains("has an invalid skin weight -1"), "{error}");
    }

    #[test]
    fn converts_a_non_identity_bone_transform_to_the_expected_inverse_bind() {
        use std::f32::consts::FRAC_PI_2;

        let mut nif = one_bone_fixture([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0], Vec3::ZERO);
        let rotation = Mat3::from_rotation_z(FRAC_PI_2);
        if let NifBlock::NiSkinData(data) = &mut nif.blocks[3] {
            data.bones[0].skin_transform.rotation = BSMatrix3(rotation);
            data.bones[0].skin_transform.translation = BSVec3(Vec3::new(1.0, 2.0, 3.0));
        } else {
            panic!("fixture block 3 is not NiSkinData");
        }

        let model = nif_to_model(&nif, Some(&nif)).unwrap();
        let actual = model.skeletal_meshes[0].inverse_bind_matrices[0].0;

        let mut expected = Mat4::from_mat3(rotation);
        expected.w_axis = Vec4::new(1.0, 2.0, 3.0, 1.0);
        for (actual, expected) in actual.to_cols_array().iter().zip(expected.to_cols_array()) {
            assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
        }
        assert_eq!(actual.w_axis, Vec4::new(1.0, 2.0, 3.0, 1.0));
    }

    #[test]
    fn keeps_the_skin_transform_scale_in_the_inverse_bind() {
        let mut nif = one_bone_fixture([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0], Vec3::ZERO);
        if let NifBlock::NiSkinData(data) = &mut nif.blocks[3] {
            data.bones[0].skin_transform.scale = 0.5;
            data.bones[0].skin_transform.translation = BSVec3(Vec3::new(1.0, 2.0, 3.0));
        } else {
            panic!("fixture block 3 is not NiSkinData");
        }

        let model = nif_to_model(&nif, Some(&nif)).unwrap();
        let actual = model.skeletal_meshes[0].inverse_bind_matrices[0].0;

        let expected = Mat4::from_scale_rotation_translation(
            Vec3::splat(0.5),
            Quat::IDENTITY,
            Vec3::new(1.0, 2.0, 3.0),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn places_partition_rows_through_a_permuting_vertex_map() {
        let mut partition = partition(vec![0, 1, 2]);
        partition.weights = vec![vec![1.0, 0.0, 0.0, 0.0]; 3];
        partition.bone_indices = vec![vec![0, 0, 0, 0], vec![1, 0, 0, 0], vec![2, 0, 0, 0]];
        partition.vertex_map = Some(vec![2, 0, 1]);
        let position = |x: f32| BSVertexData {
            position: Some(Vec3::new(x, 0.0, 0.0)),
            ..Default::default()
        };
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::NiNode(node(1)),
                NifBlock::NiNode(node(2)),
                NifBlock::BSTriShape(shape(
                    3,
                    4,
                    vec![position(0.0), position(1.0), position(2.0)],
                )),
                NifBlock::NiSkinInstance(instance(vec![0, 1, 2], 5, 6)),
                NifBlock::NiSkinData(skin_data(vec![
                    bone(Vec3::ZERO),
                    bone(Vec3::ZERO),
                    bone(Vec3::ZERO),
                ])),
                NifBlock::NiSkinPartition(partition),
            ],
            &["BoneA", "BoneB", "BoneC", "Shape"],
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        // Row r belongs to vertex map[r]: rows 0, 1, 2 land on vertices 2, 0, 1.
        let first_joints = model.skeletal_meshes[0]
            .joints
            .iter()
            .map(|joints| joints.to_array()[0])
            .collect::<Vec<_>>();
        assert_eq!(first_joints, vec![1, 2, 0]);
    }

    #[test]
    fn leaves_unskinned_shapes_out_of_the_skeletal_mesh_set() {
        let nif = nif_file(
            vec![
                NifBlock::NiNode(node(0)),
                NifBlock::BSTriShape(shape(
                    1,
                    2,
                    vec![vertex(Vec3::ZERO, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])],
                )),
                NifBlock::NiSkinInstance(instance(vec![0], 3, 4)),
                NifBlock::NiSkinData(skin_data(vec![bone(Vec3::ZERO)])),
                NifBlock::NiSkinPartition(partition(vec![0])),
                NifBlock::BSTriShape(shape(
                    2,
                    u32::MAX,
                    vec![vertex(Vec3::X, [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])],
                )),
            ],
            &["BoneA", "ShapeA", "Unskinned"],
        );

        let model = nif_to_model(&nif, Some(&nif)).unwrap();

        assert_eq!(model.skeletal_meshes.len(), 1);
        assert_eq!(model.skeletal_meshes[0].skin, vec!["BoneA".to_string()]);
    }
}

#[test]
#[ignore = "requires retail Skyrim actor NIFs from MUDCRAB_ACTOR_NIF_DIR"]
fn retail_male_body_builds_legacy_skeletal_meshes() {
    let directory = match std::env::var("MUDCRAB_ACTOR_NIF_DIR") {
        Ok(directory) => directory,
        Err(_) => {
            eprintln!("MUDCRAB_ACTOR_NIF_DIR is not set; skipping retail actor check");
            return;
        }
    };
    let base = format!("{directory}/character/character assets");
    let nif_bytes = std::fs::read(format!("{base}/malebody_1.nif")).expect("read malebody_1.nif");
    let skeleton_bytes = std::fs::read(format!("{base}/skeleton.nif")).expect("read skeleton.nif");
    let (_, nif) = NifFile::parse(&nif_bytes).expect("parse malebody_1.nif");
    let (_, skeleton) = NifFile::parse(&skeleton_bytes).expect("parse skeleton.nif");

    let model = nif_to_model(&nif, Some(&skeleton)).expect("malebody_1.nif converts to a model");
    assert_eq!(
        model.skeletal_meshes.len(),
        2,
        "expected two skinned body shapes"
    );

    println!("skeletal meshes: {}", model.skeletal_meshes.len());
    for (index, mesh) in model.skeletal_meshes.iter().enumerate() {
        let geometry = mesh.mesh.as_ref().expect("skeletal mesh has geometry");
        assert!(!mesh.skin.is_empty(), "mesh {index} has no joints");
        assert_eq!(mesh.skin.len(), mesh.inverse_bind_matrices.len());
        assert_eq!(mesh.joints.len(), mesh.weights.len());
        assert_eq!(mesh.joints.len(), geometry.positions.len());

        let mut max_joint = 0usize;
        for joint in &mesh.joints {
            for value in [joint.x, joint.y, joint.z, joint.w] {
                assert!(
                    usize::from(value) < mesh.skin.len(),
                    "mesh {index} joint {value} is outside the skin"
                );
                max_joint = max_joint.max(usize::from(value));
            }
        }
        for (vertex, weight) in mesh.weights.iter().enumerate() {
            let sum = weight.x + weight.y + weight.z + weight.w;
            assert!(
                (sum - 1.0).abs() <= 1e-3,
                "mesh {index} vertex {vertex} weights sum to {sum}"
            );
        }
        println!(
            "mesh {index}: joints={} vertices={} max_joint={max_joint}",
            mesh.skin.len(),
            geometry.positions.len()
        );
    }

    assert!(
        model.skeletal_meshes[0].bones.has_bone("NPC Head [Head]"),
        "bone tree is missing the head node"
    );
    println!(
        "bone tree: {} bones",
        model.skeletal_meshes[0].bones.bones.len()
    );
}
