use crate::{
    profiling::ProfilingState,
    world::{cache::TerrainSnapshot, database::AssetCatalog},
};
use bevy::{
    asset::embedded_asset,
    camera::{RenderTarget, visibility::RenderLayers},
    core_pipeline::{mip_generation::experimental::depth::ViewDepthPyramid, prepass::DepthPrepass},
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    pbr::{ExtendedMaterial, MaterialExtension},
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainExtension>;
pub type WaterMaterial = ExtendedMaterial<StandardMaterial, WaterExtension>;

fn repeating_terrain_sampler() -> ImageSampler {
    ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    })
}

pub struct VercidiumRendererPlugin;

impl Plugin for VercidiumRendererPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/terrain.wgsl");
        embedded_asset!(app, "shaders/water.wgsl");
        app.add_plugins((
            MaterialPlugin::<TerrainMaterial>::default(),
            MaterialPlugin::<WaterMaterial>::default(),
        ))
        .init_resource::<RendererMetrics>()
        .add_systems(Startup, setup_water_reflection)
        .add_systems(
            Update,
            (
                animate_water_materials,
                update_water_reflection_camera,
                sync_renderer_metrics,
            ),
        );

        let bridge = RendererProofBridge::default();
        app.insert_resource(bridge.clone());
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.insert_resource(bridge).add_systems(
                Render,
                sample_renderer_path.after(RenderSystems::PrepareResourcesCollectPhaseBuffers),
            );
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
    #[sampler(101)]
    layer_0: Option<Handle<Image>>,
    #[texture(102)]
    #[sampler(103)]
    layer_1: Option<Handle<Image>>,
    #[texture(104)]
    #[sampler(105)]
    layer_2: Option<Handle<Image>>,
    #[texture(106)]
    #[sampler(107)]
    layer_3: Option<Handle<Image>>,
    #[texture(108)]
    #[sampler(109)]
    layer_4: Option<Handle<Image>>,
    #[texture(110)]
    #[sampler(111)]
    layer_5: Option<Handle<Image>>,
    #[uniform(112)]
    settings: TerrainSettings,
}

#[derive(ShaderType, Reflect, Debug, Clone)]
struct TerrainSettings {
    tiling_and_layer_count: Vec4,
    fallback_weights_0: Vec4,
    fallback_weights_1: Vec4,
}

impl TerrainExtension {
    pub fn from_quadrant(
        terrain: &TerrainSnapshot,
        quadrant: u8,
        catalog: &AssetCatalog,
        asset_server: &AssetServer,
    ) -> Result<(Self, Vec<Handle<Image>>), String> {
        let mut textures: [Option<Handle<Image>>; 6] = std::array::from_fn(|_| None);
        let layers = crate::streaming::quadrant_layers(terrain, quadrant)?;
        let mut handles = Vec::with_capacity(layers.len());
        for (target, layer) in textures.iter_mut().zip(&layers) {
            if layer.is_base && layer.texture_form_id == 0 {
                continue;
            }
            let path = catalog
                .landscape_diffuse(layer.texture_form_id)
                .ok_or_else(|| {
                    format!(
                        "LAND {:08X} quadrant {quadrant} texture {:08X} has no diffuse image",
                        terrain.cell_id, layer.texture_form_id
                    )
                })?;
            let handle = asset_server
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.sampler = repeating_terrain_sampler();
                })
                .load(path.to_owned());
            *target = Some(handle.clone());
            handles.push(handle);
        }
        Ok((
            Self {
                layer_0: textures[0].clone(),
                layer_1: textures[1].clone(),
                layer_2: textures[2].clone(),
                layer_3: textures[3].clone(),
                layer_4: textures[4].clone(),
                layer_5: textures[5].clone(),
                settings: TerrainSettings {
                    tiling_and_layer_count: Vec4::new(8.0, 8.0, layers.len() as f32, 0.0),
                    fallback_weights_0: Vec4::new(1.0, 0.0, 0.0, 0.0),
                    fallback_weights_1: Vec4::ZERO,
                },
            },
            handles,
        ))
    }

    pub(crate) fn fixture(textures: [Handle<Image>; 6]) -> Self {
        Self {
            layer_0: Some(textures[0].clone()),
            layer_1: Some(textures[1].clone()),
            layer_2: Some(textures[2].clone()),
            layer_3: Some(textures[3].clone()),
            layer_4: Some(textures[4].clone()),
            layer_5: Some(textures[5].clone()),
            settings: TerrainSettings {
                tiling_and_layer_count: Vec4::new(8.0, 8.0, 6.0, 0.0),
                fallback_weights_0: Vec4::X,
                fallback_weights_1: Vec4::ZERO,
            },
        }
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
            settings: TerrainSettings {
                tiling_and_layer_count: Vec4::new(8.0, 8.0, 0.0, 0.0),
                fallback_weights_0: Vec4::X,
                fallback_weights_1: Vec4::ZERO,
            },
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
}

impl Default for WaterExtension {
    fn default() -> Self {
        Self {
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, 0.32, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, 0.0),
            },
            reflection: None,
            flow_normal: None,
        }
    }
}

impl WaterExtension {
    pub fn with_reflection(reflection: Handle<Image>, flow_normal: Option<Handle<Image>>) -> Self {
        let has_flow_normal = flow_normal.is_some() as u8 as f32;
        Self {
            reflection: Some(reflection),
            flow_normal,
            settings: WaterSettings {
                wave_scale_speed_strength: Vec4::new(0.006, 0.15, 0.32, 0.0),
                flow_direction: Vec4::new(0.8, 0.35, 0.0, has_flow_normal),
            },
        }
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
) {
    let started = std::time::Instant::now();
    let elapsed = if config.is_some_and(|config| config.terrain_water_fixture) {
        1.0
    } else {
        time.elapsed_secs()
    };
    for (_, material) in materials.iter_mut() {
        material.extension.settings.wave_scale_speed_strength.w = elapsed;
    }
    profiler.record_elapsed("render/water_animation", started);
}

#[derive(Resource, Clone)]
pub struct WaterReflectionTexture(pub Handle<Image>);

#[derive(Component)]
struct WaterReflectionCamera;

fn setup_water_reflection(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(Image::new_target_texture(
        1024,
        576,
        bevy::render::render_resource::TextureFormat::Rgba8Unorm,
        Some(bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb),
    ));
    commands.insert_resource(WaterReflectionTexture(image.clone()));
    commands.spawn((
        Camera3d::default(),
        Camera {
            order: -1,
            invert_culling: true,
            is_active: false,
            ..default()
        },
        RenderTarget::Image(image.into()),
        Transform::default(),
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::layer(0),
        WaterReflectionCamera,
    ));
}

fn update_water_reflection_camera(
    main_camera: Query<
        &GlobalTransform,
        (
            With<crate::world::components::StreamingCamera>,
            Without<WaterReflectionCamera>,
        ),
    >,
    water: Query<&GlobalTransform, With<crate::world::components::WaterSurface>>,
    mut reflection_camera: Query<(&mut Transform, &mut Camera), With<WaterReflectionCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let (Ok(main), Ok((mut reflection, mut camera))) =
        (main_camera.single(), reflection_camera.single_mut())
    else {
        return;
    };
    let Some(surface) = water.iter().min_by(|left, right| {
        let left_distance = (left.translation().y - main.translation().y).abs();
        let right_distance = (right.translation().y - main.translation().y).abs();
        left_distance.total_cmp(&right_distance)
    }) else {
        camera.is_active = false;
        return;
    };
    let water_y = surface.translation().y;
    *reflection = reflected_camera_transform(main, water_y);
    camera.is_active = true;
    profiler.record_elapsed("render/water_reflection_camera", started);
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
    fn terrain_sampler_repeats_with_linear_mip_filtering() {
        let ImageSampler::Descriptor(sampler) = repeating_terrain_sampler() else {
            panic!("terrain sampler must have a descriptor");
        };
        assert_eq!(sampler.address_mode_u, ImageAddressMode::Repeat);
        assert_eq!(sampler.address_mode_v, ImageAddressMode::Repeat);
        assert_eq!(sampler.mag_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.min_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.mipmap_filter, bevy::image::ImageFilterMode::Linear);
        assert_eq!(sampler.anisotropy_clamp, 1);
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
}
