//! Loads a model-space glTF through the real glTF loader and checks what the native handler
//! binds: the skin material, its linear `_s` twin and the dependency holder.

use super::*;
use crate::skin_material::{SkyrimSkinFlags, SkyrimSkinKey};
use bevy::{
    asset::{LoadState, RenderAssetUsages},
    image::{
        CompressedImageFormats, ImageAddressMode, ImageFilterMode, ImageLoader, ImagePlugin,
        ImageSampler, ImageSamplerDescriptor,
    },
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    world_serialization::{WorldAsset, WorldSerializationPlugin},
};
use std::{fs, path::Path};

/// Stands in for `bevy_pbr`'s private glTF handler, which only a rendering app registers: it
/// publishes `"{label}/std"` and binds it, which is all [`NativeMaterialHandler`] reads.
#[derive(Clone)]
struct StockStandardHandler;

impl GltfExtensionHandler for StockStandardHandler {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new(self.clone())
    }

    fn on_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _source: &gltf::Material<'_>,
        _handle: Handle<GltfMaterial>,
        loaded: &GltfMaterial,
        label: &str,
    ) {
        context.add_labeled_asset(
            format!("{label}/std"),
            StandardMaterial {
                base_color: loaded.base_color,
                base_color_channel: loaded.base_color_channel.clone(),
                base_color_texture: loaded.base_color_texture.clone(),
                emissive: loaded.emissive,
                emissive_channel: loaded.emissive_channel.clone(),
                emissive_texture: loaded.emissive_texture.clone(),
                normal_map_texture: loaded.normal_map_texture.clone(),
                specular_tint_texture: loaded.specular_tint_texture.clone(),
                alpha_mode: loaded.alpha_mode,
                reflectance: loaded.reflectance,
                ..default()
            },
        );
    }

    fn on_spawn_mesh_and_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _primitive: &gltf::Primitive<'_>,
        _mesh: &gltf::Mesh<'_>,
        _material: &gltf::Material<'_>,
        entity: &mut EntityWorldMut,
        label: &str,
    ) {
        entity.insert(MeshMaterial3d(
            context.get_label_handle::<StandardMaterial>(format!("{label}/std")),
        ));
    }
}

fn write_png(directory: &Path, name: &str, pixel: [u8; 4]) {
    Image::new_fill(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &pixel,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD,
    )
    .try_into_dynamic()
    .unwrap()
    .save(directory.join(name))
    .unwrap();
}

/// One triangle with positions, normals and UVs, its material tagged `model_space` with the given
/// `shaderFlags1`, the `_msn` as normal texture and the `_s` published as the converter does: an
/// sRGB `.opensky-srgb` alias in `KHR_materials_specular.specularColorTexture`, clamped.
fn write_model_space_gltf(directory: &Path, shader_flags_1: u64) {
    write_png(directory, "body_msn.png", [128, 128, 255, 255]);
    write_png(directory, "body_s.opensky-srgb.png", [64, 64, 64, 255]);
    write_png(directory, "body_s.png", [64, 64, 64, 255]);
    let floats: [f32; 24] = [
        0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // positions
        0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // normals
        0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // uvs
    ];
    let bytes: Vec<u8> = floats
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    fs::write(directory.join("body.bin"), &bytes).unwrap();
    let document = serde_json::json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_materials_specular"],
        "buffers": [{"uri": "body.bin", "byteLength": bytes.len()}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 36},
            {"buffer": 0, "byteOffset": 72, "byteLength": 24}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"},
            {"bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC2"}
        ],
        "images": [{"uri": "body_msn.png"}, {"uri": "body_s.opensky-srgb.png"}],
        "samplers": [{"wrapS": 33071, "wrapT": 33071}],
        "textures": [{"source": 0}, {"source": 1, "sampler": 0}],
        "materials": [{
            "normalTexture": {"index": 0},
            "extensions": {"KHR_materials_specular": {
                "specularFactor": 1.0,
                "specularColorTexture": {"index": 1}
            }},
            "extras": {"openSkyrim": {
                "normalConvention": "model_space",
                "shaderFlags1": shader_flags_1,
                "shaderFlags2": 1
            }}
        }],
        "meshes": [{"primitives": [{
            "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
            "material": 0
        }]}],
        "nodes": [{"mesh": 0}],
        "scenes": [{"nodes": [0]}],
        "scene": 0
    });
    fs::write(
        directory.join("body.gltf"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

/// Two skin materials reference one converted source texture with clamp and repeat aliases.
/// Their linear companions use the same names emitted by converter alias publication.
fn write_two_sampler_skin_gltf(directory: &Path, reverse_material_order: bool) {
    write_model_space_gltf(directory, (1 << 31) | (1 << 12) | 1);
    for (path, pixel) in [
        ("body_s.opensky-srgb.opensky-wrap0.png", [64, 64, 64, 255]),
        ("body_s.opensky-wrap0.png", [64, 64, 64, 255]),
        ("body_s.opensky-srgb.opensky-wrap2.png", [64, 64, 64, 255]),
        ("body_s.opensky-wrap2.png", [64, 64, 64, 255]),
    ] {
        write_png(directory, path, pixel);
    }
    let path = directory.join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["images"][1]["uri"] = serde_json::json!("body_s.opensky-srgb.opensky-wrap0.png");
    document["images"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"uri": "body_s.opensky-srgb.opensky-wrap2.png"}));
    document["samplers"] = serde_json::json!([
        {"wrapS": 33071, "wrapT": 33071},
        {"wrapS": 10497, "wrapT": 10497}
    ]);
    document["textures"][1]["sampler"] = serde_json::json!(0);
    document["textures"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"source": 2, "sampler": 1}));
    let mut repeat_material = document["materials"][0].clone();
    repeat_material["extensions"]["KHR_materials_specular"]["specularColorTexture"]["index"] =
        serde_json::json!(2);
    document["materials"]
        .as_array_mut()
        .unwrap()
        .push(repeat_material);
    document["meshes"][0]["primitives"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
            "material": 1
        }));
    if reverse_material_order {
        document["materials"].as_array_mut().unwrap().reverse();
        document["meshes"][0]["primitives"][0]["material"] = serde_json::json!(1);
        document["meshes"][0]["primitives"][1]["material"] = serde_json::json!(0);
    }
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

fn loader_app(directory: &Path) -> App {
    let mut app = App::new();
    let default_sampler = configured_default_sampler();
    app.add_plugins((
        MinimalPlugins,
        AssetPlugin {
            file_path: directory.to_string_lossy().into_owned(),
            ..default()
        },
        ImagePlugin::default(),
        WorldSerializationPlugin,
        bevy::gltf::GltfPlugin {
            default_sampler: default_sampler.clone(),
            ..default()
        },
    ))
    .init_asset::<Mesh>()
    .init_asset::<StandardMaterial>()
    .register_asset_loader(ImageLoader::new(CompressedImageFormats::NONE));
    app.world_mut()
        .resource_mut::<GltfExtensionHandlers>()
        .0
        .write_blocking()
        .push(Box::new(StockStandardHandler));
    app.add_plugins(NifMaterialPlugin);
    assert!(
        !app.world().contains_resource::<DefaultGltfImageSampler>(),
        "GltfPlugin creates its configured sampler in finish, after build hooks"
    );
    app.finish();
    let installed = app.world().resource::<DefaultGltfImageSampler>().get();
    assert_eq!(installed, default_sampler);
    app.cleanup();
    app
}

/// A distinctive real glTF default. Fixture textures specify wrapping only, so their linear
/// aliases must inherit these unspecified filter and anisotropy settings.
fn configured_default_sampler() -> ImageSamplerDescriptor {
    ImageSamplerDescriptor {
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 8,
        ..ImageSamplerDescriptor::linear()
    }
}

fn assert_configured_sampler(image: &Image) {
    let ImageSampler::Descriptor(sampler) = &image.sampler else {
        panic!("the glTF sampler is passed to the linear load");
    };
    assert_eq!(sampler.mag_filter, ImageFilterMode::Linear);
    assert_eq!(sampler.min_filter, ImageFilterMode::Linear);
    assert_eq!(sampler.mipmap_filter, ImageFilterMode::Linear);
    assert_eq!(sampler.anisotropy_clamp, 8);
}

fn wait_for_dependencies<A: Asset>(app: &mut App, handle: &Handle<A>) {
    // A healthy load takes milliseconds; the margin only keeps a slow runner from failing it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        app.update();
        let server = app.world().resource::<AssetServer>();
        if let LoadState::Failed(error) = server.load_state(handle.id()) {
            panic!("fixture load failed: {error}");
        }
        if server.is_loaded_with_dependencies(handle.id()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture load timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn model_space_material_loads_as_a_skin_material_with_a_linear_gloss_mask() {
    let directory = tempfile::tempdir().unwrap();
    // Depth test and write (no depth state), SLSF1 Specular and Model_Space_Normals.
    write_model_space_gltf(directory.path(), (1 << 31) | (1 << 12) | 1);
    let mut app = loader_app(directory.path());
    let scene: Handle<WorldAsset> = app
        .world()
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
    wait_for_dependencies(&mut app, &scene);

    let mut scene_world = app
        .world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .remove(&scene)
        .unwrap()
        .world;
    let mut query = scene_world.query::<(
        &MeshMaterial3d<SkyrimSkinMaterial>,
        &NifNativeMaterialSource,
        &NifSourceMaterial,
        Option<&MeshMaterial3d<StandardMaterial>>,
    )>();
    let (skin, base, source, standard) = query.single(&scene_world).unwrap();
    assert!(standard.is_none(), "a mesh draws once per material binding");

    let world = app.world();
    let server = world.resource::<AssetServer>();
    assert!(server.is_loaded_with_dependencies(skin.0.id()));
    let skin = world
        .resource::<Assets<SkyrimSkinMaterial>>()
        .get(&skin.0)
        .unwrap();
    let images = skin
        .extension
        .images
        .as_ref()
        .expect("the skin material holds its images");
    assert!(
        world
            .resource::<Assets<SkyrimSkinImages>>()
            .contains(images)
    );
    let materials = world.resource::<Assets<StandardMaterial>>();
    let stock = materials.get(&source.0).unwrap();
    let base = materials.get(&base.0).unwrap();
    assert!(base.normal_map_texture.is_none());
    assert_eq!(
        skin.extension.model_space_normal,
        stock.normal_map_texture.clone()
    );
    assert!(skin.base.normal_map_texture.is_none());
    assert!(skin.base.specular_tint_texture.is_none());
    assert_eq!(
        skin.extension.settings.flags,
        SkyrimSkinFlags::MODEL_SPACE_NORMALS
            | SkyrimSkinFlags::SPECULAR_MAP
            | SkyrimSkinFlags::SPECULAR
    );

    let srgb = stock.specular_tint_texture.as_ref().unwrap();
    let linear = skin.extension.specular.as_ref().unwrap();
    assert_eq!(
        server.get_path(srgb.id()).unwrap(),
        "body_s.opensky-srgb.png".into()
    );
    assert_eq!(server.get_path(linear.id()).unwrap(), "body_s.png".into());
    let images = world.resource::<Assets<Image>>();
    assert_eq!(
        images.get(srgb).unwrap().texture_descriptor.format,
        TextureFormat::Rgba8UnormSrgb
    );
    let linear = images.get(linear).unwrap();
    assert_eq!(linear.texture_descriptor.format, TextureFormat::Rgba8Unorm);
    assert_configured_sampler(linear);
    let ImageSampler::Descriptor(sampler) = &linear.sampler else {
        unreachable!("assert_configured_sampler checks this variant");
    };
    assert_eq!(sampler.address_mode_u, ImageAddressMode::ClampToEdge);
    assert_eq!(sampler.address_mode_v, ImageAddressMode::ClampToEdge);
}

#[test]
fn shared_source_keeps_distinct_linear_images_and_samplers_in_both_material_orders() {
    for reverse in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write_two_sampler_skin_gltf(directory.path(), reverse);
        let mut app = loader_app(directory.path());
        let scene: Handle<WorldAsset> = app
            .world()
            .resource::<AssetServer>()
            .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
        wait_for_dependencies(&mut app, &scene);
        let mut scene_world = app
            .world_mut()
            .resource_mut::<Assets<WorldAsset>>()
            .remove(&scene)
            .unwrap()
            .world;
        let mut query = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
        let handles = query
            .iter(&scene_world)
            .map(|binding| binding.0.clone())
            .collect::<Vec<_>>();
        assert_eq!(handles.len(), 2);
        let materials = app.world().resource::<Assets<SkyrimSkinMaterial>>();
        let skins = handles
            .iter()
            .map(|handle| materials.get(handle).unwrap())
            .collect::<Vec<_>>();
        let linear_handles = skins
            .iter()
            .map(|skin| skin.extension.specular.as_ref().unwrap())
            .collect::<Vec<_>>();
        assert_ne!(linear_handles[0].id(), linear_handles[1].id());

        let world = app.world();
        let server = world.resource::<AssetServer>();
        let images = world.resource::<Assets<Image>>();
        let mut observed = linear_handles
            .iter()
            .map(|handle| {
                let path = server.get_path(handle.id()).unwrap().to_string();
                let ImageSampler::Descriptor(sampler) = &images.get(*handle).unwrap().sampler
                else {
                    panic!("linear alias must carry its glTF sampler");
                };
                (path, sampler.address_mode_u)
            })
            .collect::<Vec<_>>();
        observed.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            observed,
            vec![
                (
                    "body_s.opensky-wrap0.png".to_owned(),
                    ImageAddressMode::ClampToEdge
                ),
                (
                    "body_s.opensky-wrap2.png".to_owned(),
                    ImageAddressMode::Repeat
                ),
            ]
        );
    }
}

#[test]
fn model_space_material_with_a_depth_state_keeps_the_depth_material() {
    let directory = tempfile::tempdir().unwrap();
    // No depth write: an authored depth state, which keeps its own extension.
    write_model_space_gltf(directory.path(), (1 << 12) | 1);
    let mut app = loader_app(directory.path());
    let scene: Handle<WorldAsset> = app
        .world()
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
    wait_for_dependencies(&mut app, &scene);
    let mut scene_world = app
        .world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .remove(&scene)
        .unwrap()
        .world;
    let mut skins = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    assert_eq!(skins.iter(&scene_world).count(), 0);
    let mut depth = scene_world.query::<&MeshMaterial3d<NifDepthMaterial>>();
    assert_eq!(depth.iter(&scene_world).count(), 1);
}

/// [`write_model_space_gltf`] turned into a FaceGen head: a `face_tint` material whose detail and
/// FaceGen tint slots point at sRGB aliases, as the converter publishes them.
fn write_facegen_gltf(directory: &Path, tint_semantic: &str) {
    write_model_space_gltf(directory, (1 << 31) | (1 << 12) | 1);
    write_png(directory, "detail.opensky-srgb.png", [65, 64, 65, 255]);
    write_png(directory, "detail.png", [65, 64, 65, 255]);
    write_png(directory, "tint.opensky-srgb.png", [184, 162, 154, 255]);
    write_png(directory, "tint.png", [184, 162, 154, 255]);
    let path = directory.join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for uri in ["detail.opensky-srgb.png", "tint.opensky-srgb.png"] {
        let image = document["images"].as_array().unwrap().len();
        document["images"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "uri": uri }));
        document["textures"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "source": image, "sampler": 0 }));
    }
    document["extensionsUsed"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("OPEN_SKYRIM_material"));
    document["materials"][0]["extensions"]["OPEN_SKYRIM_material"] = serde_json::json!({
        "shaderFamily": "lighting",
        "lightingShaderType": "face_tint",
        "textureSlots": [
            {"slot": 3, "semantic": "detail", "texture": 2, "colorSpace": "srgb"},
            {"slot": 6, "semantic": tint_semantic, "texture": 3, "colorSpace": "srgb"}
        ]
    });
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

fn load_skin_material(directory: &Path) -> (App, SkyrimSkinMaterial) {
    let mut app = loader_app(directory);
    let scene: Handle<WorldAsset> = app
        .world()
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
    wait_for_dependencies(&mut app, &scene);
    let mut scene_world = app
        .world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .remove(&scene)
        .unwrap()
        .world;
    let mut query = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    let handle = query.single(&scene_world).unwrap().0.clone();
    let material = app
        .world()
        .resource::<Assets<SkyrimSkinMaterial>>()
        .get(&handle)
        .unwrap()
        .clone();
    (app, material)
}

#[test]
fn facegen_head_binds_its_tint_and_detail_maps_linear() {
    let directory = tempfile::tempdir().unwrap();
    write_facegen_gltf(directory.path(), "facegen_tint");
    let (app, skin) = load_skin_material(directory.path());
    assert_eq!(
        skin.extension.settings.flags,
        SkyrimSkinFlags::MODEL_SPACE_NORMALS
            | SkyrimSkinFlags::SPECULAR_MAP
            | SkyrimSkinFlags::SPECULAR
            | SkyrimSkinFlags::FACEGEN
    );
    let world = app.world();
    let server = world.resource::<AssetServer>();
    let images = world.resource::<Assets<Image>>();
    for (handle, path) in [
        (skin.extension.facegen_tint.as_ref().unwrap(), "tint.png"),
        (
            skin.extension.facegen_detail.as_ref().unwrap(),
            "detail.png",
        ),
    ] {
        assert_eq!(server.get_path(handle.id()).unwrap(), path.into());
        let image = images.get(handle).unwrap();
        assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba8Unorm);
        assert_configured_sampler(image);
        let ImageSampler::Descriptor(sampler) = &image.sampler else {
            unreachable!("assert_configured_sampler checks this variant");
        };
        assert_eq!(sampler.address_mode_u, ImageAddressMode::ClampToEdge);
    }
}

#[test]
fn a_face_tint_head_from_an_older_pack_keeps_the_plain_skin_material() {
    let directory = tempfile::tempdir().unwrap();
    write_facegen_gltf(directory.path(), "inner_layer");
    let (_app, skin) = load_skin_material(directory.path());
    assert_eq!(skin.extension.settings.flags & SkyrimSkinFlags::FACEGEN, 0);
    assert!(skin.extension.facegen_tint.is_none());
    assert!(skin.extension.facegen_detail.is_none());
}

/// [`write_model_space_gltf`] as a `skin_tint` body part, with `tint_color` published as the NIF's
/// own skin tint when given.
fn write_skin_tint_gltf(directory: &Path, tint_color: Option<[f32; 3]>) {
    write_model_space_gltf(directory, (1 << 31) | (1 << 12) | 1);
    let path = directory.join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["extensionsUsed"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("OPEN_SKYRIM_material"));
    let mut extension = serde_json::json!({
        "shaderFamily": "lighting",
        "lightingShaderType": "skin_tint",
    });
    if let Some(colour) = tint_color {
        extension["tintColor"] = serde_json::json!(colour);
    }
    document["materials"][0]["extensions"]["OPEN_SKYRIM_material"] = extension;
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

/// Whether the scene's one skin primitive carries [`SkinTintSurface`], and its material.
fn load_skin_tint_surface(directory: &Path) -> (bool, SkyrimSkinMaterial) {
    let mut app = loader_app(directory);
    let scene: Handle<WorldAsset> = app
        .world()
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
    wait_for_dependencies(&mut app, &scene);
    let mut scene_world = app
        .world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .remove(&scene)
        .unwrap()
        .world;
    let mut query = scene_world.query::<(
        &MeshMaterial3d<SkyrimSkinMaterial>,
        Has<crate::skin_material::SkinTintSurface>,
    )>();
    let (handle, marked) = query.single(&scene_world).unwrap();
    let material = app
        .world()
        .resource::<Assets<SkyrimSkinMaterial>>()
        .get(&handle.0)
        .unwrap()
        .clone();
    (marked, material)
}

#[test]
fn a_skin_tint_part_is_marked_and_untinted_without_a_nif_colour() {
    let directory = tempfile::tempdir().unwrap();
    write_skin_tint_gltf(directory.path(), None);
    let (marked, skin) = load_skin_tint_surface(directory.path());
    assert!(
        marked,
        "the actor's skin tone finds the surface by its marker"
    );
    assert_eq!(
        skin.extension.settings.flags & SkyrimSkinFlags::SKIN_TINT,
        0
    );
    assert_eq!(skin.extension.settings.tint_color, Vec4::ONE);
}

#[test]
fn a_skin_tint_part_ignores_the_nif_tint_colour() {
    for colour in [[1.0, 1.0, 1.0], [0.5, 0.4, 0.3]] {
        let directory = tempfile::tempdir().unwrap();
        write_skin_tint_gltf(directory.path(), Some(colour));
        let (marked, skin) = load_skin_tint_surface(directory.path());
        assert!(marked, "the actor's skin tone still finds the surface");
        assert_eq!(
            skin.extension.settings.flags & SkyrimSkinFlags::SKIN_TINT,
            0,
            "{colour:?}"
        );
        assert_eq!(skin.extension.settings.tint_color, Vec4::ONE, "{colour:?}");
    }
}

#[test]
fn a_plain_model_space_material_is_not_a_skin_tint_surface() {
    let directory = tempfile::tempdir().unwrap();
    write_model_space_gltf(directory.path(), (1 << 31) | (1 << 12) | 1);
    let (marked, skin) = load_skin_tint_surface(directory.path());
    assert!(!marked);
    assert_eq!(
        skin.extension.settings.flags & SkyrimSkinFlags::SKIN_TINT,
        0
    );
}

/// [`write_model_space_gltf`] as a FaceGen hair card: a tangent-space (`directx`) `hair_tint`
/// material with the given flags and, when given, the NIF's hair tint as `tintColor`.
fn write_hair_gltf(
    directory: &Path,
    shader_flags_1: u64,
    shader_flags_2: u64,
    tint_color: Option<[f32; 3]>,
) {
    write_model_space_gltf(directory, shader_flags_1);
    let path = directory.join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["extensionsUsed"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("OPEN_SKYRIM_material"));
    let material = &mut document["materials"][0];
    material["extras"]["openSkyrim"]["normalConvention"] = serde_json::json!("directx");
    material["extras"]["openSkyrim"]["shaderFlags2"] = serde_json::json!(shader_flags_2);
    let mut extension = serde_json::json!({
        "shaderFamily": "lighting",
        "lightingShaderType": "hair_tint",
        "textureSlots": []
    });
    if let Some(colour) = tint_color {
        extension["tintColor"] = serde_json::json!(colour);
    }
    material["extensions"]["OPEN_SKYRIM_material"] = extension;
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

/// [`write_hair_gltf`] as an alpha-tested hair card: glTF `MASK` with `cutoff`.
fn write_masked_hair_gltf(directory: &Path, cutoff: f32) {
    write_hair_gltf(directory, 1 << 31, 1, Some(ALVOR_HAIR));
    let path = directory.join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["materials"][0]["alphaMode"] = serde_json::json!("MASK");
    document["materials"][0]["alphaCutoff"] = serde_json::json!(cutoff);
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

fn load_scene_world(app: &mut App) -> World {
    let scene: Handle<WorldAsset> = app
        .world()
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
    wait_for_dependencies(app, &scene);
    app.world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .remove(&scene)
        .unwrap()
        .world
}

#[test]
fn own_emittance_scene_binds_the_diffuse_texture_and_preserves_stock_emission() {
    let directory = tempfile::tempdir().unwrap();
    write_model_space_gltf(directory.path(), (1 << 31) | (1 << 22));
    let path = directory.path().join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let material = &mut document["materials"][0];
    material.as_object_mut().unwrap().remove("normalTexture");
    material["extras"]["openSkyrim"] = serde_json::json!({
        "shaderFamily":"lighting", "lightingShaderType":"default",
        "shaderFlags1":(1u32 << 31) | (1 << 22), "shaderFlags2":1
    });
    material["pbrMetallicRoughness"] = serde_json::json!({"baseColorTexture":{"index":1}});
    material["emissiveFactor"] = serde_json::json!([0.4980393, 0.4078432, 0.1725490]);
    material["alphaMode"] = serde_json::json!("MASK");
    material["alphaCutoff"] = serde_json::json!(0.4980392);
    fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
    let mut app = loader_app(directory.path());
    let mut world = load_scene_world(&mut app);
    let mut query = world.query::<(&MeshMaterial3d<StandardMaterial>, &NifSourceMaterial)>();
    let (native, stock) = query.single(&world).unwrap();
    assert_ne!(native.0, stock.0, "the scene must draw the native material");
    let materials = app.world().resource::<Assets<StandardMaterial>>();
    let native = materials.get(&native.0).unwrap();
    let stock = materials.get(&stock.0).unwrap();
    assert_eq!(native.emissive, stock.emissive);
    assert_eq!(native.emissive_texture, stock.base_color_texture);
    assert_eq!(native.base_color_texture, stock.base_color_texture);
    assert_eq!(native.alpha_mode, stock.alpha_mode);
    assert!(stock.emissive_texture.is_none());
    let image = app
        .world()
        .resource::<Assets<Image>>()
        .get(native.emissive_texture.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        image.texture_descriptor.format,
        TextureFormat::Rgba8UnormSrgb
    );
    assert_configured_sampler(image);
}

/// Alvor's hair tint as `00013475.nif` stores it: twice his `HCLF` colour.
const ALVOR_HAIR: [f32; 3] = [0.439_215_7, 0.462_745_1, 0.345_098_05];

#[test]
fn hair_loads_as_a_tinted_skin_material_with_its_tangent_space_maps() {
    let directory = tempfile::tempdir().unwrap();
    write_hair_gltf(directory.path(), 1 << 31, 1, Some(ALVOR_HAIR));
    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut query = scene_world.query::<(
        &MeshMaterial3d<SkyrimSkinMaterial>,
        &NifNativeMaterialSource,
        Option<&MeshMaterial3d<StandardMaterial>>,
        Has<SkinTintSurface>,
    )>();
    let (hair, base, standard, skin_tone) = query.single(&scene_world).unwrap();
    assert!(standard.is_none(), "a mesh draws once per material binding");
    assert!(!skin_tone, "the actor's skin tone does not apply to hair");
    let world = app.world();
    let hair = world
        .resource::<Assets<SkyrimSkinMaterial>>()
        .get(&hair.0)
        .unwrap();
    let base = world
        .resource::<Assets<StandardMaterial>>()
        .get(&base.0)
        .unwrap();
    assert_eq!(hair.extension.settings.flags, SkyrimSkinFlags::HAIR_TINT);
    assert_eq!(
        hair.extension.settings.tint_color,
        Vec3::from_array(ALVOR_HAIR).extend(1.0)
    );
    assert!(hair.extension.model_space_normal.is_none());
    assert!(hair.base.normal_map_texture.is_some());
    assert_eq!(hair.base.normal_map_texture, base.normal_map_texture);
    assert_eq!(hair.base.specular_tint_texture, base.specular_tint_texture);
}

#[test]
fn decal_hair_draws_tinted_under_its_depth_state() {
    let directory = tempfile::tempdir().unwrap();
    // Brows and hairlines: depth test, decal and dynamic decal, no depth write.
    write_hair_gltf(
        directory.path(),
        (1 << 31) | (1 << 26) | (1 << 27),
        0,
        Some(ALVOR_HAIR),
    );
    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut skins = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    assert_eq!(skins.iter(&scene_world).count(), 0);
    let mut depth = scene_world.query::<&MeshMaterial3d<NifDepthMaterial>>();
    assert_eq!(depth.iter(&scene_world).count(), 0);
    let mut query = scene_world.query::<(
        &MeshMaterial3d<SkyrimSkinDepthMaterial>,
        &NifNativeMaterialSource,
    )>();
    let (hair, _) = query.single(&scene_world).unwrap();
    let world = app.world();
    assert!(
        world
            .resource::<AssetServer>()
            .is_loaded_with_dependencies(hair.0.id())
    );
    let hair = world
        .resource::<Assets<SkyrimSkinDepthMaterial>>()
        .get(&hair.0)
        .unwrap();
    assert!(hair.extension.state.decal);
    assert!(!hair.extension.state.depth_write);
    assert_eq!(
        hair.base.extension.settings.flags,
        SkyrimSkinFlags::HAIR_TINT
    );
    assert!(hair.base.base.normal_map_texture.is_some());
}

#[test]
fn hair_from_an_older_pack_keeps_the_plain_materials() {
    let directory = tempfile::tempdir().unwrap();
    write_hair_gltf(directory.path(), (1 << 31) | (1 << 26), 0, None);
    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut depth = scene_world.query::<&MeshMaterial3d<NifDepthMaterial>>();
    assert_eq!(depth.iter(&scene_world).count(), 1);
    let mut hair = scene_world.query::<&MeshMaterial3d<SkyrimSkinDepthMaterial>>();
    assert_eq!(hair.iter(&scene_world).count(), 0);

    let directory = tempfile::tempdir().unwrap();
    write_hair_gltf(directory.path(), 1 << 31, 1, None);
    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut standard = scene_world.query::<&MeshMaterial3d<StandardMaterial>>();
    assert_eq!(standard.iter(&scene_world).count(), 1);
    let mut skins = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    assert_eq!(skins.iter(&scene_world).count(), 0);
}

#[test]
fn default_white_nif_skin_colour_does_not_enable_skin_tint() {
    let directory = tempfile::tempdir().unwrap();
    write_model_space_gltf(directory.path(), (1 << 31) | (1 << 12) | 1);
    let path = directory.path().join("body.gltf");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["materials"][0]["extensions"] = serde_json::json!({
        "OPEN_SKYRIM_material": {
            "lightingShaderType": "skin_tint",
            "tintColor": [1.0, 1.0, 1.0]
        },
        "KHR_materials_specular": {
            "specularFactor": 1.0,
            "specularColorTexture": {"index": 1}
        }
    });
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();

    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut query = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    let binding = query.single(&scene_world).unwrap();
    let material = app
        .world()
        .resource::<Assets<SkyrimSkinMaterial>>()
        .get(&binding.0)
        .unwrap();
    assert_eq!(
        material.extension.settings.flags & SkyrimSkinFlags::SKIN_TINT,
        0,
        "default-white NIF data must not fabricate an NPC skin tone"
    );
}

#[test]
fn a_loaded_masked_hair_card_keeps_its_cutoff_and_specialises_as_hair() {
    let directory = tempfile::tempdir().unwrap();
    write_masked_hair_gltf(directory.path(), 0.3);
    let mut app = loader_app(directory.path());
    let mut scene_world = load_scene_world(&mut app);
    let mut query = scene_world.query::<&MeshMaterial3d<SkyrimSkinMaterial>>();
    let handle = query.single(&scene_world).unwrap().0.clone();
    let materials = app.world().resource::<Assets<SkyrimSkinMaterial>>();
    let material = materials.get(&handle).unwrap();
    // The depth prepass and shadows alpha-test at this cutoff; only the colour pass under MSAA
    // switches to coverage, which the hair key selects.
    assert_eq!(material.base.alpha_mode, AlphaMode::Mask(0.3));
    assert!(SkyrimSkinKey::from(&material.extension).hair);
}

#[test]
fn palette_scenes_keep_labeled_image_dependencies_alive() {
    for depth_wrapped in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write_model_space_gltf(directory.path(), 0);
        let path = directory.path().join("body.gltf");
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["extensionsUsed"] = serde_json::json!(["OPEN_SKYRIM_material"]);
        document["materials"][0] = serde_json::json!({
            "extensions": {"OPEN_SKYRIM_material": {
                "shaderFamily": "effect", "shaderFlags1": 16,
                "textureSlots": [{"semantic": "greyscale", "texture": 1}]
            }}
        });
        if depth_wrapped {
            document["materials"][0]["extras"] = serde_json::json!({"openSkyrim": {
                "shaderFlags1": 1u32 << 31, "shaderFlags2": 0
            }});
        }
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut app = loader_app(directory.path());
        let scene: Handle<WorldAsset> = app
            .world()
            .resource::<AssetServer>()
            .load(GltfAssetLabel::Scene(0).from_asset("body.gltf"));
        wait_for_dependencies(&mut app, &scene);
        let holder_id = app
            .world()
            .resource::<AssetServer>()
            .get_path_id("body.gltf#Material0/nif_palette_image")
            .expect("labeled palette holder remains registered")
            .typed::<EffectPaletteImage>();
        let mut scene_world = app
            .world_mut()
            .resource_mut::<Assets<WorldAsset>>()
            .remove(&scene)
            .unwrap()
            .world;
        let material = if depth_wrapped {
            let mut query = scene_world.query::<&MeshMaterial3d<EffectPaletteDepthMaterial>>();
            query.single(&scene_world).unwrap().0.clone().untyped()
        } else {
            let mut query = scene_world.query::<&MeshMaterial3d<EffectPaletteMaterial>>();
            query.single(&scene_world).unwrap().0.clone().untyped()
        };
        drop(scene_world);
        drop(scene);
        for _ in 0..10 {
            app.update();
        }
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteImage>>()
                .get(holder_id)
                .is_some(),
            "the material must keep its palette holder alive after the scene is dropped"
        );
        drop(material);
        for _ in 0..10 {
            app.update();
        }
        assert!(
            app.world()
                .resource::<Assets<EffectPaletteImage>>()
                .get(holder_id)
                .is_none(),
            "the holder should be released after its material is dropped"
        );
    }
}
