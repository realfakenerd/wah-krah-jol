use crate::{
    color_pipeline::{DEFAULT_SCENE_EV100, SceneColorPipeline},
    profiling::ProfilingState,
    world::{cache::TerrainSnapshot, database::AssetCatalog},
};
use bevy::{
    app::{HierarchyPropagatePlugin, PropagateOver, PropagateSet},
    asset::embedded_asset,
    camera::{
        Exposure, RenderTarget,
        primitives::{Aabb, Frustum},
        visibility::{Layer, RenderLayers, VisibilitySystems},
    },
    core_pipeline::{mip_generation::experimental::depth::ViewDepthPyramid, prepass::DepthPrepass},
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    pbr::{
        ExtendedMaterial, ExtractedDirectionalLight, LightEntity, MaterialExtension, ShadowView,
        prepare_lights,
    },
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        batching::gpu_preprocessing::{
            GpuPreprocessingMode, GpuPreprocessingSupport, IndirectParametersBuffers,
        },
        occlusion_culling::OcclusionCulling,
        render_resource::{AsBindGroup, ShaderType},
    },
    shader::ShaderRef,
};
use serde::Serialize;
use shared::LAND_TEXTURE_REPEATS_PER_CELL;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainExtension>;
pub type WaterMaterial = ExtendedMaterial<StandardMaterial, WaterExtension>;

/// How far the procedural waves tilt the water's normal. Skyrim's water is close to flat at a
/// distance; a stronger tilt striped lakes with bright and dark bands.
pub(crate) const WAVE_STRENGTH: f32 = 0.05;

/// Skyrim's DefaultWater after Update.esm: Fresnel Amount 0.10 and a Reflectivity Amount of 0.8
/// (WATR `DNAM`). Used when a water has no decoded colours yet, e.g. a database converted before
/// `crates/converter` started reading them.
pub const DEFAULT_WATER_FRESNEL: f32 = 0.10;
pub const DEFAULT_WATER_REFLECTIVITY: f32 = 0.8;
/// DefaultWater's Reflection Color after Update.esm (`#778C9D`): what the water reflects where the
/// reflection image holds nothing, and what a water with no decoded colours falls back to.
pub const DEFAULT_WATER_REFLECTION_COLOR: [u8; 3] = [0x77, 0x8C, 0x9D];

fn water_reflection_color(rgb: [u8; 3]) -> Vec4 {
    let linear = Color::srgb_u8(rgb[0], rgb[1], rgb[2]).to_linear();
    Vec4::new(linear.red, linear.green, linear.blue, 0.0)
}

/// One rendering layer per role, so no view list is a bare literal.
///
/// Layer 0 is Bevy's default and holds the world itself: terrain today, and - when they land - the
/// distant LOD land, LOD objects, LOD trees and the sky. Water surfaces take their own layer so a
/// water plane can never reflect another one. Full-detail placed objects take a third layer so the
/// reflection pass can leave them out; see [`REFLECTION_VIEW_LAYERS`] for why.
pub const WORLD_LAYER: Layer = 0;
pub const WATER_LAYER: Layer = 1;
pub const PLACED_OBJECT_LAYER: Layer = 2;

/// Everything the player's own cameras draw: terrain, water and the placed objects.
pub const MAIN_VIEW_LAYERS: &[Layer] = &[WORLD_LAYER, WATER_LAYER, PLACED_OBJECT_LAYER];

/// What the water reflection camera draws.
///
/// Vanilla Skyrim builds its water reflection from the LOD world - LOD land, LOD objects and LOD
/// trees - plus the sky, selected by the `[Water]` settings `bReflectLODLand`, `bReflectLODObjects`,
/// `bReflectLODTrees` and `bReflectSky`; full-detail placed objects, actors and grass are not in it
/// (SE adds screen-space reflections for near geometry, which this engine does not have). This
/// engine has no LOD and no sky yet, so what vanilla reflects is the terrain: the world layer only.
/// LOD and the sky join [`WORLD_LAYER`] when they land, and a full-detail object stays out of the
/// reflection because it is on [`PLACED_OBJECT_LAYER`].
pub const REFLECTION_VIEW_LAYERS: &[Layer] = &[WORLD_LAYER];

/// What every scene light belongs to: the sun today, and any point light a cell adds later.
///
/// A light must intersect a view's layers to light it, so this has to keep [`WORLD_LAYER`] - both
/// cameras render it. It also has to include [`PLACED_OBJECT_LAYER`], because a light only collects
/// shadow casters that share a layer with it (`check_dir_light_mesh_visibility`,
/// `bevy_light-0.19.0/src/lib.rs:423`). Dropping the placed-object layer would light the objects
/// while silently removing them from the sun's shadow cascades.
pub const LIGHT_LAYERS: &[Layer] = &[WORLD_LAYER, PLACED_OBJECT_LAYER];

/// The render layers for a light spawned below a placed-object reference.
///
/// The reference's [`Propagate`](bevy::app::Propagate) would otherwise copy
/// [`PLACED_OBJECT_RENDER_LAYERS`] onto the light and drop it from [`WORLD_LAYER`], breaking the
/// [`LIGHT_LAYERS`] contract. [`PropagateOver`] keeps the propagated value off this entity only
/// (`PropagateStop` would still write it here and just stop the walk below).
pub fn light_render_layers() -> (RenderLayers, PropagateOver<RenderLayers>) {
    (
        RenderLayers::from_layers(LIGHT_LAYERS),
        PropagateOver::default(),
    )
}

/// The value [`Propagate`](bevy::app::Propagate) copies onto the meshes below a placed-object
/// reference, which is what keeps those meshes out of the reflection pass.
pub const PLACED_OBJECT_RENDER_LAYERS: RenderLayers = RenderLayers::layer(PLACED_OBJECT_LAYER);

/// Registers the propagation of [`PLACED_OBJECT_RENDER_LAYERS`] down reference hierarchies.
///
/// A reference entity is spawned in `streaming::spawn_cell` and the meshes it draws arrive later as
/// descendants from the converted glb, so `RenderLayers` on the reference alone would not reach
/// them. The propagation runs in `PostUpdate` before [`VisibilitySystems::CheckVisibility`]:
/// visibility compares render layers after that, and the glTF scene children are spawned in
/// `SpawnScene` between `Update` and `PostUpdate`, so the layers are in place in the first frame a
/// mesh exists - including for descendants spawned after the reference.
pub fn add_placed_object_layer_propagation(app: &mut App) {
    app.add_plugins(HierarchyPropagatePlugin::<RenderLayers>::new(PostUpdate))
        .configure_sets(
            PostUpdate,
            PropagateSet::<RenderLayers>::default().before(VisibilitySystems::CheckVisibility),
        );
}

pub struct VercidiumRendererPlugin;

impl Plugin for VercidiumRendererPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/terrain.wgsl");
        embedded_asset!(app, "shaders/water.wgsl");
        app.add_plugins((
            MaterialPlugin::<TerrainMaterial>::default(),
            MaterialPlugin::<WaterMaterial>::default(),
            crate::prepass_vertex_alpha::PrepassVertexAlphaPlugin,
        ))
        .init_resource::<RendererMetrics>()
        .add_systems(Startup, setup_water_reflection)
        .add_systems(
            Update,
            (
                animate_water_materials,
                update_water_reflection_camera,
                sync_renderer_metrics,
            )
                .after(crate::settings_ui::SettingsUiSet),
        );

        add_placed_object_layer_propagation(app);

        let bridge = RendererProofBridge::default();
        app.insert_resource(bridge.clone());
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(bridge)
                .add_systems(
                    Render,
                    sample_renderer_path.after(RenderSystems::PrepareResourcesCollectPhaseBuffers),
                )
                .add_systems(
                    Render,
                    give_shadow_views_their_light_layers
                        .in_set(RenderSystems::CreateViews)
                        .after(prepare_lights),
                );
        }
    }
}

/// A shadow view Bevy created without render layers.
type UnlayeredShadowView = (With<ShadowView>, Without<RenderLayers>);

/// Gives each directional shadow cascade the render layers of the light that owns it.
///
/// Bevy 0.19 builds a cascade's shadow view without a `RenderLayers` component, and
/// `queue_shadows` tests every caster's layers against that view's, defaulting to layer 0 when it
/// has none. The CPU side (`check_dir_light_mesh_visibility`) does use the light's layers, so a
/// caster on [`PLACED_OBJECT_LAYER`] is collected for the cascade and then dropped before it is
/// queued: none of the placed objects - buildings, trees, rocks - would cast a shadow. Copying the
/// light's layers onto its views makes the queue test agree with the collection test.
fn give_shadow_views_their_light_layers(
    mut commands: Commands,
    views: Query<(Entity, &LightEntity), UnlayeredShadowView>,
    lights: Query<&ExtractedDirectionalLight>,
) {
    for (view, owner) in &views {
        let LightEntity::Directional { light_entity, .. } = owner else {
            continue;
        };
        if let Ok(light) = lights.get(*light_entity) {
            commands.entity(view).insert(light.render_layers.clone());
        }
    }
}

#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct RendererMetrics {
    pub gpu_preprocessing_active: bool,
    pub gpu_culling_active: bool,
    pub indirect_drawing_active: bool,
    pub occlusion_culling_views: u64,
    pub hzb_views: u64,
    pub indirect_phase_buffers: u64,
    pub indirect_batch_sets: u64,
    pub proof_frames: u64,
    pub renderer_fixture_validated: bool,
    pub renderer_validation_failures: u64,
}

impl RendererMetrics {
    pub fn final_path_active(&self) -> bool {
        self.gpu_preprocessing_active
            && self.gpu_culling_active
            && self.indirect_drawing_active
            && self.occlusion_culling_views > 0
            && self.hzb_views > 0
            && self.indirect_phase_buffers > 0
            && self.indirect_batch_sets > 0
            && self.proof_frames > 0
            && self.renderer_validation_failures == 0
    }
}

#[derive(Default)]
struct RendererProofState {
    gpu_preprocessing: AtomicBool,
    gpu_culling: AtomicBool,
    indirect_drawing: AtomicBool,
    occlusion_views: AtomicU64,
    hzb_views: AtomicU64,
    indirect_phases: AtomicU64,
    indirect_batch_sets: AtomicU64,
    frames: AtomicU64,
}

#[derive(Resource, Clone, Default)]
struct RendererProofBridge(Arc<RendererProofState>);

fn sample_renderer_path(
    bridge: Res<RendererProofBridge>,
    support: Option<Res<GpuPreprocessingSupport>>,
    indirect: Option<Res<IndirectParametersBuffers>>,
    views: Query<Option<&ViewDepthPyramid>, With<OcclusionCulling>>,
) {
    let Some(support) = support else { return };
    let preprocessing = support.is_available();
    let culling = support.max_supported_mode == GpuPreprocessingMode::Culling;
    let (phase_count, batch_sets, indirect_active) =
        indirect.as_deref().map_or((0, 0, false), |buffers| {
            let phases = buffers.len() as u64;
            let batch_sets = buffers
                .values()
                .map(|phase| {
                    phase.batch_set_count(true) as u64 + phase.batch_set_count(false) as u64
                })
                .sum::<u64>();
            let active = buffers.values().any(|phase| {
                phase.indexed.data_buffer().is_some() || phase.non_indexed.data_buffer().is_some()
            });
            (phases, batch_sets, active)
        });
    let occlusion_views = views.iter().count() as u64;
    let hzb_views = views.iter().filter(|pyramid| pyramid.is_some()).count() as u64;
    bridge
        .0
        .gpu_preprocessing
        .fetch_or(preprocessing, Ordering::Relaxed);
    bridge.0.gpu_culling.fetch_or(culling, Ordering::Relaxed);
    bridge
        .0
        .indirect_drawing
        .fetch_or(indirect_active, Ordering::Relaxed);
    bridge
        .0
        .occlusion_views
        .fetch_max(occlusion_views, Ordering::Relaxed);
    bridge.0.hzb_views.fetch_max(hzb_views, Ordering::Relaxed);
    bridge
        .0
        .indirect_phases
        .fetch_max(phase_count, Ordering::Relaxed);
    bridge
        .0
        .indirect_batch_sets
        .fetch_max(batch_sets, Ordering::Relaxed);
    bridge.0.frames.fetch_add(1, Ordering::Relaxed);
}

fn sync_renderer_metrics(bridge: Res<RendererProofBridge>, mut metrics: ResMut<RendererMetrics>) {
    metrics.gpu_preprocessing_active = bridge.0.gpu_preprocessing.load(Ordering::Relaxed);
    metrics.gpu_culling_active = bridge.0.gpu_culling.load(Ordering::Relaxed);
    metrics.indirect_drawing_active = bridge.0.indirect_drawing.load(Ordering::Relaxed);
    metrics.occlusion_culling_views = bridge.0.occlusion_views.load(Ordering::Relaxed);
    metrics.hzb_views = bridge.0.hzb_views.load(Ordering::Relaxed);
    metrics.indirect_phase_buffers = bridge.0.indirect_phases.load(Ordering::Relaxed);
    metrics.indirect_batch_sets = bridge.0.indirect_batch_sets.load(Ordering::Relaxed);
    metrics.proof_frames = bridge.0.frames.load(Ordering::Relaxed);
}

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct TerrainExtension {
    #[texture(100)]
    layer_0: Option<Handle<Image>>,
    #[texture(102)]
    layer_1: Option<Handle<Image>>,
    #[texture(104)]
    layer_2: Option<Handle<Image>>,
    #[texture(106)]
    layer_3: Option<Handle<Image>>,
    #[texture(108)]
    layer_4: Option<Handle<Image>>,
    #[texture(110)]
    layer_5: Option<Handle<Image>>,
    // LAND images share one repeating sampler. Use the first available color layer so an
    // untextured base does not select Bevy's clamping fallback sampler. AsBindGroup requires a
    // texture on the sampler's field; binding 119 supplies it without another image allocation.
    #[texture(119)]
    #[sampler(101)]
    sampler_source: Option<Handle<Image>>,
    #[uniform(112)]
    settings: TerrainSettings,
    // Each layer's normal map, sampled through `sampler_source`: every terrain layer image is
    // loaded with the same repeating sampler (`terrain_layer_sampler`), so the six need no sampler
    // bindings of their own. Which layers have one is in `TerrainSettings::normal_layers_*`, since
    // Bevy's stand-in for a missing texture is white, which decodes to a steep tilt.
    #[texture(113)]
    normal_0: Option<Handle<Image>>,
    #[texture(114)]
    normal_1: Option<Handle<Image>>,
    #[texture(115)]
    normal_2: Option<Handle<Image>>,
    #[texture(116)]
    normal_3: Option<Handle<Image>>,
    #[texture(117)]
    normal_4: Option<Handle<Image>>,
    #[texture(118)]
    normal_5: Option<Handle<Image>>,
}

#[derive(ShaderType, Reflect, Debug, Clone)]
struct TerrainSettings {
    tiling_and_layer_count: Vec4,
    quadrant_origin: Vec4,
    fallback_weights_0: Vec4,
    fallback_weights_1: Vec4,
    weight_source: Vec4,
    /// 1.0 in component `i` when layer `i` (0-3) has a normal map bound, else 0.0.
    normal_layers_0: Vec4,
    /// Layers 4 and 5 in x and y.
    normal_layers_1: Vec4,
    weights: [Vec4; WEIGHT_FIELD_WORDS],
}

/// The sample grid of one terrain quadrant: `17x17` opacities on the same `128`-unit spacing as the
/// quadrant's mesh columns, which is what LAND's `VTXT` entries index.
pub(crate) const QUADRANT_WEIGHT_SAMPLES: usize = 17;

/// Overlay layers a quadrant can carry beside its base: the runtime's cap, since
/// [`crate::streaming::quadrant_layers`] rejects a quadrant with more than six layers in total.
pub(crate) const OVERLAY_WEIGHT_SLOTS: usize = 5;

/// `Vec4`s per overlay in the weight field: one [`QUADRANT_WEIGHT_SAMPLES`]-square grid packed four
/// samples to a `Vec4` and rounded up to whole `Vec4`s, so every overlay starts on a 16-byte
/// boundary. The shader declares the same number (`WEIGHT_GRID_WORDS` in `terrain.wgsl`).
pub(crate) const OVERLAY_WEIGHT_WORDS: usize =
    (QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES).div_ceil(4);

/// The whole field one material carries: [`OVERLAY_WEIGHT_SLOTS`] overlays of
/// [`OVERLAY_WEIGHT_WORDS`] words each. The shader declares the same number as the length of its
/// `weights` array.
pub(crate) const WEIGHT_FIELD_WORDS: usize = OVERLAY_WEIGHT_SLOTS * OVERLAY_WEIGHT_WORDS;

/// The sampler a terrain layer texture needs: the shader tiles every layer `tiling` times across a
/// cell (`uv * LAND_TEXTURE_REPEATS_PER_CELL`), so the address mode must repeat.
/// Bevy's default sampler clamps to the edge,
/// which stretches a texture's last texel column, row and corner across everything past the first
/// tile - long streaks where the edge column is stretched, and one flat colour where the corner
/// texel covers the rest. A terrain texture that is not loaded through the asset server - the
/// synthetic fixtures build theirs in memory - has to be given the same sampler by hand.
pub(crate) fn terrain_layer_sampler() -> ImageSamplerDescriptor {
    ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        address_mode_w: ImageAddressMode::Repeat,
        anisotropy_clamp: 16,
        ..ImageSamplerDescriptor::linear()
    }
}

/// Packs a quadrant's overlay weight grids into the field the material uniform carries: sample `s`
/// of overlay `o` lands in component `s % 4` of word `o * OVERLAY_WEIGHT_WORDS + s / 4`, which is
/// where `grid_weight` in `terrain.wgsl` reads it. Grids beyond [`OVERLAY_WEIGHT_SLOTS`] and
/// samples beyond one grid are dropped, so a malformed grid cannot write outside the field.
fn weight_field(overlay_weights: &[Vec<f32>]) -> [Vec4; WEIGHT_FIELD_WORDS] {
    let mut field = [Vec4::ZERO; WEIGHT_FIELD_WORDS];
    for (overlay, grid) in overlay_weights
        .iter()
        .take(OVERLAY_WEIGHT_SLOTS)
        .enumerate()
    {
        for (sample, weight) in grid
            .iter()
            .enumerate()
            .take(QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES)
        {
            field[overlay * OVERLAY_WEIGHT_WORDS + sample / 4][sample % 4] = *weight;
        }
    }
    field
}

impl TerrainSettings {
    /// Marks which layers have a normal map bound, for the shader to blend only those.
    fn set_normal_layers(&mut self, normals: &[Option<Handle<Image>>; 6]) {
        let present = |index: usize| if normals[index].is_some() { 1.0 } else { 0.0 };
        self.normal_layers_0 = Vec4::new(present(0), present(1), present(2), present(3));
        self.normal_layers_1 = Vec4::new(present(4), present(5), 0.0, 0.0);
    }

    /// The settings of one quadrant of `terrain`: the tiling the shader repeats every layer by, the
    /// quadrant's origin inside the cell, and its overlay weight field. The shader interpolates the
    /// field itself, so a weight reaches the fragment stage exactly as LAND records it instead of
    /// through a vertex attribute Bevy re-normalizes.
    fn for_quadrant(quadrant: u8, layers: usize, overlay_weights: &[Vec<f32>]) -> Self {
        Self {
            tiling_and_layer_count: Vec4::new(
                LAND_TEXTURE_REPEATS_PER_CELL,
                LAND_TEXTURE_REPEATS_PER_CELL,
                layers as f32,
                0.0,
            ),
            quadrant_origin: Vec4::new(f32::from(quadrant % 2), f32::from(quadrant / 2), 0.0, 0.0),
            fallback_weights_0: Vec4::X,
            fallback_weights_1: Vec4::ZERO,
            // A quadrant with no overlay has nothing in the field to read, so the shader is told to
            // skip it: the packed attributes carry the same (empty) overlays.
            normal_layers_0: Vec4::ZERO,
            normal_layers_1: Vec4::ZERO,
            weight_source: if overlay_weights.is_empty() {
                Vec4::ZERO
            } else {
                Vec4::X
            },
            weights: weight_field(overlay_weights),
        }
    }

    /// The settings of a material that carries no weight field, so the shader reads the weights the
    /// mesh packs into its vertex attributes instead. Only the stand-in terrain a synthetic scene
    /// draws without a LAND snapshot - the streaming and benchmark fixtures - uses it.
    fn vertex_weights_only(layers: f32) -> Self {
        Self {
            tiling_and_layer_count: Vec4::new(
                LAND_TEXTURE_REPEATS_PER_CELL,
                LAND_TEXTURE_REPEATS_PER_CELL,
                layers,
                0.0,
            ),
            quadrant_origin: Vec4::ZERO,
            fallback_weights_0: Vec4::X,
            fallback_weights_1: Vec4::ZERO,
            normal_layers_0: Vec4::ZERO,
            normal_layers_1: Vec4::ZERO,
            weight_source: Vec4::ZERO,
            weights: [Vec4::ZERO; WEIGHT_FIELD_WORDS],
        }
    }
}

/// The images a terrain quadrant's material waits on, by the colour space each
/// must decode to.
pub struct TerrainImages {
    /// The layers' diffuse images, sRGB.
    pub color: Vec<Handle<Image>>,
    /// The layers' normal maps, linear.
    pub normal: Vec<Handle<Image>>,
}

impl TerrainExtension {
    /// The exact image resources retained by this quadrant's material.
    pub(crate) fn image_ids(&self) -> impl Iterator<Item = bevy::asset::AssetId<Image>> + '_ {
        [
            self.layer_0.as_ref(),
            self.layer_1.as_ref(),
            self.layer_2.as_ref(),
            self.layer_3.as_ref(),
            self.layer_4.as_ref(),
            self.layer_5.as_ref(),
            self.normal_0.as_ref(),
            self.normal_1.as_ref(),
            self.normal_2.as_ref(),
            self.normal_3.as_ref(),
            self.normal_4.as_ref(),
            self.normal_5.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(Handle::id)
    }

    /// Retains slots and weights for zero IDs and unresolved diffuse images without binding
    /// either a diffuse or a normal image for those layers.
    pub fn from_quadrant(
        terrain: &TerrainSnapshot,
        quadrant: u8,
        catalog: &AssetCatalog,
        asset_server: &AssetServer,
    ) -> Result<(Self, TerrainImages), String> {
        let mut textures: [Option<Handle<Image>>; 6] = std::array::from_fn(|_| None);
        let mut normals: [Option<Handle<Image>>; 6] = std::array::from_fn(|_| None);
        let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
        let mut handles = Vec::with_capacity(layers.len());
        let mut normal_handles = Vec::new();
        for ((target, normal), layer) in textures.iter_mut().zip(normals.iter_mut()).zip(&layers) {
            if layer.texture_form_id == 0 {
                // A preserved unresolved layer keeps its slot and weights with no image bound.
                continue;
            }
            let Some(path) = catalog.landscape_diffuse(layer.texture_form_id) else {
                // Keep the unresolved layer's weight without loading an orphan normal map.
                continue;
            };
            if let Some(path) = catalog.landscape_normal(layer.texture_form_id) {
                // A normal map holds directions, not colours: it must not be decoded as sRGB.
                let handle = asset_server
                    .load_builder()
                    .with_settings(|settings: &mut ImageLoaderSettings| {
                        settings.sampler = ImageSampler::Descriptor(terrain_layer_sampler());
                        settings.is_srgb = false;
                    })
                    .load(path.to_owned());
                *normal = Some(handle.clone());
                normal_handles.push(handle);
            }
            let handle = asset_server
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.sampler = ImageSampler::Descriptor(terrain_layer_sampler());
                })
                .load(path.to_owned());
            *target = Some(handle.clone());
            handles.push(handle);
        }
        let overlay_weights = crate::streaming::quadrant_overlay_weights(terrain, quadrant)?;
        let mut settings = TerrainSettings::for_quadrant(quadrant, layers.len(), &overlay_weights);
        settings.set_normal_layers(&normals);
        Ok((
            Self {
                layer_0: textures[0].clone(),
                layer_1: textures[1].clone(),
                layer_2: textures[2].clone(),
                layer_3: textures[3].clone(),
                layer_4: textures[4].clone(),
                layer_5: textures[5].clone(),
                sampler_source: textures.iter().flatten().next().cloned(),
                settings,
                normal_0: normals[0].clone(),
                normal_1: normals[1].clone(),
                normal_2: normals[2].clone(),
                normal_3: normals[3].clone(),
                normal_4: normals[4].clone(),
                normal_5: normals[5].clone(),
            },
            TerrainImages {
                color: handles,
                normal: normal_handles,
            },
        ))
    }

    /// The material of one quadrant of a synthetic `terrain`, drawn with textures a fixture already
    /// holds in memory. It carries the same settings a streamed quadrant does, so a fixture with
    /// overlay layers shows the weight field the shader interpolates rather than only the packed
    /// vertex attributes it falls back on.
    pub(crate) fn fixture(
        terrain: &TerrainSnapshot,
        quadrant: u8,
        textures: [Handle<Image>; 6],
    ) -> Result<Self, String> {
        let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
        let overlay_weights = crate::streaming::quadrant_overlay_weights(terrain, quadrant)?;
        Ok(Self {
            layer_0: Some(textures[0].clone()),
            layer_1: Some(textures[1].clone()),
            layer_2: Some(textures[2].clone()),
            layer_3: Some(textures[3].clone()),
            layer_4: Some(textures[4].clone()),
            layer_5: Some(textures[5].clone()),
            sampler_source: Some(textures[0].clone()),
            settings: TerrainSettings::for_quadrant(quadrant, layers.len(), &overlay_weights),
            normal_0: None,
            normal_1: None,
            normal_2: None,
            normal_3: None,
            normal_4: None,
            normal_5: None,
        })
    }

    /// Whether the shader reads this material's overlay weights from its uniform weight field rather
    /// than from the weights the mesh packs into its vertex attributes. False only for a material
    /// with no overlays to read: a quadrant whose sole layer is its base, or a stand-in terrain.
    pub(crate) fn reads_weight_field(&self) -> bool {
        self.settings.weight_source.x > 0.5
    }

    /// Unbinds every layer's normal map, so the quadrant is lit by its geometric normal as a
    /// quadrant without normal maps is. Used when a normal map fails to load: a normal map is
    /// optional detail, and losing one must not hide the terrain its diffuse layers can draw.
    pub(crate) fn drop_normal_maps(&mut self) {
        let none: [Option<Handle<Image>>; 6] = Default::default();
        self.settings.set_normal_layers(&none);
        [
            self.normal_0,
            self.normal_1,
            self.normal_2,
            self.normal_3,
            self.normal_4,
            self.normal_5,
        ] = none;
    }

    /// Whether any layer has a normal map bound.
    #[cfg(test)]
    pub(crate) fn has_normal_maps(&self) -> bool {
        self.settings.normal_layers_0 != Vec4::ZERO || self.settings.normal_layers_1 != Vec4::ZERO
    }
}

impl Default for TerrainExtension {
    fn default() -> Self {
        Self {
            layer_0: None,
            layer_1: None,
            layer_2: None,
            layer_3: None,
            layer_4: None,
            layer_5: None,
            sampler_source: None,
            settings: TerrainSettings::vertex_weights_only(0.0),
            normal_0: None,
            normal_1: None,
            normal_2: None,
            normal_3: None,
            normal_4: None,
            normal_5: None,
        }
    }
}

impl MaterialExtension for TerrainExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/terrain.wgsl".into()
    }
}

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct WaterExtension {
    #[uniform(100)]
    settings: WaterSettings,
    #[texture(101)]
    #[sampler(102)]
    reflection: Option<Handle<Image>>,
    #[texture(103)]
    #[sampler(104)]
    flow_normal: Option<Handle<Image>>,
}

#[derive(ShaderType, Reflect, Debug, Clone)]
struct WaterSettings {
    wave_scale_speed_strength: Vec4,
    flow_direction: Vec4,
    /// x = Fresnel Amount (Schlick F0), y = Reflectivity Amount. z/w unused.
    fresnel_reflectivity: Vec4,
    /// The water's Reflection Color (WATR DNAM), linear. What the reflection fades to where the
    /// reflection image has no coverage; alpha unused.
    reflection_color: Vec4,
}

impl Default for WaterExtension {
    fn default() -> Self {
        Self {
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, WAVE_STRENGTH, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, 0.0),
                fresnel_reflectivity: Vec4::new(
                    DEFAULT_WATER_FRESNEL,
                    DEFAULT_WATER_REFLECTIVITY,
                    1.0,
                    0.0,
                ),
                reflection_color: water_reflection_color(DEFAULT_WATER_REFLECTION_COLOR),
            },
            reflection: None,
            flow_normal: None,
        }
    }
}

impl WaterExtension {
    /// Per-cell dependencies. The shared reflection target belongs to the
    /// renderer baseline and must not keep a retired water cell reserved.
    pub(crate) fn image_ids(&self) -> impl Iterator<Item = bevy::asset::AssetId<Image>> + '_ {
        self.flow_normal.iter().map(Handle::id)
    }

    /// Builds a water material with Skyrim's DefaultWater fresnel and reflectivity. Callers that
    /// know a water's own factors (from [`crate::world::database::AssetCatalog::water_colors`])
    /// should use [`Self::with_reflection_and_factors`] instead.
    pub fn with_reflection(reflection: Handle<Image>, flow_normal: Option<Handle<Image>>) -> Self {
        Self::with_reflection_and_factors(
            reflection,
            flow_normal,
            DEFAULT_WATER_FRESNEL,
            DEFAULT_WATER_REFLECTIVITY,
        )
    }

    pub fn with_reflection_and_factors(
        reflection: Handle<Image>,
        flow_normal: Option<Handle<Image>>,
        fresnel: f32,
        reflectivity: f32,
    ) -> Self {
        let has_flow_normal = flow_normal.is_some() as u8 as f32;
        Self {
            reflection: Some(reflection),
            flow_normal,
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, WAVE_STRENGTH, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, has_flow_normal),
                // z gates the reflection strength; 1.0 keeps reflections on until settings set it.
                fresnel_reflectivity: Vec4::new(fresnel, reflectivity, 1.0, 0.0),
                reflection_color: water_reflection_color(DEFAULT_WATER_REFLECTION_COLOR),
            },
        }
    }

    /// Sets the water's own Reflection Color (WATR DNAM, as stored bytes).
    pub fn with_reflection_color(mut self, rgb: [u8; 3]) -> Self {
        self.settings.reflection_color = water_reflection_color(rgb);
        self
    }
}

impl MaterialExtension for WaterExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/water.wgsl".into()
    }
}

fn animate_water_materials(
    time: Res<Time>,
    config: Option<Res<crate::config::EngineConfig>>,
    mut materials: ResMut<Assets<WaterMaterial>>,
    mut profiler: ResMut<ProfilingState>,
    settings: Option<Res<crate::settings::RenderingSettings>>,
) {
    let started = std::time::Instant::now();
    let elapsed =
        if config.is_some_and(|config| config.terrain_water_fixture || config.shots.is_some()) {
            1.0
        } else {
            time.elapsed_secs()
        };
    for (_, material) in materials.iter_mut() {
        material.extension.settings.wave_scale_speed_strength.w = elapsed;
        if let Some(settings) = settings.as_deref() {
            let water = &mut material.extension;
            water.settings.wave_scale_speed_strength.x = settings.wave_scale;
            water.settings.wave_scale_speed_strength.y = settings.wave_speed;
            water.settings.wave_scale_speed_strength.z = if settings.water_waves {
                settings.wave_strength
            } else {
                0.0
            };
            water.settings.flow_direction.w =
                if settings.water_flow_normals && water.flow_normal.is_some() {
                    1.0
                } else {
                    0.0
                };
            water.settings.fresnel_reflectivity.z = if settings.water_reflections {
                settings.water_reflection_strength
            } else {
                0.0
            };
        }
    }
    profiler.record_elapsed("render/water_animation", started);
}

#[derive(Resource, Clone)]
pub struct WaterReflectionTexture(pub Handle<Image>);

#[derive(Component)]
pub(crate) struct WaterReflectionCamera;

/// Slack added to a water plane's bounds before the main camera frustum test, in world units (4096
/// to a cell).
///
/// The frustum carried by the main camera was built at the end of the previous frame, so a fast
/// turn brings water into a view the gate has already closed for, and the frame that renders it
/// shows the reflection of the frame before. A plane this far outside the frustum still keeps the
/// reflection camera on: seen from one cell away, it buys about seven degrees of extra turn per
/// frame - a flick past 400 degrees a second - or, when the camera moves instead of turning, 512
/// units of travel between two frames. A plane that far off the edge of a 45 degree view is a
/// sliver of the frame, so little of the saving is given back.
const WATER_REFLECTION_FRUSTUM_MARGIN: f32 = 512.0;

/// Spawns the camera that fills [`WaterReflectionTexture`], and the texture it renders into.
///
/// It is the flipped copy of the streaming camera, drawn before the main view, and it renders
/// [`REFLECTION_VIEW_LAYERS`]: the world layer only. What Skyrim puts in a water reflection, and
/// why full-detail placed objects are not in it, is documented on that constant.
fn setup_water_reflection(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(Image::new_target_texture(
        1024,
        576,
        bevy::render::render_resource::TextureFormat::Rgba16Float,
        None,
    ));
    commands.insert_resource(WaterReflectionTexture(image.clone()));
    commands.spawn((
        Camera3d::default(),
        SceneColorPipeline::reflection(),
        Camera {
            order: -1,
            // The camera is placed below the water looking up, not mirrored, so its triangles keep
            // their winding: inverting the culling drew the back faces.
            invert_culling: false,
            is_active: false,
            // Nothing drawn means alpha 0, so the water can tell "no reflected geometry" from
            // reflected geometry and fall back to its Reflection Color there.
            clear_color: ClearColorConfig::Custom(Color::NONE),
            ..default()
        },
        RenderTarget::Image(image.into()),
        Transform::default(),
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(REFLECTION_VIEW_LAYERS),
        WaterReflectionCamera,
    ));
}

/// The streamed world camera, never the reflection camera.
type WaterReflectionObserver = (
    With<crate::world::components::StreamingCamera>,
    Without<WaterReflectionCamera>,
);

/// What the gate drives on the reflection camera: its entity, pose, switch, exposure and lens.
type WaterReflectionCameraParts = (
    Entity,
    &'static mut Transform,
    &'static mut Camera,
    Option<&'static mut Exposure>,
    Option<&'static mut Projection>,
);

/// What the gate reads off the world camera: its pose, the [`Frustum`] Bevy keeps in step with it,
/// and the projection whose far distance the frustum itself does not carry.
type WaterReflectionObserverView = (
    &'static GlobalTransform,
    Option<&'static Frustum>,
    Option<&'static Projection>,
    Option<&'static Exposure>,
);

fn update_water_reflection_camera(
    mut commands: Commands,
    main_camera: Query<WaterReflectionObserverView, WaterReflectionObserver>,
    water: Query<(&GlobalTransform, Option<&Aabb>), With<crate::world::components::WaterSurface>>,
    mut reflection_camera: Query<WaterReflectionCameraParts, With<WaterReflectionCamera>>,
    mut profiler: ResMut<ProfilingState>,
    settings: Option<Res<crate::settings::RenderingSettings>>,
) {
    let started = std::time::Instant::now();
    let (
        Ok((main, frustum, projection, exposure)),
        Ok((entity, mut reflection, mut camera, reflection_exposure, reflection_projection)),
    ) = (main_camera.single(), reflection_camera.single_mut())
    else {
        return;
    };
    // PBR lighting is already exposure-scaled when the reflection is sampled by water.
    // Keep both views in the same domain, including explicit diagnostic exposure changes.
    let ev100 = exposure.map_or(DEFAULT_SCENE_EV100, |value| value.ev100);
    if let Some(mut reflection_exposure) = reflection_exposure {
        if reflection_exposure.ev100 != ev100 {
            reflection_exposure.ev100 = ev100;
        }
    } else {
        commands.entity(entity).insert(Exposure { ev100 });
    }
    // The surface nearest the main camera fixes the mirror plane; a further surface would put the
    // reflection at the wrong height when more than one water level is streamed in.
    let mut mirror_surface = None;
    let mut mirror_distance = f32::INFINITY;
    let mut surface_in_view = false;
    for (surface, bounds) in &water {
        let distance = (surface.translation().y - main.translation().y).abs();
        if distance < mirror_distance {
            mirror_distance = distance;
            mirror_surface = Some(surface);
        }
        surface_in_view |= water_plane_in_view(
            main,
            frustum,
            projection.map(|projection| projection.far()),
            surface,
            bounds,
        );
    }
    let Some(surface) = mirror_surface else {
        if camera.is_active {
            camera.is_active = false;
        }
        return;
    };
    // The mirror plane is refreshed even while the view is gated off, so the frame the gate reopens
    // reflects the camera as it stands then rather than the last frame water was on screen.
    reflection.set_if_neq(reflected_camera_transform(main, surface.translation().y));
    // The reflection image is sampled with the main view's screen coordinates, so it has to be
    // rendered with the main camera's lens, and with everything below the water out of it.
    if let (Some(Projection::Perspective(main_lens)), Some(mut reflection_projection)) =
        (projection, reflection_projection)
    {
        *reflection_projection = Projection::Perspective(reflection_lens(
            main_lens,
            &reflection,
            surface.translation().y,
        ));
    }
    // The Graphics setting switches the reflection off without touching the mirror plane above.
    let active = surface_in_view && settings.is_none_or(|settings| settings.water_reflections);
    if camera.is_active != active {
        camera.is_active = active;
    }
    profiler.record_elapsed("render/water_reflection_camera", started);
}

/// Whether a water plane's bounds reach the main camera's view, its bounds grown by
/// [`WATER_REFLECTION_FRUSTUM_MARGIN`] first.
///
/// The frustum half is Bevy's own test against the plane's [`Aabb`]. Bevy's perspective projection
/// is infinite reverse-z, so the [`Frustum`] carries no far plane at all (`from_clip_from_world`
/// leaves its last half space at `(NaN, NaN, NaN, inf)`), and the far distance of the camera's
/// [`Projection`] is applied here by hand: the plane's nearest reach along the camera's forward
/// axis, from its bounds projected onto that axis.
///
/// Missing pieces mean the test cannot run - a plane whose mesh has not produced an [`Aabb`] yet,
/// or a main camera without a [`Frustum`] - and the plane then counts as visible, keeping the
/// reflection camera on rather than risk a stale reflection.
fn water_plane_in_view(
    main: &GlobalTransform,
    frustum: Option<&Frustum>,
    far: Option<f32>,
    surface: &GlobalTransform,
    bounds: Option<&Aabb>,
) -> bool {
    let (Some(frustum), Some(far), Some(bounds)) = (frustum, far, bounds) else {
        return true;
    };
    let margin = Vec3::splat(WATER_REFLECTION_FRUSTUM_MARGIN);
    let grown = Aabb::from_min_max(
        Vec3::from(bounds.min()) - margin,
        Vec3::from(bounds.max()) + margin,
    );
    let surface_to_world = surface.affine();
    let centre = Vec3::from(surface_to_world.transform_point3a(grown.center));
    let radius = grown.relative_radius(&main.forward().as_vec3().into(), &surface_to_world.matrix3);
    if (centre - main.translation()).dot(*main.forward()) - radius > far {
        return false;
    }
    frustum.intersects_obb(&grown, &surface_to_world, true, true)
}

/// The reflection camera's lens: the main camera's, clipped at the water plane.
///
/// The reflection camera sits below the surface looking up, so the river bed and the banks lie
/// between it and the sky and would fill the image (the white sheet). Clipping at the water plane
/// keeps only what is above it, as a real mirror shows. Bevy's own mirror example does the same with
/// [`PerspectiveProjection::near_clip_plane`]: the plane's normal in the camera's view space, and
/// the negative distance to it as the fourth component.
fn reflection_lens(
    main_lens: &PerspectiveProjection,
    reflection: &Transform,
    water_y: f32,
) -> PerspectiveProjection {
    // Keep the side the main camera looks at the water from: above it, unless the main camera is
    // underwater (its mirrored camera then sits above the surface).
    let height_above_camera = water_y - reflection.translation.y;
    let normal_world = if height_above_camera >= 0.0 {
        Vec3::Y
    } else {
        Vec3::NEG_Y
    };
    let normal_view = reflection.rotation.inverse() * normal_world;
    PerspectiveProjection {
        near_clip_plane: normal_view.extend(-height_above_camera.abs()),
        ..*main_lens
    }
}

fn reflected_camera_transform(main: &GlobalTransform, water_y: f32) -> Transform {
    let mut position = main.translation();
    position.y = water_y * 2.0 - position.y;
    let mut forward = main.forward().as_vec3();
    forward.y = -forward.y;
    Transform::from_translation(position).looking_to(forward, Vec3::Y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_reservation_observes_all_six_diffuse_and_normal_layers() {
        let mut images = Assets::<Image>::default();
        let handles: [Handle<Image>; 12] = std::array::from_fn(|_| images.add(Image::default()));
        let extension = TerrainExtension {
            layer_0: Some(handles[0].clone()),
            layer_1: Some(handles[1].clone()),
            layer_2: Some(handles[2].clone()),
            layer_3: Some(handles[3].clone()),
            layer_4: Some(handles[4].clone()),
            layer_5: Some(handles[5].clone()),
            normal_0: Some(handles[6].clone()),
            normal_1: Some(handles[7].clone()),
            normal_2: Some(handles[8].clone()),
            normal_3: Some(handles[9].clone()),
            normal_4: Some(handles[10].clone()),
            normal_5: Some(handles[11].clone()),
            ..default()
        };
        assert_eq!(
            extension.image_ids().collect::<Vec<_>>(),
            handles.iter().map(Handle::id).collect::<Vec<_>>()
        );
    }
    use crate::world::components::{StreamingCamera, WaterSurface};

    const MAIN_CAMERA_FAR: f32 = 1000.0;
    /// The mesh bounds of the water plane streaming.rs spawns: a `Plane3d` quad a cell wide with no
    /// thickness at all.
    const CELL_WATER_HALF_EXTENTS: Vec3 = Vec3::new(2048.0, 0.0, 2048.0);

    /// The main camera and the reflection camera the gate drives, with no plugins. The gate reads
    /// the camera's pose, frustum and far distance and writes `Camera::is_active` on the reflection
    /// camera, so a headless `App` is enough to run its frames.
    struct ReflectionHarness {
        app: App,
        main_camera: Entity,
        reflection_camera: Entity,
    }

    impl ReflectionHarness {
        fn new(main: Transform) -> Self {
            let mut app = App::new();
            app.init_resource::<ProfilingState>()
                .add_systems(Update, update_water_reflection_camera);
            let main_camera = app
                .world_mut()
                .spawn((
                    GlobalTransform::from(main),
                    main_camera_frustum(&main),
                    main_camera_projection(),
                    StreamingCamera,
                ))
                .id();
            let reflection_camera = app
                .world_mut()
                .spawn((
                    Camera {
                        order: -1,
                        is_active: false,
                        ..default()
                    },
                    Transform::default(),
                    Projection::Perspective(PerspectiveProjection::default()),
                    SceneColorPipeline::reflection(),
                    WaterReflectionCamera,
                ))
                .id();
            Self {
                app,
                main_camera,
                reflection_camera,
            }
        }

        /// Turns the main camera and the frustum Bevy keeps in step with it.
        fn aim(&mut self, main: Transform) {
            self.app
                .world_mut()
                .entity_mut(self.main_camera)
                .insert((GlobalTransform::from(main), main_camera_frustum(&main)));
        }

        fn spawn_water(&mut self, translation: Vec3, half_extents: Vec3) {
            self.app.world_mut().spawn((
                GlobalTransform::from(Transform::from_translation(translation)),
                water_bounds(half_extents),
                WaterSurface,
            ));
        }

        fn frame(&mut self) -> ReflectionFrame {
            self.app.update();
            let world = self.app.world();
            let camera = world.get::<Camera>(self.reflection_camera).unwrap();
            let transform = world.get::<Transform>(self.reflection_camera).unwrap();
            ReflectionFrame {
                active: camera.is_active,
                transform: *transform,
            }
        }
    }

    struct ReflectionFrame {
        active: bool,
        transform: Transform,
    }

    fn main_camera_projection() -> Projection {
        Projection::Perspective(PerspectiveProjection {
            far: MAIN_CAMERA_FAR,
            ..default()
        })
    }

    /// The `Frustum` `update_frusta` derives from a camera's projection and pose.
    fn main_camera_frustum(main: &Transform) -> Frustum {
        let projection = main_camera_projection();
        let global = GlobalTransform::from(*main);
        let clip_from_world = projection.get_clip_from_view() * global.to_matrix().inverse();
        Frustum(ViewFrustum::from_clip_from_world(&clip_from_world))
    }

    /// A water plane's bounds in its own space, as `calculate_bounds` leaves them on the entity.
    fn water_bounds(half_extents: Vec3) -> Aabb {
        Aabb::from_min_max(-half_extents, half_extents)
    }

    #[test]
    fn a_spawned_camera_carries_the_components_the_gate_reads() {
        let mut app = App::new();
        let camera = app
            .world_mut()
            .spawn((Camera3d::default(), StreamingCamera))
            .id();
        let world = app.world();
        assert!(world.get::<Frustum>(camera).is_some());
        assert!(world.get::<Projection>(camera).is_some());
    }

    #[test]
    fn reflection_target_preserves_linear_hdr_until_the_main_view() {
        use bevy::camera::Hdr;
        use bevy::core_pipeline::tonemapping::Tonemapping;
        use bevy::render::render_resource::TextureFormat;

        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .add_systems(Startup, setup_water_reflection);
        app.update();
        let target = &app.world().resource::<WaterReflectionTexture>().0;
        let image = app.world().resource::<Assets<Image>>().get(target).unwrap();
        assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba16Float);
        assert!(image.texture_view_descriptor.is_none());
        let mut cameras = app
            .world_mut()
            .query_filtered::<(&Hdr, &Tonemapping, &Exposure), With<WaterReflectionCamera>>();
        let (_, tonemapping, exposure) = cameras.single(app.world()).unwrap();
        assert_eq!(*tonemapping, Tonemapping::None);
        assert_eq!(exposure.ev100, 9.7);
    }

    #[test]
    fn reflection_tracks_exposure_changes_even_while_not_visible() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        for ev100 in [7.0, 12.0, 9.7] {
            harness
                .app
                .world_mut()
                .entity_mut(harness.main_camera)
                .insert(Exposure { ev100 });
            assert!(!harness.frame().active);
            assert_eq!(
                harness
                    .app
                    .world()
                    .get::<Exposure>(harness.reflection_camera)
                    .unwrap()
                    .ev100,
                ev100
            );
        }
        harness
            .app
            .world_mut()
            .entity_mut(harness.main_camera)
            .remove::<Exposure>();
        harness.frame();
        assert_eq!(
            harness
                .app
                .world()
                .get::<Exposure>(harness.reflection_camera)
                .unwrap()
                .ev100,
            9.7
        );
    }

    #[test]
    fn stationary_reflection_does_not_mark_camera_state_changed() {
        #[derive(Resource, Default)]
        struct Changes {
            transform: bool,
            camera: bool,
            exposure: bool,
        }

        type ReflectionChanges<'w> = (Ref<'w, Transform>, Ref<'w, Camera>, Ref<'w, Exposure>);

        fn observe_changes(
            reflection: Query<ReflectionChanges<'_>, With<WaterReflectionCamera>>,
            mut changes: ResMut<Changes>,
        ) {
            let (transform, camera, exposure) = reflection.single().unwrap();
            *changes = Changes {
                transform: transform.is_changed(),
                camera: camera.is_changed(),
                exposure: exposure.is_changed(),
            };
        }

        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness
            .app
            .init_resource::<Changes>()
            .add_systems(Last, observe_changes);
        harness.spawn_water(Vec3::new(0.0, 40.0, -800.0), Vec3::new(200.0, 0.0, 200.0));
        assert!(harness.frame().active);
        assert!(harness.frame().active);
        let changes = harness.app.world().resource::<Changes>();
        assert!(!changes.transform && !changes.camera && !changes.exposure);

        harness.aim(Transform::from_xyz(0.0, 160.0, 0.0));
        let frame = harness.frame();
        assert_eq!(frame.transform.translation.y, -80.0);
        let changes = harness.app.world().resource::<Changes>();
        assert!(changes.transform && !changes.camera && !changes.exposure);

        harness
            .app
            .world_mut()
            .entity_mut(harness.main_camera)
            .insert(Exposure { ev100: 7.0 });
        harness.frame();
        let changes = harness.app.world().resource::<Changes>();
        assert!(!changes.transform && !changes.camera && changes.exposure);

        harness.aim(Transform::from_xyz(0.0, 160.0, 0.0).looking_to(Vec3::Z, Vec3::Y));
        assert!(!harness.frame().active);
        assert!(harness.app.world().resource::<Changes>().camera);
        assert!(!harness.frame().active);
        let changes = harness.app.world().resource::<Changes>();
        assert!(!changes.transform && !changes.camera && !changes.exposure);
    }

    #[test]
    fn v2_reflection_restores_missing_exposure_and_updates_its_pose() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, -800.0), CELL_WATER_HALF_EXTENTS);
        for exposure in [Some(Exposure { ev100: 7.0 }), None] {
            harness
                .app
                .world_mut()
                .entity_mut(harness.reflection_camera)
                .remove::<Exposure>();
            if let Some(exposure) = exposure {
                harness
                    .app
                    .world_mut()
                    .entity_mut(harness.main_camera)
                    .insert(exposure);
            } else {
                harness
                    .app
                    .world_mut()
                    .entity_mut(harness.main_camera)
                    .remove::<Exposure>();
            }
            let frame = harness.frame();
            assert!(frame.active);
            assert_eq!(frame.transform.translation.y, -40.0);
            assert_eq!(
                harness
                    .app
                    .world()
                    .get::<Exposure>(harness.reflection_camera)
                    .unwrap()
                    .ev100,
                exposure.map_or(9.7, |value| value.ev100)
            );
        }
    }

    #[test]
    fn reflection_camera_renders_while_a_water_plane_is_in_view() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, -800.0), CELL_WATER_HALF_EXTENTS);
        assert!(harness.frame().active);
    }

    #[test]
    fn reflection_camera_skips_water_behind_the_main_camera() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, 5_000.0), Vec3::new(200.0, 0.0, 200.0));
        assert!(!harness.frame().active);
    }

    #[test]
    fn reflection_camera_skips_water_past_the_far_plane() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, -4_000.0), Vec3::new(200.0, 0.0, 200.0));
        assert!(!harness.frame().active);
    }

    #[test]
    fn reflection_camera_skips_a_cell_sized_plane_past_the_far_plane() {
        // The plane's bounding sphere reaches inside the far distance, but no part of the plane
        // does: its nearest edge, grown by the margin, is still 1,940 units ahead.
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, -4_500.0), CELL_WATER_HALF_EXTENTS);
        assert!(!harness.frame().active);
    }

    #[test]
    fn reflection_camera_renders_when_only_a_plane_edge_reaches_the_view() {
        // A cell-sized plane a full cell to the side: its centre is far outside the view, but the
        // near corner of the plane crosses into the frustum.
        let main = Transform::from_xyz(0.0, 0.0, 0.0);
        let mut harness = ReflectionHarness::new(main);
        let centre = Vec3::new(2048.0, 0.0, -700.0);
        harness.spawn_water(centre, CELL_WATER_HALF_EXTENTS);
        assert!(harness.frame().active);

        // Counted as a point at its centre, the same plane stays outside the frustum even with the
        // gate's margin, so only the plane's bounds can carry the test.
        assert!(!water_plane_in_view(
            &GlobalTransform::from(main),
            Some(&main_camera_frustum(&main)),
            Some(MAIN_CAMERA_FAR),
            &GlobalTransform::from_translation(centre),
            Some(&water_bounds(Vec3::ZERO)),
        ));
    }

    #[test]
    fn reflection_camera_stays_off_without_water() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        assert!(!harness.frame().active);
    }

    #[test]
    fn reflection_camera_is_mirrored_in_the_frame_it_becomes_active() {
        let facing_away = Transform::from_xyz(0.0, 300.0, 0.0).looking_to(Vec3::Z, Vec3::Y);
        let mut harness = ReflectionHarness::new(facing_away);
        harness.spawn_water(Vec3::new(0.0, 40.0, -800.0), Vec3::new(200.0, 0.0, 200.0));
        assert!(!harness.frame().active);

        // Facing the water reopens the gate, and the mirror of that frame reflects the camera of
        // that frame, not the pose the gate closed at.
        harness.aim(Transform::from_xyz(0.0, 120.0, 0.0));
        let frame = harness.frame();
        assert!(frame.active);
        assert!((frame.transform.translation.y + 40.0).abs() < 1.0e-4);
        assert!(frame.transform.forward().z < 0.0);
    }

    #[test]
    fn reflection_camera_renders_until_a_plane_reports_its_bounds() {
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.app.world_mut().spawn((
            GlobalTransform::from(Transform::from_translation(Vec3::new(0.0, 40.0, -800.0))),
            WaterSurface,
        ));
        assert!(harness.frame().active);
    }

    #[test]
    fn terrain_sampler_repeats_with_linear_mips_and_16x_anisotropy() {
        let sampler = terrain_layer_sampler();
        assert_eq!(sampler.address_mode_u, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_v, ImageAddressMode::Repeat);
        assert_eq!(sampler.mag_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.min_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.mipmap_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.anisotropy_clamp, 16);
    }

    use crate::world::cache::TerrainLayerSnapshot;
    use bevy::app::Propagate;
    use bevy::asset::AssetApp;
    use bevy::{
        image::ImageFilterMode,
        mesh::VertexAttributeValues,
        reflect::{PartialReflect, ReflectRef},
    };

    /// A cell whose four quadrants each carry a base layer plus the given overlays, in the order the
    /// `VTXT` lists are given, so a weight field built from it can be read back against the lists
    /// that produced it.
    fn terrain_fixture_with_overlays(
        cell_id: u32,
        overlays: &[Vec<(u16, f32)>],
    ) -> TerrainSnapshot {
        let mut layers = Vec::new();
        for quadrant in 0..4u8 {
            layers.push(TerrainLayerSnapshot {
                texture_form_id: 1,
                quadrant,
                layer: 0,
                is_base: true,
                weights: Vec::new(),
            });
            for (slot, weights) in overlays.iter().enumerate() {
                let layer = u16::try_from(slot + 1).expect("fixture overlay count fits a u16");
                layers.push(TerrainLayerSnapshot {
                    texture_form_id: u32::from(layer) + 6,
                    quadrant,
                    layer,
                    is_base: false,
                    weights: weights.clone(),
                });
            }
        }
        TerrainSnapshot {
            cell_id,
            width: 33,
            height: 33,
            heights: vec![0.0; 33 * 33],
            normals: [0, 0, 127].repeat(33 * 33),
            vertex_colors: vec![255; 33 * 33 * 3],
            layers,
            water_height: None,
            water_type_form_id: None,
        }
    }

    /// The samples of one overlay's grid, in `VTXT` vertex order.
    fn overlay_grid(sample: impl Fn(usize, usize) -> f32) -> Vec<(u16, f32)> {
        let mut grid = Vec::with_capacity(QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES);
        for y in 0..QUADRANT_WEIGHT_SAMPLES {
            for x in 0..QUADRANT_WEIGHT_SAMPLES {
                let index = y * QUADRANT_WEIGHT_SAMPLES + x;
                grid.push((
                    u16::try_from(index).expect("a sample index fits a u16"),
                    sample(x, y),
                ));
            }
        }
        grid
    }

    /// The weight a fragment receives from `grid_point` and `grid_weight` in `terrain.wgsl`: the
    /// quadrant-local coordinate clamped onto the `17x17` sample grid, then read bilinearly, clamped
    /// at the far edge. A model of the shader, not of the field: which sample of which word a
    /// coordinate reads is pinned against the shader's own expressions by
    /// `shader_weight_index_expressions_address_the_packed_field`.
    fn shader_weight(settings: &TerrainSettings, overlay: usize, coordinate: [f32; 2]) -> f32 {
        let last = (QUADRANT_WEIGHT_SAMPLES - 1) as f32;
        let sample = |x: usize, y: usize| -> f32 {
            let index = y * QUADRANT_WEIGHT_SAMPLES + x;
            settings.weights[overlay * OVERLAY_WEIGHT_WORDS + index / 4][index % 4]
        };
        // `grid_point`: clamping before the fraction is taken keeps the edge sample of a coordinate
        // an ulp outside the quadrant from blending its neighbour in.
        let column = coordinate[0].clamp(0.0, last);
        let row = coordinate[1].clamp(0.0, last);
        let blend = [column - column.floor(), row - row.floor()];
        let axis = |value: f32| -> (usize, usize) {
            let base = value.floor() as usize;
            (base, (base + 1).min(last as usize))
        };
        let (west, east) = axis(column);
        let (north, south) = axis(row);
        let top = sample(west, north) * (1.0 - blend[0]) + sample(east, north) * blend[0];
        let bottom = sample(west, south) * (1.0 - blend[0]) + sample(east, south) * blend[0];
        top * (1.0 - blend[1]) + bottom * blend[1]
    }

    /// The value of a `const NAME: u32 = <n>u;` declaration in `terrain.wgsl`.
    fn shader_constant(source: &str, name: &str) -> usize {
        let declaration = format!("const {name}: u32 = ");
        let rest = source
            .split_once(&declaration)
            .unwrap_or_else(|| panic!("terrain.wgsl declares no `{declaration}`"))
            .1;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("`{declaration}` is not followed by a number: {rest}"))
    }

    /// The element count of a `field: array<vec4<f32>, <n>>` declaration in `terrain.wgsl`.
    fn shader_array_length(source: &str, field: &str) -> usize {
        let declaration = format!("{field}: array<vec4<f32>, ");
        let rest = source
            .split_once(&declaration)
            .unwrap_or_else(|| panic!("terrain.wgsl declares no `{declaration}`"))
            .1;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("`{declaration}` is not followed by a number: {rest}"))
    }

    /// The field names of `struct NAME { .. }` in `terrain.wgsl`, in declaration order, so the
    /// uniform's layout can be compared with the Rust struct that is written into it.
    fn shader_struct_fields(source: &str, name: &str) -> Vec<String> {
        let body = source
            .split_once(&format!("struct {name} {{"))
            .unwrap_or_else(|| panic!("terrain.wgsl declares no `struct {name}`"))
            .1
            .split_once('}')
            .expect("the struct declaration must be closed")
            .0;
        body.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(|line| {
                line.split_once(':')
                    .unwrap_or_else(|| panic!("`{line}` is not a field declaration"))
                    .0
                    .to_owned()
            })
            .collect()
    }

    /// The expression a `const NAME: u32 = <expression>;` declaration in `terrain.wgsl` holds, so a
    /// constant derived from another is read as the shader writes it.
    fn shader_constant_expression<'a>(source: &'a str, name: &str) -> &'a str {
        let declaration = format!("const {name}: u32 = ");
        source
            .split_once(&declaration)
            .unwrap_or_else(|| panic!("terrain.wgsl declares no `{declaration}`"))
            .1
            .split_once(';')
            .expect("a const declaration must end with a semicolon")
            .0
            .trim()
    }

    /// The body of `fn NAME(..) { <body> }` in `terrain.wgsl`, for the helpers whose whole body is
    /// the arithmetic under test.
    fn shader_function_body<'a>(source: &'a str, name: &str) -> &'a str {
        source
            .split_once(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("terrain.wgsl declares no `fn {name}`"))
            .1
            .split_once('{')
            .expect("a function declaration must open its body")
            .1
            .split_once('}')
            .expect("a function body must be closed")
            .0
    }

    /// `source` without its whitespace, so a pinned expression survives a reformat of the file.
    fn without_whitespace(source: &str) -> String {
        source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    /// The value of one of `terrain.wgsl`'s `u32` expressions over the named `terms`: `+`, `-`, `*`,
    /// `/`, `%`, parentheses, literals with their `u` suffix, and identifiers. This runs the
    /// shader's own arithmetic rather than a copy of it, which is the only way a test can notice a
    /// transposed index - a copy of the expression transposes with it.
    fn evaluate_u32(expression: &str, terms: &[(&str, u32)]) -> u32 {
        /// The expression left to read, and what the shader's identifiers stand for.
        struct Parser<'a> {
            characters: Vec<char>,
            position: usize,
            terms: &'a [(&'a str, u32)],
        }

        impl<'a> Parser<'a> {
            fn skip_whitespace(&mut self) {
                while self
                    .characters
                    .get(self.position)
                    .is_some_and(|character| character.is_whitespace())
                {
                    self.position += 1;
                }
            }

            /// Reads `expected` if it is next, whitespace aside.
            fn eat(&mut self, expected: char) -> bool {
                self.skip_whitespace();
                if self.characters.get(self.position) == Some(&expected) {
                    self.position += 1;
                    return true;
                }
                false
            }

            /// `<operand> (('*' | '/' | '%') <operand>)*`, left to right as WGSL binds it.
            fn product(&mut self) -> u32 {
                let mut value = self.operand();
                loop {
                    if self.eat('*') {
                        value *= self.operand();
                    } else if self.eat('/') {
                        value /= self.operand();
                    } else if self.eat('%') {
                        value %= self.operand();
                    } else {
                        return value;
                    }
                }
            }

            /// `<product> (('+' | '-') <product>)*`.
            fn sum(&mut self) -> u32 {
                let mut value = self.product();
                loop {
                    if self.eat('+') {
                        value += self.product();
                    } else if self.eat('-') {
                        value -= self.product();
                    } else {
                        return value;
                    }
                }
            }

            /// The whole expression, for the messages of the assertions below.
            fn text(&self) -> String {
                self.characters.iter().collect()
            }

            /// A literal, one of the named terms, or a parenthesised sum.
            fn operand(&mut self) -> u32 {
                self.skip_whitespace();
                if self.eat('(') {
                    let value = self.sum();
                    assert!(
                        self.eat(')'),
                        "`{}` must close its parentheses",
                        self.text()
                    );
                    return value;
                }
                let mut name = String::new();
                while let Some(&character) = self.characters.get(self.position) {
                    if !character.is_ascii_alphanumeric() && character != '_' {
                        break;
                    }
                    name.push(character);
                    self.position += 1;
                }
                // A literal may carry WGSL's `u` suffix, which says nothing about its value.
                if let Ok(literal) = name.trim_end_matches('u').parse::<u32>() {
                    return literal;
                }
                let (_, value) = self
                    .terms
                    .iter()
                    .find(|(term, _)| *term == name.as_str())
                    .unwrap_or_else(|| {
                        panic!(
                            "`{}` names `{name}`, which the test does not define",
                            self.text()
                        )
                    });
                *value
            }
        }

        let mut parser = Parser {
            characters: expression.chars().collect(),
            position: 0,
            terms,
        };
        let value = parser.sum();
        parser.skip_whitespace();
        assert_eq!(
            parser.position,
            parser.characters.len(),
            "`{expression}` has a suffix the test cannot evaluate"
        );
        value
    }

    #[test]
    fn reflects_camera_above_and_below_the_water_plane() {
        let above = GlobalTransform::from(
            Transform::from_xyz(2.0, 10.0, 4.0).looking_to(Vec3::new(0.0, -0.5, -1.0), Vec3::Y),
        );
        let reflected = reflected_camera_transform(&above, 3.0);
        assert!((reflected.translation.y + 4.0).abs() < 1.0e-5);
        assert!(reflected.forward().y > 0.0);

        let below = GlobalTransform::from(
            Transform::from_xyz(2.0, -4.0, 4.0).looking_to(Vec3::new(0.0, 0.5, -1.0), Vec3::Y),
        );
        let reflected = reflected_camera_transform(&below, 3.0);
        assert!((reflected.translation.y - 10.0).abs() < 1.0e-5);
        assert!(reflected.forward().y < 0.0);
    }

    /// Whether a world point is inside the reflection camera's clipped view (reverse-z, so a point
    /// in front of the near or oblique clip plane has `z > w`).
    fn in_reflection_view(lens: &PerspectiveProjection, camera: &Transform, point: Vec3) -> bool {
        use bevy::camera::CameraProjection;
        let view = camera.compute_affine().inverse().transform_point3(point);
        let clip = lens.get_clip_from_view() * view.extend(1.0);
        clip.w > 0.0 && clip.z >= 0.0 && clip.z <= clip.w
    }

    #[test]
    fn the_reflection_lens_keeps_what_is_above_the_water_and_drops_what_is_below() {
        let main = GlobalTransform::from(
            Transform::from_xyz(0.0, 10.0, 0.0).looking_to(Vec3::NEG_Z, Vec3::Y),
        );
        let water_y = 3.0;
        let reflection = reflected_camera_transform(&main, water_y);
        let lens = reflection_lens(&PerspectiveProjection::default(), &reflection, water_y);
        // The mirrored camera sits at y = -4 looking level; the points are straight ahead of it.
        assert!(in_reflection_view(
            &lens,
            &reflection,
            Vec3::new(0.0, 5.0, -20.0)
        ));
        assert!(!in_reflection_view(
            &lens,
            &reflection,
            Vec3::new(0.0, 1.0, -20.0)
        ));
        assert!(!in_reflection_view(
            &lens,
            &reflection,
            Vec3::new(0.0, -2.0, -20.0)
        ));
    }

    #[test]
    fn the_reflection_lens_for_a_camera_below_the_water_keeps_what_is_below_it() {
        let main = GlobalTransform::from(
            Transform::from_xyz(0.0, -4.0, 0.0).looking_to(Vec3::NEG_Z, Vec3::Y),
        );
        let water_y = 3.0;
        let reflection = reflected_camera_transform(&main, water_y);
        let lens = reflection_lens(&PerspectiveProjection::default(), &reflection, water_y);
        // The mirrored camera sits at y = 10 looking level: the world under the surface is kept
        // and the air above it is dropped.
        assert!(in_reflection_view(
            &lens,
            &reflection,
            Vec3::new(0.0, -2.0, -20.0)
        ));
        assert!(!in_reflection_view(
            &lens,
            &reflection,
            Vec3::new(0.0, 5.0, -20.0)
        ));
    }

    #[test]
    fn the_reflection_system_gives_the_reflection_camera_the_clipped_main_lens() {
        use bevy::camera::CameraProjection;
        let mut harness = ReflectionHarness::new(Transform::from_xyz(0.0, 120.0, 0.0));
        harness.spawn_water(Vec3::new(0.0, 40.0, -800.0), CELL_WATER_HALF_EXTENTS);
        assert!(harness.frame().active);
        let Projection::Perspective(lens) = harness
            .app
            .world()
            .get::<Projection>(harness.reflection_camera)
            .unwrap()
        else {
            panic!("the reflection camera lost its perspective projection");
        };
        let Projection::Perspective(main_lens) = main_camera_projection() else {
            unreachable!();
        };
        assert_eq!(lens.far, main_lens.far);
        assert!(lens.near_clip_plane.xyz().length() > 0.99);
        assert!(lens.near_clip_plane.w < 0.0);
        let _ = lens.get_clip_from_view();
    }

    #[test]
    fn the_reflection_lens_keeps_the_main_cameras_field_of_view() {
        let main_lens = PerspectiveProjection {
            fov: 1.3,
            aspect_ratio: 4.0 / 3.0,
            near: 0.5,
            far: 5000.0,
            ..default()
        };
        let reflection = Transform::from_xyz(0.0, -4.0, 0.0);
        let lens = reflection_lens(&main_lens, &reflection, 3.0);
        assert_eq!(lens.fov, main_lens.fov);
        assert_eq!(lens.aspect_ratio, main_lens.aspect_ratio);
        assert_eq!(lens.far, main_lens.far);
    }

    /// The reflection pass is what costs 1.72 ms of a 4.41 ms frame at the rural cell, because it
    /// redraws every placed object; the camera must be spawned on the world layer alone. Asserting
    /// on the spawned camera rather than on the constant keeps the two from drifting apart.
    #[test]
    fn the_reflection_camera_renders_the_world_layer_only() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .add_systems(Startup, setup_water_reflection);
        app.update();

        let mut cameras = app
            .world_mut()
            .query_filtered::<&RenderLayers, With<WaterReflectionCamera>>();
        let layers = cameras
            .single(app.world())
            .expect("the reflection camera must be spawned");
        assert!(layers.intersects(&RenderLayers::layer(WORLD_LAYER)));
        assert!(!layers.intersects(&RenderLayers::layer(WATER_LAYER)));
        assert!(!layers.intersects(&PLACED_OBJECT_RENDER_LAYERS));
    }

    /// The other half of the contract: the main camera is what still draws the placed objects and
    /// the water, so a new layer has to be added to its list and not only taken out of the
    /// reflection camera's.
    #[test]
    fn the_main_view_renders_every_layer() {
        let main_view = RenderLayers::from_layers(MAIN_VIEW_LAYERS);
        assert!(main_view.intersects(&RenderLayers::layer(WORLD_LAYER)));
        assert!(main_view.intersects(&RenderLayers::layer(WATER_LAYER)));
        assert!(main_view.intersects(&PLACED_OBJECT_RENDER_LAYERS));
        assert!(
            !RenderLayers::from_layers(REFLECTION_VIEW_LAYERS)
                .intersects(&PLACED_OBJECT_RENDER_LAYERS),
            "a placed object must never be drawn into the reflection texture"
        );
    }

    /// A light needs the world layer for both cameras and the placed-object layer for its shadow
    /// cascades: `check_dir_light_mesh_visibility` only collects casters that share a layer with
    /// the light.
    #[test]
    fn lights_cover_both_cameras_and_the_placed_objects() {
        let light = RenderLayers::from_layers(LIGHT_LAYERS);
        for view in [MAIN_VIEW_LAYERS, REFLECTION_VIEW_LAYERS] {
            assert!(
                light.intersects(&RenderLayers::from_layers(view)),
                "a light must reach {view:?}"
            );
        }
        assert!(light.intersects(&PLACED_OBJECT_RENDER_LAYERS));
    }

    /// A point light is a child of its placed-object reference, whose layer propagates down the
    /// hierarchy; the light must keep [`LIGHT_LAYERS`] anyway, or it stops lighting the terrain
    /// and the reflection view.
    #[test]
    fn a_light_below_a_reference_keeps_the_light_layers() {
        let mut app = App::new();
        add_placed_object_layer_propagation(&mut app);
        let reference = app
            .world_mut()
            .spawn(Propagate(PLACED_OBJECT_RENDER_LAYERS))
            .id();
        let light = app
            .world_mut()
            .spawn((light_render_layers(), ChildOf(reference)))
            .id();
        app.update();
        app.update();
        assert_eq!(
            app.world().entity(reference).get::<RenderLayers>(),
            Some(&PLACED_OBJECT_RENDER_LAYERS)
        );
        assert_eq!(
            app.world().entity(light).get::<RenderLayers>(),
            Some(&RenderLayers::from_layers(LIGHT_LAYERS)),
            "the propagated placed-object layer must not replace the light's layers"
        );
    }

    /// The propagation has to be in place before visibility compares layers, and the meshes it
    /// carries arrive from the glb spawner in `SpawnScene` - between `Update` and `PostUpdate`. A
    /// descendant spawned after its reference must still inherit the layer in that same frame.
    #[test]
    fn a_descendant_spawned_after_its_reference_inherits_the_placed_object_layer() {
        let mut app = App::new();
        add_placed_object_layer_propagation(&mut app);
        let reference = app
            .world_mut()
            .spawn(Propagate(PLACED_OBJECT_RENDER_LAYERS))
            .id();
        app.update();
        // The glb is spawned frames after the reference: a scene root with the mesh primitives
        // below it.
        let scene_root = app.world_mut().spawn(ChildOf(reference)).id();
        let mesh = app
            .world_mut()
            .spawn((Mesh3d(Handle::default()), ChildOf(scene_root)))
            .id();

        app.update();

        for entity in [reference, scene_root, mesh] {
            assert_eq!(
                app.world().entity(entity).get::<RenderLayers>(),
                Some(&PLACED_OBJECT_RENDER_LAYERS),
                "a reference and everything below it must carry the placed-object layer"
            );
        }
    }

    #[test]
    fn animated_water_advances_the_shader_phase() {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<Assets<WaterMaterial>>()
            .init_resource::<ProfilingState>()
            .add_systems(Update, animate_water_materials);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<WaterMaterial>>()
            .add(WaterMaterial {
                base: StandardMaterial::default(),
                extension: WaterExtension::default(),
            });
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(2));
        app.update();
        let material = app
            .world()
            .resource::<Assets<WaterMaterial>>()
            .get(&handle)
            .unwrap();
        assert_eq!(material.extension.settings.wave_scale_speed_strength.w, 2.0);
    }

    #[test]
    fn final_renderer_requires_every_gpu_path_signal() {
        let complete = RendererMetrics {
            gpu_preprocessing_active: true,
            gpu_culling_active: true,
            indirect_drawing_active: true,
            occlusion_culling_views: 1,
            hzb_views: 1,
            indirect_phase_buffers: 1,
            indirect_batch_sets: 1,
            proof_frames: 1,
            ..default()
        };
        assert!(complete.final_path_active());
        assert!(
            !RendererMetrics {
                hzb_views: 0,
                ..complete.clone()
            }
            .final_path_active()
        );
        assert!(
            !RendererMetrics {
                renderer_validation_failures: 1,
                ..complete
            }
            .final_path_active()
        );
    }

    /// The shader tiles every layer by the shared LAND repeats per cell, so the layer textures
    /// must be sampled with a repeating address mode. Bevy's default clamps to the edge, which
    /// stretched the textures in the frames: everything past the first tile read the edge texels.
    #[test]
    fn dropping_normal_maps_unbinds_them_and_clears_their_flags() {
        let mut extension = TerrainExtension::default();
        let normals: [Option<Handle<Image>>; 6] =
            std::array::from_fn(|index| (index % 2 == 0).then(Handle::default));
        extension.settings.set_normal_layers(&normals);
        [extension.normal_0, extension.normal_2, extension.normal_4] =
            [normals[0].clone(), normals[2].clone(), normals[4].clone()];
        assert!(extension.has_normal_maps());

        extension.drop_normal_maps();

        assert!(!extension.has_normal_maps());
        assert!(
            [
                &extension.normal_0,
                &extension.normal_1,
                &extension.normal_2,
                &extension.normal_3,
                &extension.normal_4,
                &extension.normal_5,
            ]
            .iter()
            .all(|normal| normal.is_none())
        );
    }

    #[test]
    fn tiled_terrain_layers_are_sampled_with_a_repeating_sampler() {
        let sampler = terrain_layer_sampler();
        assert_eq!(sampler.address_mode_u, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_v, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_w, ImageAddressMode::Repeat);
        // Repeat and 16x anisotropy retain linear filtering and the default mip range.
        let default_sampler = ImageSamplerDescriptor::linear();
        assert_eq!(sampler.mag_filter, default_sampler.mag_filter);
        assert_eq!(sampler.min_filter, default_sampler.min_filter);
        assert_eq!(sampler.mipmap_filter, default_sampler.mipmap_filter);
        assert_eq!(sampler.lod_min_clamp, default_sampler.lod_min_clamp);
        assert_eq!(sampler.lod_max_clamp, default_sampler.lod_max_clamp);
        assert_eq!(sampler.anisotropy_clamp, 16);
        assert_eq!(sampler.compare, default_sampler.compare);
        assert_eq!(sampler.mag_filter, ImageFilterMode::Linear);
        assert_eq!(
            default_sampler.address_mode_u,
            ImageAddressMode::ClampToEdge
        );
        assert_eq!(
            default_sampler.address_mode_v,
            ImageAddressMode::ClampToEdge
        );
        assert!(
            TerrainSettings::vertex_weights_only(1.0)
                .tiling_and_layer_count
                .x
                > 1.0,
            "layers only need a repeating sampler because the shader tiles them"
        );
    }

    /// Zero and unresolved nonzero overlays keep material slots and weights without requesting
    /// a diffuse image or an orphan normal map.
    #[test]
    fn terrain_zero_texture_overlay_material_preserves_slots_without_image_requests() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("placeholder-catalogue.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT,normal_path TEXT);
                 CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
                 CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);
                 INSERT INTO texture_sets VALUES(1,'textures/zero-must-not-load.dds',NULL);
                 INSERT INTO texture_sets VALUES(2,NULL,'textures/orphan-normal-must-not-load.dds');
                 INSERT INTO landscape_textures VALUES(0,1),(2116,2);",
            )
            .unwrap();
        drop(connection);
        let catalog = AssetCatalog::open(&path).unwrap();
        assert!(
            catalog.landscape_diffuse(0).is_some(),
            "poison null lookup is observable"
        );
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>();
        let asset_server = app.world().resource::<AssetServer>();
        let mut terrain =
            terrain_fixture_with_overlays(91, &[vec![(0, 0.25), (18, 0.75)], vec![(3, 0.625)]]);
        for layer in &mut terrain.layers {
            layer.texture_form_id = 0;
            if !layer.is_base && layer.layer == 2 {
                layer.layer = 4;
            }
        }
        let (extension, images) =
            TerrainExtension::from_quadrant(&terrain, 0, &catalog, asset_server)
                .expect("zero overlays keep material slots and do not request a texture");
        assert!(images.color.is_empty() && images.normal.is_empty());
        assert!(
            extension.layer_0.is_none()
                && extension.layer_1.is_none()
                && extension.layer_2.is_none()
        );
        assert!(
            extension.layer_3.is_none()
                && extension.layer_4.is_none()
                && extension.layer_5.is_none()
        );
        assert_eq!(extension.settings.tiling_and_layer_count.z, 3.0);
        assert_eq!(extension.settings.normal_layers_0, Vec4::ZERO);
        assert_eq!(extension.settings.normal_layers_1, Vec4::ZERO);
        let sample = |overlay: usize, vertex: usize| {
            extension.settings.weights[overlay * OVERLAY_WEIGHT_WORDS + vertex / 4][vertex % 4]
        };
        assert_eq!(sample(0, 0).to_bits(), 0.25f32.to_bits());
        assert_eq!(sample(0, 18).to_bits(), 0.75f32.to_bits());
        assert_eq!(sample(1, 3).to_bits(), 0.625f32.to_bits());
        assert_eq!(sample(1, 0), 0.0);

        terrain.layers[2].texture_form_id = 0x844;
        assert!(catalog.landscape_normal(0x844).is_some());
        let (fallback, images) =
            TerrainExtension::from_quadrant(&terrain, 0, &catalog, asset_server)
                .expect("a missing diffuse retains the layer without requesting either image");
        assert!(images.color.is_empty() && images.normal.is_empty());
        assert!(fallback.layer_2.is_none() && fallback.normal_2.is_none());
        assert_eq!(fallback.settings.weights, extension.settings.weights);
        assert_eq!(fallback.settings.tiling_and_layer_count.z, 3.0);
        assert_eq!(fallback.settings.normal_layers_0, Vec4::ZERO);
        assert_eq!(fallback.settings.normal_layers_1, Vec4::ZERO);
        terrain.layers[2].texture_form_id = 0;
        assert!(TerrainExtension::from_quadrant(&terrain, 0, &catalog, asset_server).is_ok());
    }

    /// The weight field is the quadrant's own `17x17` grids, one per overlay, in the order
    /// `quadrant_layers` returns them: an overlay's `VTXT` list reaches the fragment stage at the
    /// samples it names, and every sample it does not name reads as opacity 0.
    #[test]
    fn weight_field_carries_every_overlay_grid_from_the_layer_list() {
        let terrain = terrain_fixture_with_overlays(
            0x0001_2345,
            &[
                vec![(0, 1.0), (17 + 1, 0.5), (16 * 17 + 16, 0.25)],
                vec![(17 * 8 + 8, 0.75)],
            ],
        );
        let grids = crate::streaming::quadrant_overlay_weights(&terrain, 0).unwrap();
        assert_eq!(grids.len(), 2, "one grid per overlay, base excluded");
        assert_eq!(
            grids[0].len(),
            QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES,
            "a grid covers the quadrant's whole sample square"
        );
        let settings = TerrainSettings::for_quadrant(0, 3, &grids);
        assert_eq!(
            settings.tiling_and_layer_count,
            Vec4::new(24.0, 24.0, 3.0, 0.0)
        );
        assert_eq!(settings.quadrant_origin.xy(), Vec2::ZERO);
        assert_eq!(
            settings.weight_source.x, 1.0,
            "the material carries the weight field"
        );

        let sample = |overlay: usize, x: usize, y: usize| -> f32 {
            let index = y * QUADRANT_WEIGHT_SAMPLES + x;
            settings.weights[overlay * OVERLAY_WEIGHT_WORDS + index / 4][index % 4]
        };
        assert_eq!(sample(0, 0, 0), 1.0);
        assert_eq!(sample(0, 1, 1), 0.5);
        assert_eq!(sample(0, 16, 16), 0.25);
        assert_eq!(sample(0, 5, 5), 0.0, "an unnamed sample is opacity 0");
        assert_eq!(sample(1, 8, 8), 0.75, "the second overlay's own grid");
        assert_eq!(
            sample(1, 0, 0),
            0.0,
            "the two grids do not bleed into each other"
        );
        for overlay in 2..OVERLAY_WEIGHT_SLOTS {
            assert_eq!(
                sample(overlay, 8, 8),
                0.0,
                "an overlay the quadrant does not use leaves its slot empty"
            );
        }
        assert_eq!(
            settings.weights[OVERLAY_WEIGHT_WORDS - 1].w,
            0.0,
            "the padding word after the last sample stays zero"
        );
    }

    /// Each quadrant reads its own origin out of the cell, so the same cell uv addresses the same
    /// ground in every quadrant.
    #[test]
    fn quadrant_origin_places_each_quadrant_inside_the_cell() {
        let origins: Vec<[f32; 2]> = (0..4)
            .map(|quadrant| {
                let settings = TerrainSettings::for_quadrant(quadrant, 2, &[]);
                assert_eq!(
                    settings.quadrant_origin.z, 0.0,
                    "only xy carries the origin"
                );
                [settings.quadrant_origin.x, settings.quadrant_origin.y]
            })
            .collect();
        assert_eq!(
            origins,
            [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            "the mesh's quadrants in the same order: west/east then south/north"
        );
    }

    /// The weight field is only worth reading where there are overlays to read. A quadrant whose only
    /// layer is its base has none, and the packed vertex attributes carry none either, so the shader
    /// is told to skip the field's five lookups altogether.
    #[test]
    fn weight_source_is_set_only_for_quadrants_with_overlays() {
        let grid = vec![1.0; QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES];
        assert_eq!(
            TerrainSettings::for_quadrant(0, 2, &[grid]).weight_source,
            Vec4::X
        );
        assert_eq!(
            TerrainSettings::for_quadrant(0, 1, &[]).weight_source,
            Vec4::ZERO,
            "a base-only quadrant has no overlay to read"
        );
        assert_eq!(
            TerrainSettings::vertex_weights_only(6.0).weight_source,
            Vec4::ZERO,
            "a material with no weight field reads the packed attributes"
        );
    }

    /// A fixture material carries the quadrant's own weight field, so the no-game-data scene draws
    /// its overlays through the interpolation the streamed path uses instead of the packed tangent.
    #[test]
    fn fixture_materials_carry_the_quadrants_weight_field() {
        let sample = QUADRANT_WEIGHT_SAMPLES * 8 + 8;
        let terrain =
            terrain_fixture_with_overlays(0x0001_2345, &[vec![(0, 1.0), (sample as u16, 0.5)]]);
        for quadrant in 0..4u8 {
            let extension = TerrainExtension::fixture(
                &terrain,
                quadrant,
                std::array::from_fn(|_| Handle::<Image>::default()),
            )
            .expect("the fixture material must build");
            assert!(
                extension.reads_weight_field(),
                "quadrant {quadrant} must render its overlay through the weight field"
            );
            assert_eq!(
                extension.settings.quadrant_origin.xy(),
                Vec2::new(f32::from(quadrant % 2), f32::from(quadrant / 2)),
                "each quadrant reads its own origin"
            );
            assert_eq!(extension.settings.weights[0][0], 1.0);
            assert_eq!(
                extension.settings.weights[sample / 4][sample % 4],
                0.5,
                "the fixture's own VTXT opacity reaches the field"
            );
        }
    }

    /// The overlay weight field is the quadrant's own sample grid, interpolated bilinearly. A layer
    /// that is full strength on one sample and absent on the next must reach the fragment as the
    /// linear interpolation of the two - `0.5` halfway between them - not as the sharpened ramp the
    /// packed vertex attributes produce.
    #[test]
    fn weight_field_interpolates_linearly_between_samples() {
        let terrain = terrain_fixture_with_overlays(0x0001_2345, &[vec![(0, 1.0)]]);
        let grids = crate::streaming::quadrant_overlay_weights(&terrain, 0).unwrap();
        let settings = TerrainSettings::for_quadrant(0, 2, &grids);

        assert_eq!(shader_weight(&settings, 0, [0.0, 0.0]), 1.0);
        // Sample 0 is the quadrant's (x = 0, y = 0) corner and its x neighbour is empty, so one
        // sample step in x crosses an opacity of 1.0 down to 0.0.
        let halfway = shader_weight(&settings, 0, [0.5, 0.0]);
        assert!(
            (halfway - 0.5).abs() < 1.0e-5,
            "the midpoint of a 1.0 -> 0.0 edge must read 0.5, not the packed tangent's 0.25: {halfway}"
        );
        assert!((shader_weight(&settings, 0, [0.25, 0.0]) - 0.75).abs() < 1.0e-5);
        assert!((shader_weight(&settings, 0, [0.75, 0.0]) - 0.25).abs() < 1.0e-5);
        // Both axes: the field is bilinear, so the centre of the quad whose only full sample is the
        // corner reads 0.25 - what Skyrim's interpolation gives, and not what the triangle split
        // would if the weight stayed per vertex.
        assert!(
            (shader_weight(&settings, 0, [0.5, 0.5]) - 0.25).abs() < 1.0e-5,
            "the quad centre is the bilinear blend of its four samples"
        );
        // At the quadrant's far edge the field clamps to the last sample instead of wrapping.
        assert_eq!(shader_weight(&settings, 0, [16.0, 16.0]), 0.0);
        assert_eq!(shader_weight(&settings, 0, [16.0, 0.0]), 0.0);
        assert_eq!(shader_weight(&settings, 0, [0.0, 16.0]), 0.0);
        for overlay in 1..OVERLAY_WEIGHT_SLOTS {
            assert_eq!(shader_weight(&settings, overlay, [0.0, 0.0]), 0.0);
        }
    }

    #[test]
    fn landscape_uvs_repeat_twelve_times_per_quadrant_without_scaling_weights() {
        let terrain = terrain_fixture_with_overlays(0x12345, &[vec![(0, 1.0)]]);
        for quadrant in 0..4 {
            let mesh = crate::streaming::build_terrain_quadrant_mesh(&terrain, quadrant).unwrap();
            let VertexAttributeValues::Float32x2(uvs) =
                mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap()
            else {
                panic!("UVs missing")
            };
            let grids = crate::streaming::quadrant_overlay_weights(&terrain, quadrant).unwrap();
            let settings = TerrainSettings::for_quadrant(quadrant, 2, &grids);
            let tiling = settings.tiling_and_layer_count.xy();
            let origin = Vec2::from(uvs[0]);
            assert_eq!(
                (Vec2::from(uvs[16]) - origin) * tiling,
                Vec2::new(12.0, 0.0)
            );
            assert_eq!(
                (Vec2::from(uvs[272]) - origin) * tiling,
                Vec2::new(0.0, 12.0)
            );
            // Blend coordinates still address the original 17x17 LAND samples.
            let last = (Vec2::from(uvs[288]) * 2.0 - settings.quadrant_origin.xy()) * 16.0;
            assert_eq!(last, Vec2::splat(16.0));
        }
    }

    /// A coordinate outside the sample square reads the sample it is clamped onto. `grid_point`
    /// clamps before `grid_weight` takes its blend fraction, so a coordinate an ulp below the
    /// quadrant's west or south edge reads that edge sample rather than blending its neighbour in.
    #[test]
    fn weight_field_clamps_coordinates_onto_the_sample_square() {
        let last = (QUADRANT_WEIGHT_SAMPLES - 1) as f32;
        let edge = QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES - 1;
        let terrain =
            terrain_fixture_with_overlays(0x0001_2345, &[vec![(0, 1.0), (edge as u16, 0.25)]]);
        let grids = crate::streaming::quadrant_overlay_weights(&terrain, 0).unwrap();
        let settings = TerrainSettings::for_quadrant(0, 2, &grids);

        assert_eq!(shader_weight(&settings, 0, [-1.0e-7, 0.0]), 1.0);
        assert_eq!(shader_weight(&settings, 0, [0.0, -1.0e-7]), 1.0);
        assert_eq!(shader_weight(&settings, 0, [last + 1.0e-7, last]), 0.25);
        assert_eq!(shader_weight(&settings, 0, [last, last + 1.0e-7]), 0.25);
    }

    /// The mechanism the weight field replaces. `build_terrain_quadrant_mesh` still packs overlays
    /// into `ATTRIBUTE_TANGENT` for materials without a weight field, and Bevy re-normalizes the
    /// tangent's direction in the vertex shader (`mesh_tangent_local_to_world`), so the direction
    /// interpolates at full length while the magnitude it is multiplied by does not: halfway across
    /// the same 1.0 -> 0.0 edge the product reads 0.25 where the opacity is 0.5. Streamed quadrants
    /// read the weight field instead.
    #[test]
    fn packed_vertex_weights_sharpen_the_midpoint_to_a_quarter() {
        let terrain = terrain_fixture_with_overlays(0x0001_2345, &[vec![(0, 1.0)]]);
        let mesh = crate::streaming::build_terrain_quadrant_mesh(&terrain, 0).unwrap();
        let VertexAttributeValues::Float32x4(tangents) =
            mesh.attribute(Mesh::ATTRIBUTE_TANGENT).unwrap()
        else {
            panic!("terrain tangents must be Float32x4");
        };
        assert_eq!(
            tangents[0],
            [1.0, 0.0, 0.0, 1.0],
            "full weight, unit direction"
        );
        assert_eq!(tangents[1], [0.0; 4], "no weight at all");
        // The vertex shader normalizes xyz per vertex and the rasterizer interpolates the result;
        // the fragment shader multiplies the interpolated xyz by the interpolated magnitude.
        let midpoint: [f32; 4] =
            std::array::from_fn(|axis| (tangents[0][axis] + tangents[1][axis]) * 0.5);
        let reconstructed = midpoint[0] * midpoint[3].abs();
        assert!(
            (reconstructed - 0.25).abs() < 1.0e-5,
            "the packed path reads {reconstructed} where the interpolated opacity is 0.5"
        );
    }

    /// The mesh's cell UVs and the weight field agree sample for sample: `uv * 2 - quadrant_origin`
    /// scaled onto the grid lands exactly on the sample the shader must read for that vertex, with
    /// no scaling or orientation drift between the two.
    #[test]
    fn quadrant_uvs_address_the_weight_grid_sample_for_sample() {
        let samples = QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES;
        // A `VTXT` list that names every sample with its own index, so the weight read at a vertex
        // says which sample the shader looked up.
        let terrain = terrain_fixture_with_overlays(
            0x0001_2345,
            &[overlay_grid(|x, y| {
                (y * QUADRANT_WEIGHT_SAMPLES + x) as f32 / samples as f32
            })],
        );
        let last = (QUADRANT_WEIGHT_SAMPLES - 1) as f32;
        for quadrant in 0..4u8 {
            let mesh = crate::streaming::build_terrain_quadrant_mesh(&terrain, quadrant).unwrap();
            let grids = crate::streaming::quadrant_overlay_weights(&terrain, quadrant).unwrap();
            let settings = TerrainSettings::for_quadrant(quadrant, 2, &grids);
            let VertexAttributeValues::Float32x2(uvs) =
                mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap()
            else {
                panic!("terrain UVs must be Float32x2");
            };
            assert_eq!(uvs.len(), samples, "one vertex per grid sample");
            for (sample, uv) in uvs.iter().enumerate() {
                let coordinate = [
                    (uv[0] * 2.0 - settings.quadrant_origin.x) * last,
                    (uv[1] * 2.0 - settings.quadrant_origin.y) * last,
                ];
                let expected = sample as f32 / samples as f32;
                assert!(
                    (shader_weight(&settings, 0, coordinate) - expected).abs() < 1.0e-5,
                    "quadrant {quadrant} vertex {sample} reads {coordinate:?}, not its own sample"
                );
            }
        }
    }

    /// The uniform's Rust layout and the shader's must say the same numbers, and list the same
    /// fields in the same order - an insertion on one side alone shifts every field after it, and
    /// the shader then reads its neighbour's bytes.
    #[test]
    fn terrain_uniform_layout_matches_the_shader_source() {
        let source = include_str!("shaders/terrain.wgsl");
        assert_eq!(
            shader_constant(source, "WEIGHT_GRID_SIDE"),
            QUADRANT_WEIGHT_SAMPLES
        );
        assert_eq!(
            shader_constant(source, "WEIGHT_GRID_WORDS"),
            OVERLAY_WEIGHT_WORDS
        );
        assert_eq!(shader_array_length(source, "weights"), WEIGHT_FIELD_WORDS);
        assert_eq!(
            OVERLAY_WEIGHT_WORDS, 73,
            "289 samples four to a vec4, rounded up"
        );
        assert_eq!(WEIGHT_FIELD_WORDS, 365, "five overlays of 73 vec4s");

        let settings = TerrainSettings::for_quadrant(
            0,
            2,
            &[vec![1.0; QUADRANT_WEIGHT_SAMPLES * QUADRANT_WEIGHT_SAMPLES]],
        );
        let ReflectRef::Struct(fields) = settings.reflect_ref() else {
            panic!("TerrainSettings must reflect as a struct");
        };
        let rust_fields: Vec<String> = (0..fields.field_len())
            .map(|index| {
                fields
                    .name_at(index)
                    .expect("every field is named")
                    .to_owned()
            })
            .collect();
        assert_eq!(rust_fields, shader_struct_fields(source, "TerrainSettings"));
    }

    /// The shader's own index arithmetic, against the field the Rust side packs: which word and
    /// component a sample occupies, and which sample each corner of a grid point names. The
    /// expressions are read out of `terrain.wgsl` and evaluated, so a transposed index fails here -
    /// the model in `shader_weight` would transpose with it and keep passing.
    #[test]
    fn shader_weight_index_expressions_address_the_packed_field() {
        let source = include_str!("shaders/terrain.wgsl");
        let side = shader_constant(source, "WEIGHT_GRID_SIDE") as u32;
        let words = shader_constant(source, "WEIGHT_GRID_WORDS") as u32;
        // Every sample carries a value of its own, so an index that lands anywhere but on its own
        // sample reads a different opacity rather than a plausible one.
        let grid = |overlay: u32| -> Vec<f32> {
            (0..side * side)
                .map(|sample| (overlay * side * side + sample) as f32)
                .collect()
        };
        let grids: Vec<Vec<f32>> = (0..OVERLAY_WEIGHT_SLOTS as u32).map(grid).collect();
        let settings = TerrainSettings::for_quadrant(0, OVERLAY_WEIGHT_SLOTS + 1, &grids);

        // How `packed_weight` splits a sample between the word that holds it and the component of
        // that word.
        let packed = shader_function_body(source, "packed_weight");
        let field = packed
            .split_once("terrain.weights[")
            .expect("`packed_weight` must read the weight field")
            .1;
        let (word_of, rest) = field.split_once(']').expect("the word index must close");
        let (_, rest) = rest.split_once('[').expect("the component index must open");
        let component_of = rest.split_once(']').expect("the component must close").0;
        let (word_of, component_of) = (word_of.trim(), component_of.trim());

        // How `grid_weight` names the sample each corner of the coordinate reads, in the order the
        // bilinear blend takes them: west and east of the north row, then of the south row.
        let grid_weight = shader_function_body(source, "grid_weight");
        let corners: Vec<&str> = grid_weight
            .split("packed_weight(overlay, ")
            .skip(1)
            .map(|call| call.split(')').next().expect("a call must close"))
            .collect();
        assert_eq!(
            corners.len(),
            4,
            "the blend reads the four samples around the coordinate"
        );

        // The last row and column have no next sample - the shader clamps `next` there - so the
        // corners are evaluated only where all four neighbours are on the grid.
        for row in [0u32, 1, 7, 15] {
            for column in [0u32, 1, 7, 15] {
                let terms = [
                    ("west", column),
                    ("east", column + 1),
                    ("north", row),
                    ("south", row + 1),
                    ("WEIGHT_GRID_SIDE", side),
                ];
                let samples: Vec<u32> = corners
                    .iter()
                    .map(|corner| evaluate_u32(corner, &terms))
                    .collect();
                assert_eq!(
                    samples,
                    [
                        row * side + column,
                        row * side + column + 1,
                        (row + 1) * side + column,
                        (row + 1) * side + column + 1,
                    ],
                    "the corners of grid point ({column}, {row}) must be its own row and column"
                );
                // Three of the four corners, over three overlays: the field's first, middle and last
                // slots, so an index that reads another overlay's word fails too.
                let probes = [(0u32, samples[0]), (2, samples[3]), (4, samples[2])];
                for (overlay, sample) in probes {
                    let word_terms = [
                        ("overlay", overlay),
                        ("sample", sample),
                        ("WEIGHT_GRID_WORDS", words),
                    ];
                    let word = evaluate_u32(word_of, &word_terms) as usize;
                    let component = evaluate_u32(component_of, &[("sample", sample)]) as usize;
                    assert_eq!(
                        settings.weights[word][component],
                        (overlay * side * side + sample) as f32,
                        "overlay {overlay} sample {sample} must read the Rust side's value"
                    );
                }
            }
        }
    }

    /// The parts of `terrain.wgsl`'s weight arithmetic the evaluator cannot run, pinned as the
    /// shader writes them: the two places that bound the sample grid, where a bound of
    /// `WEIGHT_GRID_SIDE` instead of the last sample would read the next quadrant's edge texels
    /// along this one, and the fragment's quadrant-local coordinate.
    #[test]
    fn shader_weight_field_bounds_its_grid_at_the_last_sample() {
        let source = include_str!("shaders/terrain.wgsl");
        let side = evaluate_u32(shader_constant_expression(source, "WEIGHT_GRID_SIDE"), &[]);
        let last = evaluate_u32(
            shader_constant_expression(source, "GRID_LAST_SAMPLE"),
            &[("WEIGHT_GRID_SIDE", side)],
        );
        assert_eq!(
            last,
            side - 1,
            "the grid's far edge is its last sample index, not the grid's side"
        );

        let point = "clamp(coordinate, vec2<f32>(0.0), vec2<f32>(f32(GRID_LAST_SAMPLE)))";
        assert!(
            without_whitespace(shader_function_body(source, "grid_point"))
                .contains(&without_whitespace(point)),
            "a coordinate must be clamped onto the sample square before its blend fraction is taken"
        );
        let next = "min(base + vec2<u32>(1u), vec2<u32>(GRID_LAST_SAMPLE))";
        assert!(
            without_whitespace(shader_function_body(source, "grid_weight"))
                .contains(&without_whitespace(next)),
            "the blend must pair the far edge's last sample with itself"
        );
        let coordinate = "(in.uv * 2.0 - terrain.quadrant_origin.xy) * f32(GRID_LAST_SAMPLE)";
        assert!(
            without_whitespace(source).contains(&without_whitespace(coordinate)),
            "the fragment's coordinate must be quadrant-local, on the grid's own scale"
        );
    }
}
