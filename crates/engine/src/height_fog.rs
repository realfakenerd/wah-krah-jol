//! Exponential height fog: a screen-space pass over the scene's depth that thickens the air
//! towards the ground, so valleys and lakes sit in haze while peaks stay clear.
//!
//! The fog model is the analytical path of Community Shaders' Exponential Height Fog (two
//! exponential layers integrated in closed form along each view ray, plus Henyey-Greenstein sun
//! inscattering); see `shaders/height_fog.wgsl` for the files it is ported from and
//! `THIRD_PARTY.md` for the licence. Densities and falloffs keep that feature's units: per world
//! unit, scaled by 0.001, with `exp2` falloff, which are Skyrim units here as well.
//!
//! The pass runs on top of the weather's distance fog rather than replacing it, the way the
//! reference composes height fog with vanilla fog when vanilla fog is left on. Its base colour
//! is the camera's distance-fog colour, so it only exists where the weather fog does: exteriors
//! with fog enabled.
//!
//! The pass reads the main pass's depth buffer, not the depth prepass: merged terrain stays out
//! of the prepass (`terrain_merge::material`), and fog computed from prepass depth paints the
//! distance behind it over the terrain itself.

use crate::sky::FogCamera;
use bevy::{
    asset::{embedded_asset, load_embedded_asset},
    core_pipeline::{
        Core3dSystems, FullscreenShader, tonemapping::tonemapping as tonemapping_system,
    },
    pbr::DistanceFog,
    prelude::*,
    render::{
        Render, RenderApp, RenderStartup, RenderSystems,
        diagnostic::RecordDiagnostics,
        extract_component::{
            ComponentUniforms, DynamicUniformIndex, ExtractComponent, ExtractComponentPlugin,
            UniformComponentPlugin,
        },
        render_resource::{
            BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, FragmentState, Operations,
            PipelineCache, RenderPassColorAttachment, RenderPassDescriptor,
            RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderStages,
            ShaderType, SpecializedRenderPipeline, SpecializedRenderPipelines, TextureFormat,
            TextureSampleType, TextureUsages,
            binding_types::{
                sampler, texture_2d, texture_depth_2d, texture_depth_2d_multisampled,
                uniform_buffer,
            },
        },
        renderer::{RenderContext, RenderDevice, ViewQuery},
        view::{
            ExtractedView, ViewDepthTexture, ViewTarget, ViewUniform, ViewUniformOffset,
            ViewUniforms,
        },
    },
    shader::Shader,
};

/// The default layer, in the reference's units. The reference ships density 0.005, falloff 0.2
/// and a base at height 0 for players to tune; these were tuned on Tamriel exteriors instead,
/// with the base below the Whiterun plains so lakes and valleys haze and the peaks stay clear.
pub const DEFAULT_DENSITY: f32 = 0.03;
pub const DEFAULT_HEIGHT_FALLOFF: f32 = 0.5;
pub const DEFAULT_HEIGHT: f32 = -3000.0;
/// The reference's default directional inscattering multiplier and anisotropy.
const DIRECTIONAL_INSCATTERING: f32 = 1.0;
const DIRECTIONAL_ANISOTROPY: f32 = 0.2;
/// The reference's per-world-unit scale for densities and falloffs.
const UNIT_SCALE: f32 = 0.001;

/// One exponential fog layer in the reference's units: density and falloff before the 0.001
/// scale, and the layer's base as a world height in Skyrim units (the runtime's Y).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeightFogLayer {
    pub density: f32,
    pub height_falloff: f32,
    pub height: f32,
}

impl Default for HeightFogLayer {
    fn default() -> Self {
        Self {
            density: DEFAULT_DENSITY,
            height_falloff: DEFAULT_HEIGHT_FALLOFF,
            height: DEFAULT_HEIGHT,
        }
    }
}

impl HeightFogLayer {
    /// Parses `density,falloff,height`, the form `--height-fog` takes.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(',').map(|part| part.trim().parse::<f32>().ok());
        let (Some(Some(density)), Some(Some(height_falloff)), Some(Some(height)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let layer = Self {
            density,
            height_falloff,
            height,
        };
        // Infinity passes the sign checks, and an infinite falloff times a zero height
        // difference is NaN in the shader.
        (layer.density.is_finite()
            && layer.density >= 0.0
            && layer.height_falloff.is_finite()
            && layer.height_falloff > 0.0
            && layer.height.is_finite())
        .then_some(layer)
    }
}

/// The per-camera uniform the fog shader reads. Field order matches `HeightFog` in
/// `shaders/height_fog.wgsl`.
#[derive(Component, ExtractComponent, ShaderType, Debug, Clone, Copy, Default, PartialEq)]
pub struct HeightFog {
    pub inscattering_color: Vec3,
    pub directional_inscattering: f32,
    pub sun_direction: Vec3,
    pub directional_anisotropy: f32,
    pub sun_color: Vec3,
    pub start_distance: f32,
    pub density: f32,
    pub height_falloff: f32,
    pub height: f32,
    pub sky_distance: f32,
    pub density2: f32,
    pub height_falloff2: f32,
    pub height2: f32,
    pub padding: f32,
}

impl HeightFog {
    fn new(
        layer: &HeightFogLayer,
        fog: &DistanceFog,
        sun: Option<(&DirectionalLight, &GlobalTransform)>,
        sky_distance: f32,
    ) -> Self {
        let (sun_direction, sun_color) = sun.map_or((Vec3::Y, Vec3::ZERO), |(light, transform)| {
            // A directional light shines along its forward axis; the fog wants the way back.
            let colour = light.color.to_linear();
            (
                transform.back().as_vec3(),
                Vec3::new(colour.red, colour.green, colour.blue) * light.illuminance,
            )
        });
        let colour = fog.color.to_linear();
        Self {
            inscattering_color: Vec3::new(colour.red, colour.green, colour.blue),
            directional_inscattering: DIRECTIONAL_INSCATTERING,
            sun_direction,
            directional_anisotropy: DIRECTIONAL_ANISOTROPY,
            sun_color,
            start_distance: 0.0,
            density: layer.density * UNIT_SCALE,
            height_falloff: layer.height_falloff * UNIT_SCALE,
            height: layer.height,
            sky_distance,
            density2: 0.0,
            height_falloff2: DEFAULT_HEIGHT_FALLOFF * UNIT_SCALE,
            height2: 0.0,
            padding: 0.0,
        }
    }
}

/// Registers the height-fog pass and keeps each fogged camera's uniform current.
pub struct HeightFogPlugin;

impl Plugin for HeightFogPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/height_fog.wgsl");
        app.add_plugins((
            ExtractComponentPlugin::<HeightFog>::default(),
            UniformComponentPlugin::<HeightFog>::default(),
        ))
        .add_systems(PostUpdate, apply_height_fog);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_resource::<SpecializedRenderPipelines<HeightFogPipeline>>()
            .add_systems(RenderStartup, init_height_fog_pipeline)
            .add_systems(
                Render,
                prepare_height_fog_pipelines.in_set(RenderSystems::Prepare),
            )
            .add_systems(
                bevy::core_pipeline::schedule::Core3d,
                // Scene-linear HDR, before the display transform, like the distance fog it
                // follows.
                height_fog_pass
                    .in_set(Core3dSystems::PostProcess)
                    .before(tonemapping_system),
            );
    }
}

/// The layer the settings ask for. `--height-fog` reaches it by seeding the settings at launch,
/// so the menu and the pass cannot disagree about what is on.
fn wanted_layer(rendering: &crate::settings::RenderingSettings) -> Option<HeightFogLayer> {
    rendering.height_fog.then_some(HeightFogLayer {
        density: DEFAULT_DENSITY * rendering.height_fog_density_scale,
        height_falloff: rendering.height_fog_falloff,
        height: rendering.height_fog_height,
    })
}

/// Marks a camera whose depth texture gained `TEXTURE_BINDING` because of the fog, so switching
/// the fog off clears exactly that bit and leaves any usage something else asked for.
#[derive(Component)]
struct HeightFogDepthBinding;

/// Gives every fogged camera the uniform for this frame, and removes it where the weather fog is
/// gone (interiors, fog switched off) or the height fog is off.
#[allow(clippy::type_complexity)]
fn apply_height_fog(
    mut commands: Commands,
    config: Option<Res<crate::config::EngineConfig>>,
    rendering: Option<Res<crate::settings::RenderingSettings>>,
    mut cameras: Query<
        (
            Entity,
            Option<&DistanceFog>,
            &Projection,
            Option<&HeightFog>,
            Option<&mut Camera3d>,
            Has<HeightFogDepthBinding>,
        ),
        With<FogCamera>,
    >,
    suns: Query<(&DirectionalLight, &GlobalTransform)>,
    mut launch_settings: Local<Option<crate::settings::RenderingSettings>>,
) {
    // Runs without a settings menu (captures, benchmarks) never insert the settings resource;
    // the command line seeds an equivalent one here.
    let launch = launch_settings.get_or_insert_with(|| {
        let mut settings = crate::settings::RenderingSettings::default();
        if let Some(layer) = config.as_deref().and_then(|config| config.height_fog) {
            settings.seed_height_fog(&layer);
        }
        settings
    });
    let layer = wanted_layer(rendering.as_deref().unwrap_or(launch));
    let sun = suns.iter().next();
    for (camera, fog, projection, current, camera_3d, added_binding) in &mut cameras {
        let wanted = match (fog, &layer) {
            (Some(fog), Some(layer)) => {
                let sky_distance = match projection {
                    Projection::Perspective(perspective) => perspective.far,
                    _ => 0.0,
                };
                Some(HeightFog::new(layer, fog, sun, sky_distance))
            }
            _ => None,
        };
        match (wanted, current) {
            (Some(wanted), Some(current)) if wanted == *current => {}
            (Some(wanted), _) => {
                // The pass samples the main depth buffer, which is otherwise only a render
                // attachment unless occlusion culling happens to need it too.
                if let Some(mut camera_3d) = camera_3d {
                    let usages = TextureUsages::from(camera_3d.depth_texture_usages);
                    if !usages.contains(TextureUsages::TEXTURE_BINDING) {
                        camera_3d.depth_texture_usages =
                            (usages | TextureUsages::TEXTURE_BINDING).into();
                        commands.entity(camera).insert(HeightFogDepthBinding);
                    }
                }
                commands.entity(camera).insert(wanted);
            }
            (None, Some(_)) => {
                commands.entity(camera).remove::<HeightFog>();
                if added_binding {
                    if let Some(mut camera_3d) = camera_3d {
                        let usages = TextureUsages::from(camera_3d.depth_texture_usages);
                        camera_3d.depth_texture_usages =
                            (usages - TextureUsages::TEXTURE_BINDING).into();
                    }
                    commands.entity(camera).remove::<HeightFogDepthBinding>();
                }
            }
            (None, None) => {}
        }
    }
}

#[derive(Resource)]
struct HeightFogPipeline {
    layout: BindGroupLayoutDescriptor,
    multisampled_layout: BindGroupLayoutDescriptor,
    sampler: Sampler,
    shader: Handle<Shader>,
    fullscreen: FullscreenShader,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct HeightFogPipelineKey {
    target_format: TextureFormat,
    multisampled: bool,
}

impl SpecializedRenderPipeline for HeightFogPipeline {
    type Key = HeightFogPipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        let (layout, shader_defs) = if key.multisampled {
            (
                self.multisampled_layout.clone(),
                vec!["MULTISAMPLED".into()],
            )
        } else {
            (self.layout.clone(), Vec::new())
        };
        RenderPipelineDescriptor {
            label: Some("height_fog_pipeline".into()),
            layout: vec![layout],
            vertex: self.fullscreen.to_vertex_state(),
            fragment: Some(FragmentState {
                shader: self.shader.clone(),
                shader_defs,
                targets: vec![Some(ColorTargetState {
                    format: key.target_format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
                ..default()
            }),
            ..default()
        }
    }
}

fn height_fog_layout(label: &'static str, multisampled: bool) -> BindGroupLayoutDescriptor {
    let depth = if multisampled {
        texture_depth_2d_multisampled()
    } else {
        texture_depth_2d()
    };
    BindGroupLayoutDescriptor::new(
        label,
        &BindGroupLayoutEntries::sequential(
            ShaderStages::FRAGMENT,
            (
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
                uniform_buffer::<HeightFog>(true),
                uniform_buffer::<ViewUniform>(true),
                depth,
            ),
        ),
    )
}

fn init_height_fog_pipeline(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    asset_server: Res<AssetServer>,
    fullscreen: Res<FullscreenShader>,
) {
    commands.insert_resource(HeightFogPipeline {
        layout: height_fog_layout("height_fog_bind_group_layout", false),
        multisampled_layout: height_fog_layout("height_fog_multisampled_bind_group_layout", true),
        sampler: render_device.create_sampler(&SamplerDescriptor::default()),
        shader: load_embedded_asset!(asset_server.as_ref(), "shaders/height_fog.wgsl"),
        fullscreen: fullscreen.clone(),
    });
}

#[derive(Component)]
struct HeightFogPipelineId(CachedRenderPipelineId);

fn prepare_height_fog_pipelines(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    pipeline: Option<Res<HeightFogPipeline>>,
    mut pipelines: ResMut<SpecializedRenderPipelines<HeightFogPipeline>>,
    views: Query<(Entity, &ExtractedView, &Msaa), With<HeightFog>>,
) {
    let Some(pipeline) = pipeline else {
        return;
    };
    for (entity, view, msaa) in &views {
        let key = HeightFogPipelineKey {
            target_format: view.target_format,
            multisampled: msaa.samples() > 1,
        };
        let id = pipelines.specialize(&pipeline_cache, &pipeline, key);
        commands.entity(entity).insert(HeightFogPipelineId(id));
    }
}

#[allow(clippy::type_complexity)]
fn height_fog_pass(
    view: ViewQuery<
        (
            &ViewTarget,
            &ViewDepthTexture,
            &DynamicUniformIndex<HeightFog>,
            &ViewUniformOffset,
            &HeightFogPipelineId,
            &Msaa,
        ),
        // The uniform index and pipeline id are render-world components that can outlive a
        // camera's fog when it is switched off; the extracted fog itself cannot.
        With<HeightFog>,
    >,
    pipeline: Res<HeightFogPipeline>,
    pipeline_cache: Res<PipelineCache>,
    fog_uniforms: Res<ComponentUniforms<HeightFog>>,
    view_uniforms: Res<ViewUniforms>,
    render_device: Res<RenderDevice>,
    mut ctx: RenderContext,
) {
    let (view_target, depth, fog_index, view_offset, pipeline_id, msaa) = view.into_inner();
    let (Some(render_pipeline), Some(fog_binding), Some(view_binding)) = (
        pipeline_cache.get_render_pipeline(pipeline_id.0),
        fog_uniforms.uniforms().binding(),
        view_uniforms.uniforms.binding(),
    ) else {
        return;
    };
    let layout = if msaa.samples() > 1 {
        &pipeline.multisampled_layout
    } else {
        &pipeline.layout
    };

    let post_process = view_target.post_process_write();
    let bind_group = render_device.create_bind_group(
        "height_fog_bind_group",
        &pipeline_cache.get_bind_group_layout(layout),
        &BindGroupEntries::sequential((
            post_process.source,
            &pipeline.sampler,
            fog_binding,
            view_binding,
            depth.view(),
        )),
    );
    // The span name becomes `render/height_fog/elapsed_gpu` in the diagnostics store.
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let time_span = diagnostics.time_span(ctx.command_encoder(), "height_fog");
    {
        let mut render_pass = ctx
            .command_encoder()
            .begin_render_pass(&RenderPassDescriptor {
                label: Some("height_fog_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: post_process.destination,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations::default(),
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        render_pass.set_pipeline(render_pipeline);
        render_pass.set_bind_group(0, &bind_group, &[fog_index.index(), view_offset.offset]);
        render_pass.draw(0..3, 0..1);
    }
    time_span.end(ctx.command_encoder());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded source with Bevy's imports replaced by the fields it reads and the
    /// `MULTISAMPLED` branch resolved the way the shader processor would.
    fn standalone_shader(multisampled: bool) -> String {
        let mut keep = true;
        let mut lines = Vec::new();
        for line in include_str!("shaders/height_fog.wgsl").lines() {
            match line.trim() {
                "#ifdef MULTISAMPLED" => keep = multisampled,
                "#else" => keep = !multisampled,
                "#endif" => keep = true,
                _ if keep => lines.push(line),
                _ => {}
            }
        }
        lines
            .join("\n")
            .replace(
                "#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput",
                "struct FullscreenVertexOutput {\n\
                    @builtin(position) position: vec4<f32>,\n\
                    @location(0) uv: vec2<f32>,\n\
                }",
            )
            .replace(
                "#import bevy_render::view::View",
                "struct View {\n\
                    world_from_clip: mat4x4<f32>,\n\
                    world_position: vec3<f32>,\n\
                    exposure: f32,\n\
                }",
            )
    }

    #[test]
    fn shader_validates_and_matches_uniform_layout() {
        for multisampled in [false, true] {
            let module = wgpu::naga::front::wgsl::parse_str(&standalone_shader(multisampled))
                .unwrap_or_else(|error| panic!("multisampled={multisampled}: {error}"));
            wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(),
                wgpu::naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap();
            let uniform = module
                .types
                .iter()
                .find_map(|(_, ty)| (ty.name.as_deref() == Some("HeightFog")).then_some(&ty.inner))
                .unwrap();
            let wgpu::naga::TypeInner::Struct { members, span } = uniform else {
                panic!("height fog uniform must be a struct");
            };
            assert_eq!(u64::from(*span), HeightFog::min_size().get());
            assert_eq!(
                members
                    .iter()
                    .map(|member| member.offset)
                    .collect::<Vec<_>>(),
                [0, 12, 16, 28, 32, 44, 48, 52, 56, 60, 64, 68, 72, 76]
            );
        }
    }

    #[test]
    fn fog_takes_the_weather_colour_and_the_sun_behind_the_light() {
        let fog = DistanceFog {
            color: Color::linear_rgb(0.25, 0.5, 0.75),
            ..default()
        };
        let light = DirectionalLight {
            color: Color::linear_rgb(1.0, 0.5, 0.25),
            illuminance: 1000.0,
            ..default()
        };
        let transform =
            GlobalTransform::from(Transform::default().looking_to(Vec3::NEG_Y, Vec3::Z));
        let uniform = HeightFog::new(
            &HeightFogLayer::default(),
            &fog,
            Some((&light, &transform)),
            50_000.0,
        );
        assert_eq!(uniform.inscattering_color, Vec3::new(0.25, 0.5, 0.75));
        assert!(uniform.sun_direction.abs_diff_eq(Vec3::Y, 1e-6));
        assert_eq!(uniform.sun_color, Vec3::new(1000.0, 500.0, 250.0));
        assert_eq!(uniform.density, DEFAULT_DENSITY * UNIT_SCALE);
        assert_eq!(uniform.sky_distance, 50_000.0);
    }

    #[test]
    fn layer_parses_density_falloff_and_height() {
        assert_eq!(
            HeightFogLayer::parse("0.05, 0.4,-1200"),
            Some(HeightFogLayer {
                density: 0.05,
                height_falloff: 0.4,
                height: -1200.0,
            })
        );
        for bad in [
            "",
            "0.05,0.4",
            "0.05,0.4,0,1",
            "-1,0.4,0",
            "0.05,0,0",
            "a,b,c",
        ] {
            assert_eq!(HeightFogLayer::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn layer_rejects_non_finite_values_in_every_position() {
        for bad in ["inf", "-inf", "NaN"] {
            for text in [
                format!("{bad},0.4,0"),
                format!("0.05,{bad},0"),
                format!("0.05,0.4,{bad}"),
            ] {
                assert_eq!(HeightFogLayer::parse(&text), None, "{text:?}");
            }
        }
    }

    fn launch(args: &[&str]) -> crate::config::EngineConfig {
        let args = args.iter().map(|arg| (*arg).to_owned());
        match crate::config::EngineConfig::from_args(args).unwrap() {
            crate::config::ConfigAction::Run(config) => *config,
            crate::config::ConfigAction::Help => panic!("expected a run"),
        }
    }

    fn seeded(args: &[&str]) -> crate::settings::RenderingSettings {
        let mut settings = crate::settings::RenderingSettings::default();
        settings.seed_height_fog(&launch(args).height_fog.expect("--height-fog was given"));
        settings
    }

    #[test]
    fn settings_alone_decide_the_layer() {
        let mut rendering = crate::settings::RenderingSettings::default();
        assert_eq!(wanted_layer(&rendering), None);
        rendering.height_fog = true;
        rendering.height_fog_density_scale = 2.0;
        rendering.height_fog_falloff = 0.25;
        rendering.height_fog_height = -300.0;
        assert_eq!(
            wanted_layer(&rendering),
            Some(HeightFogLayer {
                density: DEFAULT_DENSITY * 2.0,
                height_falloff: 0.25,
                height: -300.0,
            })
        );
    }

    #[test]
    fn the_flag_seeds_the_settings() {
        let flag = HeightFogLayer {
            density: 0.06,
            height_falloff: 0.4,
            height: 500.0,
        };
        let rendering = seeded(&["--height-fog", "0.06,0.4,500"]);
        assert!(rendering.height_fog, "the menu shows the fog as on");
        assert_eq!(rendering.height_fog_density_scale, 0.06 / DEFAULT_DENSITY);
        assert_eq!(rendering.height_fog_falloff, 0.4);
        assert_eq!(rendering.height_fog_height, 500.0);
        let layer = wanted_layer(&rendering).unwrap();
        assert!((layer.density - flag.density).abs() < 1e-6);
        assert_eq!(layer.height_falloff, flag.height_falloff);
        assert_eq!(layer.height, flag.height);
    }

    #[test]
    fn switching_the_menu_entry_off_removes_the_layer() {
        let mut rendering = seeded(&["--height-fog", "0.06,0.4,500"]);
        assert!(wanted_layer(&rendering).is_some());
        rendering.height_fog = false;
        assert_eq!(wanted_layer(&rendering), None);
    }

    #[test]
    fn the_flags_falloff_survives_toggling() {
        let mut rendering = seeded(&["--height-fog", "0.06,0.4,500"]);
        rendering.height_fog = false;
        rendering.height_fog = true;
        assert_eq!(wanted_layer(&rendering).unwrap().height_falloff, 0.4);
    }

    fn fog_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(crate::settings::RenderingSettings {
                height_fog: true,
                ..default()
            })
            .add_systems(Update, apply_height_fog);
        let camera = app
            .world_mut()
            .spawn((
                FogCamera,
                Camera3d::default(),
                Projection::default(),
                DistanceFog::default(),
            ))
            .id();
        (app, camera)
    }

    fn depth_usages(app: &App, camera: Entity) -> TextureUsages {
        let camera = app.world().get::<Camera3d>(camera).unwrap();
        TextureUsages::from(camera.depth_texture_usages)
    }

    #[test]
    fn apply_adds_the_uniform_and_depth_binding_and_removes_the_uniform_with_the_weather_fog() {
        let (mut app, camera) = fog_app();
        app.update();
        assert!(app.world().get::<HeightFog>(camera).is_some());
        assert!(depth_usages(&app, camera).contains(TextureUsages::TEXTURE_BINDING));

        app.world_mut().entity_mut(camera).remove::<DistanceFog>();
        app.update();
        assert!(app.world().get::<HeightFog>(camera).is_none());
        assert_eq!(
            depth_usages(&app, camera),
            TextureUsages::RENDER_ATTACHMENT,
            "off leaves the depth texture as it was before the fog"
        );
    }

    #[test]
    fn switching_the_setting_off_restores_the_depth_usages() {
        let (mut app, camera) = fog_app();
        app.update();
        app.world_mut()
            .resource_mut::<crate::settings::RenderingSettings>()
            .height_fog = false;
        app.update();
        assert!(app.world().get::<HeightFog>(camera).is_none());
        assert_eq!(depth_usages(&app, camera), TextureUsages::RENDER_ATTACHMENT);
    }

    #[test]
    fn a_binding_requested_elsewhere_stays_when_the_fog_goes() {
        let (mut app, camera) = fog_app();
        let wanted = TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING;
        app.world_mut()
            .get_mut::<Camera3d>(camera)
            .unwrap()
            .depth_texture_usages = wanted.into();
        app.update();
        app.world_mut().entity_mut(camera).remove::<DistanceFog>();
        app.update();
        assert!(app.world().get::<HeightFog>(camera).is_none());
        assert_eq!(depth_usages(&app, camera), wanted);
    }
}
