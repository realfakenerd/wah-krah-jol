use crate::{
    color_pipeline::{SceneGradeSource, SkyrimCinematicPostPlugin},
    config::{EngineConfig, MAX_IO_THREADS},
    console::{ConsolePlugin, console_closed},
    metrics::AcceptanceMetricsPlugin,
    physics::{MovementTuning, PhysicsFixturePlugin, WorldPlayerPlugin},
    profiling::{ProfilingPlugin, ProfilingState},
    render::{
        LIGHT_LAYERS, MAIN_VIEW_LAYERS, RendererMetrics, TerrainExtension, TerrainMaterial,
        VercidiumRendererPlugin, WATER_LAYER, WaterExtension, WaterMaterial,
        WaterReflectionTexture, terrain_layer_sampler,
    },
    renderer_init::RendererInitPlugin,
    route::{RouteGround, RoutePlan, RouteRunner, drive_route},
    shots::{ShotOutputImage, ShotsFile, ShotsPlugin, ShotsRun, default_output_dir},
    sky::{FogCamera, SkyCamera, SkyPlugin},
    streaming::{
        AssetFailure, RenderOrigin, StreamingMetrics, StreamingPlugin, StreamingWorld,
        build_terrain_quadrant_mesh, streaming_center, validate_standard_material,
    },
    tone_mapping::SceneToneMapSource,
    world::{
        actor_catalog::ActorCatalog,
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CellRef, ExpectedModelBounds, FormId, InstanceBounds, StreamedCellRoot, StreamingCamera,
        },
        database::{AssetCatalog, CellKey, WorldDatabase},
    },
};
use bevy::{
    asset::{AssetPlugin, RenderAssetUsages},
    camera::primitives::MeshAabb,
    camera::visibility::RenderLayers,
    core_pipeline::prepass::DepthPrepass,
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    light::{CascadeShadowConfig, CascadeShadowConfigBuilder, DirectionalLightShadowMap},
    log::{Level, LogPlugin},
    math::DVec3,
    prelude::*,
    render::diagnostic::RenderDiagnosticsPlugin,
    render::error_handler::{ErrorType, RenderError, RenderErrorHandler, RenderErrorPolicy},
    render::occlusion_culling::OcclusionCulling,
    render::render_asset::RenderAssetBytesPerFrame,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    render::view::screenshot::{Screenshot, save_to_disk},
    tasks::{IoTaskPool, TaskPoolBuilder},
    window::{MonitorSelection, PresentMode, WindowPlugin, WindowPosition},
    winit::WinitSettings,
};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Resource)]
struct InitialCameraGroundHeight(f32);

#[derive(Resource)]
struct InitialWorldCameraPose {
    shot: crate::shots::Shot,
    aspect: f32,
    resolution: bevy::window::WindowResolution,
}

fn prepare_start_camera(config: &mut EngineConfig) -> Result<Option<InitialWorldCameraPose>> {
    let Some(path) = &config.start_shot else {
        return Ok(None);
    };
    color_eyre::eyre::ensure!(
        config.shots.is_none()
            && config.matched_route.is_none()
            && !config.benchmark_only
            && config.benchmark_frames.is_none()
            && config.benchmark_duration_secs.is_none()
            && config.benchmark_jump.is_none()
            && config.auto_fly_speed == 0.0
            && config.route.is_none()
            && config.acceptance_screenshot.is_none()
            && config.streaming_benchmark_route_speed.is_none()
            && !config.material_fixture
            && !config.terrain_water_fixture
            && !config.transform_bounds_fixture
            && !config.renderer_fixture
            && !config.streaming_fixture
            && !config.physics_fixture,
        "--start-shot requires a live world run without another automated camera mode"
    );
    let file = ShotsFile::load(path)?;
    color_eyre::eyre::ensure!(
        file.shots.len() == 1 && file.shots[0].is_exterior(),
        "--start-shot requires exactly one exterior camera pose"
    );
    let (worldspace, grid) = file.start().expect("validated exterior camera pose");
    config.start_grid = grid;
    if let Some(worldspace) = worldspace {
        config.worldspace_id = worldspace;
    }
    Ok(Some(InitialWorldCameraPose {
        shot: file.shots[0].clone(),
        aspect: file.aspect(),
        resolution: file.window_resolution(),
    }))
}

#[derive(Resource)]
struct AssetDirectoryReadLock {
    _guard: shared::asset_lock::AssetLock,
}

pub fn run(mut config: EngineConfig) -> Result<()> {
    validate_fixture_selection(&config)?;
    let mut preferences = config
        .interactive_world_physics()
        .then(|| crate::settings::prepare_settings(&mut config));
    // A saved profile can disagree with an explicit launch option the parser already accepted.
    config.check_visual_profile()?;
    info!(
        profile = config.visual_profile.name(),
        "visual profile active; changing it requires a restart"
    );
    config.image_space_selection()?;
    let start_camera = prepare_start_camera(&mut config)?;
    let shots = prepare_shots(&mut config)?;
    let shots_active = shots.is_some();
    let route = prepare_route(&mut config)?;
    configure_io_task_pool(config.io_threads)?;
    let interactive_world_physics = config.interactive_world_physics();
    let streaming_fixture_dir = if config.streaming_fixture {
        let fixture = StreamingFixtureDirectory::create(config.worldspace_id, config.start_grid)?;
        config.assets_dir = fixture.path.clone();
        Some(fixture)
    } else {
        None
    };
    let asset_plugin = asset_plugin(&mut config)?;
    let reads_asset_tree = config.streaming_fixture
        || !(config.benchmark_only
            || config.material_fixture
            || config.terrain_water_fixture
            || config.transform_bounds_fixture
            || config.renderer_fixture
            || config.physics_fixture);
    let asset_lock = if reads_asset_tree {
        Some(
            shared::asset_lock::AssetLock::acquire_shared(&config.assets_dir).wrap_err_with(
                || {
                    format!(
                        "engine cannot read assets at {}",
                        config.assets_dir.display()
                    )
                },
            )?,
        )
    } else {
        None
    };
    if reads_asset_tree {
        crate::streaming::requests::validate_pack_identity(&config).wrap_err_with(|| {
            format!(
                "cannot read the converted pack manifest under {}",
                config.assets_dir.display()
            )
        })?;
    }
    // The terrain heights along a route are read here, before the window opens, so that following
    // the ground costs nothing while the run is measured.
    let mut route_ground = None;
    let runtime_data = if config.streaming_fixture {
        let database_path = config.assets_dir.join("skyrim_world.db");
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?,
            InitialCameraGroundHeight(0.0),
            None,
        ))
    } else if config.benchmark_only
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
        || config.renderer_fixture
        || config.physics_fixture
    {
        None
    } else {
        validate_runtime_assets(&config)?;
        let database_path = config.assets_dir.join("skyrim_world.db");
        let cache = CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?;
        let ground_height = initial_camera_ground_height(&config, &database_path, &cache)?;
        let spawn_ground_height = if interactive_world_physics {
            initial_walk_ground_height(
                &config,
                &database_path,
                &cache,
                start_camera.as_ref().map(|pose| &pose.shot),
            )?
        } else {
            None
        };
        if let Some(plan) = &route {
            route_ground = Some(RouteGround::load(
                plan,
                config.worldspace_id,
                &database_path,
                &cache,
            )?);
        }
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            cache,
            InitialCameraGroundHeight(ground_height),
            spawn_ground_height,
        ))
    };
    // Authored lighting, weather, climate and image space, when the database has them (#269).
    // Read only; nothing renders from it yet.
    let environment = runtime_data.as_ref().map(|_| {
        crate::world::environment::EnvironmentCatalog::open(
            &config.assets_dir.join("skyrim_world.db"),
        )
        .unwrap_or_else(|error| {
            // LogPlugin is installed later, so report this startup failure directly.
            eprintln!(
                "warning: environment: cannot open the world database ({error}); using an empty catalog"
            );
            Default::default()
        })
    });
    // The actor model catalog (#109): the referenced `NPC_` bases this world converted. The spawn
    // path reads it once per cell commit; a database without the optional tables gives an empty
    // catalog and every reference spawns exactly as before.
    let actor_catalog = runtime_data
        .as_ref()
        .map(|_| ActorCatalog::open(&config.assets_dir.join("skyrim_world.db")));
    // The streamed world's merged terrain (#215): the land texture table of this conversion, if it
    // has one. Opening it reads one small file; the arrays and chunks load as they are needed.
    let terrain_merge = runtime_data
        .is_some()
        .then(|| crate::terrain_merge::TerrainMerge::open(&config.assets_dir));
    let grade_source =
        SceneGradeSource::load(&config, runtime_data.is_some() && !config.streaming_fixture)?;
    let mut tone_map_source = SceneToneMapSource::load(&config, &grade_source)?;
    if let Some((settings, _)) = preferences.as_mut() {
        tone_map_source.bevy_tonemapping = crate::settings::TONE_MAPS[settings.tone_map];
        if let Some(mode) = crate::color_pipeline::SceneGradeMode::from_index(settings.color_grade)
        {
            let resolved = tone_map_source
                .resolve_with_overrides(
                    &grade_source,
                    mode,
                    config.visual_preset,
                    settings.grade_overrides(),
                )
                .map_err(color_eyre::eyre::Error::msg)?;
            settings.show_resolved_grade(resolved.grade, resolved.post.pivot);
        }
    }
    let sky_palette = grade_source.sky_palette(config.visual_preset);
    let movement_tuning = (interactive_world_physics
        && runtime_data.is_some()
        && !config.streaming_fixture)
        .then(|| MovementTuning::from_world_database(&config.assets_dir.join("skyrim_world.db")))
        .transpose()?;
    let benchmark_active = config.benchmark_frames.is_some()
        || config.benchmark_duration_secs.is_some()
        || config.streaming_benchmark_route_speed.is_some();
    configure_benchmark_priority(benchmark_active)?;
    let window = (!config.headless || shots_active || config.hidden_window).then(|| Window {
        title: config.window_title(),
        visible: !config.hidden_window,
        // Start the window at the reference size; shots use an independent exact-size image target.
        resolution: match &shots {
            Some(run) => run.file.window_resolution(),
            None => start_camera
                .as_ref()
                .map_or_else(|| (1600, 900).into(), |pose| pose.resolution.clone()),
        },
        // Visible timing and shots runs open in the middle of the primary screen.
        position: if benchmark_active || shots_active {
            WindowPosition::Centered(MonitorSelection::Primary)
        } else {
            WindowPosition::Automatic
        },
        present_mode: if benchmark_active && !config.benchmark_vsync {
            PresentMode::AutoNoVsync
        } else {
            PresentMode::AutoVsync
        },
        ..default()
    });
    let origin = RenderOrigin(IVec2::new(config.start_grid.0, config.start_grid.1));
    let mut app = App::new();
    if let Some((settings, file)) = preferences {
        app.insert_resource(settings).insert_resource(file);
    }
    if let Some(pose) = start_camera {
        app.insert_resource(pose);
    }
    if config.stdin_console {
        app.add_plugins(crate::live_console::StdinConsolePlugin);
    }
    if let Some(tuning) = movement_tuning {
        app.insert_resource(tuning);
    }
    if let Some(asset_lock) = asset_lock {
        app.insert_resource(AssetDirectoryReadLock { _guard: asset_lock });
    }
    if benchmark_active || shots_active || config.hidden_window {
        // Acceptance runs are commonly left unfocused while the campaign driver
        // advances through its scenarios. Bevy's game default throttles an
        // unfocused window to 60 Hz, which makes a 16.67 ms P95 gate measure the
        // event-loop sleep instead of renderer performance. A shots run left
        // behind other windows would likewise starve its settle rule of frames.
        app.insert_resource(WinitSettings::continuous());
    }
    let render_asset_budget = upload_budget(&config);
    // The run fingerprint is measurement data: an ordinary play session neither builds one nor
    // pays for the startup system that reads it.
    let records_measurements = crate::fingerprint::records_measurements(&config);
    app.insert_resource(config.visual_profile)
        .insert_resource(config.color_grade)
        .insert_resource(config.visual_preset)
        .insert_resource(sky_palette)
        .insert_resource(grade_source)
        .insert_resource(tone_map_source)
        .insert_resource(config)
        .insert_resource(origin)
        .insert_resource(render_asset_budget)
        .init_resource::<StreamingMetrics>()
        .add_plugins(
            DefaultPlugins
                .build()
                .disable::<bevy::render::RenderPlugin>()
                .add_before::<bevy::render::RenderPlugin>(RendererInitPlugin::default())
                .set(asset_plugin)
                .set(world_gltf_plugin())
                .set(WindowPlugin {
                    primary_window: window,
                    ..default()
                })
                .set(LogPlugin {
                    level: Level::INFO,
                    custom_layer: crate::bug_report::logging::report_log_layer,
                    ..default()
                }),
        )
        .add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            LogDiagnosticsPlugin {
                debug: true,
                ..default()
            },
            AcceptanceMetricsPlugin,
            ProfilingPlugin,
        ))
        .add_plugins((
            VercidiumRendererPlugin,
            crate::visibility_optimization::VisibilityOptimizationPlugin,
            crate::terrain_upload::TerrainUploadPlugin,
            crate::mesh_preparation_retry::MeshPreparationRetryPlugin,
            crate::mesh_residency_audit::MeshResidencyAuditPlugin,
            SkyPlugin,
            SkyrimCinematicPostPlugin,
            ConsolePlugin,
            crate::console_selection::ConsoleSelectionPlugin,
            crate::bug_report::BugReportPlugin,
            crate::nif_material::EffectPaletteMaterialPlugin,
            crate::nif_material::NifMaterialPlugin,
            crate::emittance::EmittancePlugin,
            crate::actor_skin_tone::ActorSkinTonePlugin,
            crate::actor_body_weight::ActorBodyWeightPlugin,
        ))
        // The render→main "prepared and drawable" bridge. Registered for every
        // run: it is what publishes the render world's own answer to "is this surface drawn?" into
        // the main world, and it changes nothing else about the frame.
        .add_plugins(crate::swap_bridge::SwapBridgePlugin)
        // Registered for every run, lights or not: the plugin owns the budget, not the spawning,
        // and `--lights` is what `streaming::spawn_cell` reads to place anything for it to budget.
        .add_plugins(crate::lights::LightsPlugin)
        .add_plugins(crate::actor_animation::ActorAnimationPlugin)
        .add_plugins(crate::actor_drive::ActorDrivePlugin)
        .add_plugins(crate::actor_head::ActorHeadPlugin)
        .add_systems(PostUpdate, keep_windows_hidden)
        .add_plugins(crate::terrain_merge::TerrainMergePlugin::new(
            terrain_merge.as_ref(),
        ));
    #[cfg(feature = "collision-measure")]
    if std::env::var_os("MUDCRAB_COLLISION_MEASURE").is_some() {
        app.add_plugins(crate::collision_measure::MeasurePlugin);
    }
    if interactive_world_physics {
        app.add_plugins(crate::settings_ui::SettingsPlugin);
    }
    add_camera_and_capture_systems(&mut app);
    if let Some(terrain_merge) = terrain_merge {
        app.insert_resource(terrain_merge);
    }
    if records_measurements {
        app.add_plugins(crate::fingerprint::RunFingerprintPlugin);
    }
    if app
        .world()
        .resource::<EngineConfig>()
        .streaming_benchmark_route_speed
        .is_some()
    {
        app.init_resource::<StreamingBenchmarkRoute>()
            .add_systems(PreUpdate, drive_streaming_benchmark_route);
    }
    if let Some((database, catalog, cache, ground_height, spawn_ground_height)) = runtime_data {
        if let Some(environment) = environment {
            app.insert_resource(environment);
        }
        if let Some(actor_catalog) = actor_catalog {
            app.insert_resource(actor_catalog);
        }
        app.insert_resource(database)
            .insert_resource(catalog)
            .insert_resource(cache)
            .insert_resource(ground_height)
            .insert_resource(crate::physics::WalkSpawnGroundHeight(spawn_ground_height))
            .add_plugins(StreamingPlugin);
        #[cfg(feature = "collision-measure")]
        if std::env::var_os("MUDCRAB_PLACEMENT_PROBE").is_some() {
            app.add_plugins(crate::scene_placement_probe::ScenePlacementProbe);
        }
        if let (Some(plan), Some(ground)) = (route, route_ground) {
            info!(
                route = %plan.source,
                waypoints = plan.waypoints.len(),
                length = plan.length(),
                speed = plan.speed,
                ground_cells = ground.cell_count(),
                "following a route"
            );
            app.insert_resource(RouteRunner::new(plan, ground))
                // Before `Update`, so the streaming planner of the same frame sees the camera
                // where the route put it.
                .add_systems(PreUpdate, drive_route);
        }
        app.add_systems(Startup, setup_world);
        if interactive_world_physics {
            app.add_plugins((WorldPlayerPlugin, crate::door_crossing::DoorCrossingPlugin));
            // `coc`/`coe` move the player, so they come with it, as `tcl` does: a run without a
            // player (a benchmark, `--shots`, headless) lists neither in Help.
            crate::cell_commands::register_cell_commands(&mut app);
        }
        if app.world().resource::<EngineConfig>().streaming_fixture {
            app.init_resource::<StreamingFixtureState>()
                .add_systems(Startup, setup_streaming_fixture_visual)
                .add_systems(
                    PreUpdate,
                    (drive_streaming_fixture, cross_streaming_fixture_interior).chain(),
                )
                .add_systems(PostUpdate, validate_streaming_fixture);
        }
    } else if app.world().resource::<EngineConfig>().material_fixture {
        app.add_systems(Startup, setup_material_fixture)
            .add_systems(Update, validate_material_fixture);
    } else if app.world().resource::<EngineConfig>().terrain_water_fixture {
        app.add_systems(PostStartup, setup_terrain_water_fixture)
            .add_systems(Update, validate_terrain_water_fixture);
    } else if app
        .world()
        .resource::<EngineConfig>()
        .transform_bounds_fixture
    {
        app.add_systems(Startup, setup_transform_bounds_fixture)
            .add_systems(Update, validate_transform_bounds_fixture);
    } else if app.world().resource::<EngineConfig>().renderer_fixture {
        app.add_systems(Startup, setup_renderer_fixture)
            .add_systems(Update, validate_renderer_fixture);
    } else if app.world().resource::<EngineConfig>().physics_fixture {
        app.add_plugins(PhysicsFixturePlugin);
    } else {
        app.add_systems(Startup, setup_world);
        app.add_systems(Startup, setup_synthetic_benchmark);
    }
    if !app.world().resource::<EngineConfig>().no_render_diagnostics {
        app.add_plugins(RenderDiagnosticsPlugin);
    }
    if app.world().resource::<EngineConfig>().stdin_console && !shots_active {
        app.add_plugins(crate::capture_frame::CaptureFramePlugin);
    }
    crate::render_teardown::install(&mut app);
    if let Some(run) = shots {
        info!(
            shots = run.file.shots.len(),
            frame = format_args!("{}x{}", run.file.width, run.file.height),
            output = %run.output_dir.display(),
            "rendering camera poses"
        );
        app.add_plugins((
            crate::capture_frame::CaptureFramePlugin,
            ShotsPlugin { run },
        ));
    }
    // `RenderPlugin` only `init_resource`s its default handler, so this one wins wherever it is
    // inserted.
    app.insert_resource(RenderErrorHandler(handle_render_error));
    let exit = app.run();
    drop(app);
    drop(streaming_fixture_dir);
    finished(&exit)
}

/// Process exit code for a run that ended because the GPU device was lost.
///
/// Distinct from 1 (any other failed run) and 2 (an unusable command line), so a benchmark or
/// acceptance script can record a lost device apart from other failures. It does not clear the
/// engine: a GPU fault or a command buffer the driver times out also loses the device.
pub const EXIT_DEVICE_LOST: u8 = 3;

/// The run ended because the GPU device was lost.
///
/// Returned from [`run`] so `main` can turn it into [`EXIT_DEVICE_LOST`] instead of the generic
/// failure code.
#[derive(Debug)]
pub struct DeviceLost;

impl std::fmt::Display for DeviceLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the GPU device was lost, the engine is exiting (exit code {EXIT_DEVICE_LOST}); the log \
             above has the driver's reason. Retry the run; if it keeps happening, the engine may \
             be the cause"
        )
    }
}

impl std::error::Error for DeviceLost {}

/// The exit a render error asks for: device loss gets its own code, everything else the generic
/// failure Bevy's default handler used.
fn exit_for_render_error(error: &RenderError) -> AppExit {
    match error.ty {
        ErrorType::DeviceLost => AppExit::from_code(EXIT_DEVICE_LOST),
        _ => AppExit::error(),
    }
}

/// Same policy as Bevy's default handler (quit and stop rendering, since carrying on after a
/// render error risks rapid flashing), but with an exit code that names device loss.
fn handle_render_error(
    error: &RenderError,
    main_world: &mut World,
    _render_world: &mut World,
) -> RenderErrorPolicy {
    if matches!(error.ty, ErrorType::DeviceLost) {
        error!(
            description = %error.description,
            "the GPU device was lost; quitting"
        );
    } else {
        error!("Quitting the application due to {:?} RenderError", error.ty);
    }
    main_world.write_message(exit_for_render_error(error));
    RenderErrorPolicy::StopRendering
}

/// What the process reports after an [`App`] has run.
///
/// A shots run that could not write an image, and an acceptance run whose gates did not pass, end
/// with [`AppExit::Error`]; dropping that would report success to whatever ran the process - the
/// acceptance script reads the exit code - so the error is handed back to `main` instead. A lost
/// device comes back as [`DeviceLost`], which `main` maps to [`EXIT_DEVICE_LOST`].
fn finished(exit: &AppExit) -> Result<()> {
    match exit {
        AppExit::Success => Ok(()),
        AppExit::Error(code) if code.get() == EXIT_DEVICE_LOST => Err(DeviceLost.into()),
        AppExit::Error(code) => Err(color_eyre::eyre::eyre!(
            "the run failed (exit code {code:?})"
        )),
    }
}

/// Reset accidental visibility changes before Bevy applies Window changes in Last.
/// The primary window starts hidden, so screenshot rendering needs no visible flash.
fn keep_windows_hidden(config: Res<EngineConfig>, mut windows: Query<&mut Window>) {
    if config.hidden_window {
        for mut window in &mut windows {
            if window.visible {
                window.visible = false;
            }
        }
    }
}

/// Bevy's per-frame render-asset byte budget, seeded from the run's option.
///
/// Textures and meshes over the budget wait for a later frame instead of being
/// prepared the moment they load, so a cell's new models arrive over a few
/// frames rather than in one upload burst. Deferred assets are never dropped,
/// and a single asset larger than the whole budget is still prepared. Images
/// are prepared before meshes and share the one budget, so while new images
/// use it up, new meshes wait for a later frame.
fn upload_budget(config: &EngineConfig) -> RenderAssetBytesPerFrame {
    RenderAssetBytesPerFrame {
        max_bytes: config.max_upload_bytes_per_frame(),
    }
}

/// Direct database reads and Bevy's file reader must use the same absolute root.
fn asset_plugin(config: &mut EngineConfig) -> Result<AssetPlugin> {
    config.assets_dir = std::path::absolute(&config.assets_dir).wrap_err_with(|| {
        format!(
            "cannot resolve asset directory {}",
            config.assets_dir.display()
        )
    })?;
    Ok(AssetPlugin {
        file_path: config.assets_dir.to_string_lossy().into_owned(),
        ..default()
    })
}

/// Matches LAND's sampler for shared repeating images before either loader caches them.
/// Bevy retains authored U/V wrapping and requires linear filters for anisotropy.
fn world_gltf_plugin() -> bevy::gltf::GltfPlugin {
    bevy::gltf::GltfPlugin {
        default_sampler: terrain_layer_sampler(),
        ..default()
    }
}

/// Reads the `--shots` file, if one was given, and points the run at its first exterior pose: that
/// shot's worldspace and grid square are where streaming starts, so the first view does not wait
/// for cells around a start the file never looks at.
fn prepare_shots(config: &mut EngineConfig) -> Result<Option<ShotsRun>> {
    let Some(path) = config
        .shots
        .clone()
        .or_else(|| config.matched_route.clone())
    else {
        return Ok(None);
    };
    color_eyre::eyre::ensure!(
        !(config.shots.is_some() && config.matched_route.is_some()),
        "--shots and --matched-route cannot be used together; choose one camera driver"
    );
    let mode = if config.matched_route.is_some() {
        "--matched-route"
    } else {
        "--shots"
    };
    let other_mode = config.benchmark_only
        || config.benchmark_frames.is_some()
        || config.benchmark_duration_secs.is_some()
        || config.acceptance_screenshot.is_some()
        || config.auto_fly_speed > 0.0
        || config.route.is_some()
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
        || config.renderer_fixture
        || config.streaming_fixture
        || config.physics_fixture;
    color_eyre::eyre::ensure!(
        !other_mode,
        "{mode} renders the converted world on its own: it cannot be combined with a benchmark, \
         an acceptance screenshot, --auto-fly-speed, --route or a fixture"
    );
    if config.headless {
        // The logger is not installed yet, so a `warn!` here would never be seen.
        eprintln!("warning: {mode} photographs the window, so --headless is ignored");
    }
    let (file, checkpoints) = if config.matched_route.is_some() {
        let (file, checkpoints) = crate::matched_route::MatchedRoute::load(&path)?.expand()?;
        (file, Some(checkpoints))
    } else {
        (ShotsFile::load(&path)?, None)
    };
    if let Some((worldspace_id, grid)) = file.start() {
        if let Some(worldspace_id) = worldspace_id {
            config.worldspace_id = worldspace_id;
        }
        config.start_grid = grid;
    }
    let output_dir = config
        .shots_out
        .clone()
        .unwrap_or_else(|| default_output_dir(&path));
    let mut run = ShotsRun::new(file, output_dir, config.worldspace_id);
    run.route_checkpoints = checkpoints;
    run.settle_timeout_seconds = config.shots_settle_timeout;
    Ok(Some(run))
}

/// Reads the `--route` file, if one was given, and starts the run where the route starts: the
/// file's worldspace, and the grid square of its first waypoint. A route needs the converted world to
/// walk in, and owns the camera, so it cannot be combined with another mode that does.
fn prepare_route(config: &mut EngineConfig) -> Result<Option<RoutePlan>> {
    let Some(path) = config.route.clone() else {
        return Ok(None);
    };
    let other_mode = config.benchmark_only
        || config.acceptance_screenshot.is_some()
        || config.auto_fly_speed > 0.0
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
        || config.renderer_fixture
        || config.streaming_fixture
        || config.physics_fixture;
    color_eyre::eyre::ensure!(
        !other_mode,
        "--route walks the converted world: it cannot be combined with --benchmark-only, \
         an acceptance screenshot, --auto-fly-speed or a fixture"
    );
    if !config.is_benchmark_run() {
        // The logger is not installed yet, so a `warn!` here would never be seen.
        eprintln!(
            "warning: --route without --benchmark-frames or --benchmark-duration moves the camera but writes no report"
        );
    }
    let plan = RoutePlan::load(&path)?;
    config.worldspace_id = plan.worldspace;
    config.start_grid = plan.start_grid();
    Ok(Some(plan))
}

fn validate_fixture_selection(config: &EngineConfig) -> Result<()> {
    let selected = [
        config.material_fixture,
        config.terrain_water_fixture,
        config.transform_bounds_fixture,
        config.renderer_fixture,
        config.streaming_fixture,
        config.physics_fixture,
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count();
    color_eyre::eyre::ensure!(selected <= 1, "select only one fixture mode");
    Ok(())
}

#[cfg(windows)]
fn configure_benchmark_priority(benchmark_active: bool) -> Result<()> {
    if benchmark_active {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle,
        // which is valid for SetPriorityClass and must not be closed.
        let configured =
            unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) };
        if configured == 0 {
            return Err(std::io::Error::last_os_error())
                .wrap_err("failed to set benchmark process priority");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn configure_benchmark_priority(_benchmark_active: bool) -> Result<()> {
    Ok(())
}

/// The stack each IO task pool thread reserves.
///
/// Asset loads nest on these stacks. bevy_asset runs every load as a task on the IO pool, and
/// bevy_gltf's loader loads a file's textures inside `IoTaskPool::scope`, whose `block_on` ticks
/// the pool's shared executor on the calling thread while it waits. So a glTF load waiting for its
/// textures picks up the next queued glTF load and runs it on the same stack, that one does the
/// same, and so on: the nesting is as deep as the queue of model loads. Measured while
/// streaming Markarth's dense city interiors: each nested load costs about 85 KiB, and one thread
/// reached 8,074 KiB (about 95 loads deep) and overflowed the 8 MiB this used to be. A dense cell
/// queues hundreds of distinct models at once (the largest interior has 522), plus its
/// neighbours, so the reservation has to cover the whole queue, not a typical load.
///
/// 128 MiB is room for about 1,500 nested loads. It is address space, not memory: the thread's stack is
/// reserved, and pages are committed only as deep as the thread reaches.
const IO_TASK_STACK_BYTES: usize = 128 * 1024 * 1024;

fn io_task_pool_builder(threads: usize) -> TaskPoolBuilder {
    TaskPoolBuilder::new()
        .num_threads(threads)
        .thread_name("IO Task Pool".to_owned())
        .stack_size(IO_TASK_STACK_BYTES)
}

/// IO pool size for `requested` threads; `0` means automatic: a quarter of the hardware threads,
/// at least one and at most four (16 -> 4, 8 -> 2, 4 -> 1, 64 -> 4). A requested size is used as
/// given, up to [`MAX_IO_THREADS`]: a caller that builds `EngineConfig` by hand skips the
/// `--io-threads` check, and each thread reserves [`IO_TASK_STACK_BYTES`] of address space.
///
/// More threads were measured on a 16-thread machine: 8 loaded a world no faster than 4 (the
/// ranges overlapped) and lengthened the worst frame while loading (27.5-33.6 ms against
/// 14.7-20.4 ms, both arming 4 models a frame), so the automatic size stays as it was.
fn io_pool_threads(requested: usize, available: usize) -> Result<usize> {
    if requested > MAX_IO_THREADS {
        let stack_mib = IO_TASK_STACK_BYTES / (1024 * 1024);
        color_eyre::eyre::bail!(
            "io_threads is {requested}, but the asset IO pool takes at most {MAX_IO_THREADS} \
             threads (each reserves {stack_mib} MiB of address space); use 0 for the automatic size"
        );
    }
    Ok(if requested > 0 {
        requested
    } else {
        available.div_ceil(4).clamp(1, 4)
    })
}

fn configure_io_task_pool(requested: usize) -> Result<()> {
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let threads = io_pool_threads(requested, available)?;
    // The pool is process-global (a `OnceLock` in bevy_tasks) and cannot be resized: if a host
    // built it before `run`, that pool stays, so check it rather than claiming the requested size.
    // The pool `get_or_init` returns is the one in use, whoever built it, so that is the one
    // checked: reading it first with `try_get` would miss a host that builds it between the calls.
    let pool = IoTaskPool::get_or_init(|| io_task_pool_builder(threads).build());
    check_existing_io_pool(requested, threads, Some(pool.thread_num()))
}

/// Compares the IO pool a host built before `run` with the size `run` wants. An explicit
/// `--io-threads` that the existing pool does not match is an error, because the pool cannot be
/// resized and the caller asked for that size; the automatic size (`requested` 0) is only a
/// default, so a host's own pool is kept with a warning.
fn check_existing_io_pool(requested: usize, threads: usize, existing: Option<usize>) -> Result<()> {
    let Some(existing) = existing.filter(|&existing| existing != threads) else {
        return Ok(());
    };
    if requested > 0 {
        color_eyre::eyre::bail!(
            "io_threads is {requested}, but the asset IO pool already exists with {existing} \
             threads and cannot be resized; let run build the pool rather than building it \
             first, or use 0 to keep the existing pool"
        );
    }
    // `run` checks the pool before installing LogPlugin, so report directly to stderr.
    eprintln!(
        "warning: the asset IO pool already exists with {existing} threads; \
         automatic sizing selected {threads}; keeping the existing pool"
    );
    Ok(())
}

/// The interior cell the streaming fixture loads by id. An interior carries no grid square and
/// belongs to no worldspace, and its id sits past the block the exterior cells are numbered in.
const STREAMING_FIXTURE_INTERIOR_CELL_ID: u32 = 0x0001_0000;
/// The reference placed inside that interior cell. It has no model, so the fixture still needs no
/// converted assets; the crossing is observed through the root and this reference.
const STREAMING_FIXTURE_INTERIOR_REFERENCE_ID: u32 = STREAMING_FIXTURE_INTERIOR_CELL_ID + 1;
/// The frame the fixture loads the interior on, after the first teleport has moved the camera
/// several cells away from the grid it starts on.
const STREAMING_FIXTURE_INTERIOR_FRAME: u32 = 8;

struct StreamingFixtureDirectory {
    path: PathBuf,
}

impl StreamingFixtureDirectory {
    fn create(worldspace_id: u32, start_grid: (i32, i32)) -> Result<Self> {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path =
            std::env::temp_dir().join(format!("mudcrab-streaming-{}-{suffix}", std::process::id()));
        fs::create_dir(&path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        let fixture = Self { path };
        fixture.populate(worldspace_id, start_grid)?;
        Ok(fixture)
    }

    fn populate(&self, worldspace_id: u32, start_grid: (i32, i32)) -> Result<()> {
        let database_path = self.path.join("skyrim_world.db");
        let connection = Connection::open(&database_path)?;
        connection.execute("CREATE TABLE schema_info(version INTEGER NOT NULL)", [])?;
        connection.execute(
            "INSERT INTO schema_info VALUES(?1)",
            [shared::WORLD_DATABASE_SCHEMA_VERSION],
        )?;
        connection.execute_batch(
            r#"CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT);
            CREATE TABLE worldspaces(id INTEGER PRIMARY KEY,editor_id TEXT,parent_world INTEGER,flags INTEGER,lod_origin_x INTEGER,lod_origin_y INTEGER);
            CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
            CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL,flags INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
            CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
            CREATE TABLE lod_build(id INTEGER PRIMARY KEY CHECK (id=1),build_identity TEXT NOT NULL);
            INSERT INTO lod_build VALUES(1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
            CREATE TABLE lod_chunks(worldspace_id INTEGER,tier INTEGER,anchor_x INTEGER,anchor_y INTEGER,payload_path TEXT,content_hash TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,source_cells TEXT,PRIMARY KEY(worldspace_id,tier,anchor_x,anchor_y));
            CREATE VIRTUAL TABLE lod_chunks_spatial USING rtree(id,minX,maxX,minY,maxY,+worldspace_id,+tier,+anchor_x,+anchor_y);
            CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
            CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
            CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);"#,
        )?;
        connection.execute(
            "INSERT INTO worldspaces(id,editor_id,parent_world,flags,lod_origin_x,lod_origin_y) VALUES(?1,'Fixture',NULL,0,?2,?3)",
            params![worldspace_id, start_grid.0, start_grid.1],
        )?;
        let mut insert = connection
            .prepare("INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(?1,?2,?3,?4)")?;
        let mut cell_id = 1u32;
        for grid_y in start_grid.1.saturating_sub(48)..=start_grid.1.saturating_add(48) {
            for grid_x in start_grid.0.saturating_sub(48)..=start_grid.0.saturating_add(48) {
                insert.execute(params![cell_id, worldspace_id, grid_x, grid_y])?;
                cell_id += 1;
            }
        }
        drop(insert);
        connection.execute(
            "INSERT INTO cells(id,worldspace_id,grid_x,grid_y,interior_name) VALUES(?1,NULL,NULL,NULL,'Fixture Hall')",
            params![STREAMING_FIXTURE_INTERIOR_CELL_ID],
        )?;
        connection.execute(
            "INSERT INTO \"references\"(id,cell_id,base_form_id,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,scale)
             VALUES(?1,?2,?3,256.0,0.0,192.0,0.0,0.0,0.0,1.0)",
            params![
                STREAMING_FIXTURE_INTERIOR_REFERENCE_ID,
                STREAMING_FIXTURE_INTERIOR_CELL_ID,
                STREAMING_FIXTURE_INTERIOR_REFERENCE_ID
            ],
        )?;
        drop(connection);
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: Vec::new(),
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache)
            .wrap_err("failed to archive streaming fixture cache")?;
        fs::write(self.path.join("cell_cache.rkyv"), bytes)?;
        Ok(())
    }
}

impl Drop for StreamingFixtureDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove streaming fixture directory");
        }
    }
}

#[derive(Resource, Default)]
struct StreamingFixtureState {
    frames: u32,
    total_x: i32,
    total_y: i32,
    finished: bool,
    interior: InteriorCrossing,
    /// The exterior grid the camera stood on when the interior was loaded: the place it has to
    /// leave for the interior to be observed from far outside the unload radius.
    interior_center: IVec2,
}

/// What the fixture observed of the exterior/interior crossing. The interior is loaded by id the
/// way a door crossing will load one; the camera then carries on over exteriors far outside the
/// unload radius and comes back. The contract is that the interior never exists twice and its
/// references match its root. Today it also stays loaded throughout (an interior has no grid
/// square, so [`cell_within_unload_radius`](crate::streaming) keeps it, and only a space switch
/// unloads one); that is current behaviour, not part of the contract.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct InteriorCrossing {
    requested_frame: Option<u32>,
    resident_frame: Option<u32>,
    /// Frames on which the camera stood more than `unload_radius` cells from the grid the interior
    /// was loaded from, and the interior root counts seen on them.
    away_samples: u32,
    min_away_roots: usize,
    max_away_roots: usize,
}

impl InteriorCrossing {
    fn observe(&mut self, away: bool, roots: usize) {
        if !away {
            return;
        }
        self.away_samples = self.away_samples.saturating_add(1);
        if self.away_samples == 1 {
            self.min_away_roots = roots;
            self.max_away_roots = roots;
        } else {
            self.min_away_roots = self.min_away_roots.min(roots);
            self.max_away_roots = self.max_away_roots.max(roots);
        }
    }
}

/// The crossing contract, as the fixture's own observations and the final root and reference
/// counts express it: the interior was loaded, the camera was observed far away from it, it never
/// had two roots, and its references match its root (one each, or none if it was unloaded).
/// Orphaned and missing roots are the lifecycle validator's, which the run also requires at zero.
fn interior_crossing_valid(crossing: &InteriorCrossing, roots: usize, references: usize) -> bool {
    crossing.requested_frame.is_some()
        && crossing.resident_frame.is_some()
        && crossing.away_samples > 0
        && crossing.max_away_roots <= 1
        && roots <= 1
        && references == roots
}

fn setup_streaming_fixture_visual(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
) {
    let mesh = Mesh3d(meshes.add(Cuboid::new(180.0, 480.0, 180.0)));
    let material = MeshMaterial3d(materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.22, 0.48, 0.18),
            perceptual_roughness: 0.88,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    commands.spawn_batch((0..64).map(move |index| {
        let x = index % 8;
        let z = index / 8;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(700.0 + x as f32 * 360.0, 240.0, -700.0 - z as f32 * 360.0),
        )
    }));
}

fn drive_streaming_fixture(
    mut state: ResMut<StreamingFixtureState>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let Some((x, y, label)) = (match state.frames {
        4 => Some((6, 0, "rapid_traversal")),
        5 => Some((0, -7, "rapid_traversal")),
        6 => Some((18, 12, "teleport")),
        14 => Some((-30, -9, "teleport")),
        22 => Some((9, 5, "rapid_traversal")),
        30 => Some((-state.total_x, -state.total_y, "return_to_origin")),
        _ => None,
    }) else {
        return;
    };
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    camera.translation.x += x as f32 * crate::world::components::CELL_SIZE;
    camera.translation.z -= y as f32 * crate::world::components::CELL_SIZE;
    state.total_x += x;
    state.total_y += y;
    profiler.event("streaming-fixture", label, None);
}

/// Loads the fixture's interior by id, through the loader path the camera planner uses, and
/// watches it while the camera keeps crossing exteriors around it.
#[allow(clippy::too_many_arguments)]
fn cross_streaming_fixture_interior(
    config: Res<EngineConfig>,
    mut state: ResMut<StreamingFixtureState>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    mut streaming: ResMut<StreamingWorld>,
    mut metrics: ResMut<StreamingMetrics>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<&CellRef, With<StreamedCellRoot>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    let Ok(camera) = camera.single() else {
        return;
    };
    let center = streaming_center(camera.translation, origin.0);
    if state.interior.requested_frame.is_none() {
        if state.frames < STREAMING_FIXTURE_INTERIOR_FRAME {
            return;
        }
        if !streaming.request_cell(
            &database,
            CellKey::Interior(STREAMING_FIXTURE_INTERIOR_CELL_ID),
            &mut metrics,
            &mut profiler,
        ) {
            return;
        }
        state.interior.requested_frame = Some(state.frames);
        state.interior_center = center;
        profiler.event("streaming-fixture", "interior_requested", None);
        return;
    }
    let roots = roots
        .iter()
        .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
        .count();
    // Only a loaded interior can be unloaded against the rule: the frames between the request and
    // the commit have no root to count yet.
    if state.interior.resident_frame.is_none() {
        if roots > 0 {
            state.interior.resident_frame = Some(state.frames);
            profiler.event("streaming-fixture", "interior_resident", None);
        }
        return;
    }
    let away = (center.x - state.interior_center.x).abs() > config.unload_radius
        || (center.y - state.interior_center.y).abs() > config.unload_radius;
    state.interior.observe(away, roots);
}

fn validate_streaming_fixture(
    config: Res<EngineConfig>,
    mut state: ResMut<StreamingFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    roots: Query<&CellRef, With<StreamedCellRoot>>,
    references: Query<&FormId>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || state.frames < 90 {
        return;
    }
    let expected_resident = ((config.stream_radius * 2 + 1).max(0) as usize).pow(2);
    let maximum_resident = ((config.unload_radius * 2 + 1).max(0) as usize).pow(2);
    let interior_roots = roots
        .iter()
        .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
        .count();
    let interior_references = references
        .iter()
        .filter(|form_id| form_id.0 == STREAMING_FIXTURE_INTERIOR_REFERENCE_ID)
        .count();
    let settled = metrics.active_requests == 0 && metrics.loading_cells == 0;
    let valid = settled
        && metrics.requests_submitted > expected_resident as u64
        && metrics.responses_received > 0
        && metrics.stale_responses > 0
        && metrics.unloaded_cells > 0
        && metrics.origin_rebases >= 6
        && metrics.resident_cells >= expected_resident
        && metrics.resident_cells <= maximum_resident
        && metrics.resident_roots == metrics.resident_cells
        && metrics.out_of_range_cell_roots == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.commit_frames > 0
        && interior_crossing_valid(&state.interior, interior_roots, interior_references);
    if valid {
        metrics.streaming_fixture_validated = true;
        profiler.event("streaming-fixture", "validated", None);
        state.finished = true;
    } else if state.frames >= 300 {
        metrics.streaming_fixture_failures = metrics.streaming_fixture_failures.saturating_add(1);
        error!(
            ?metrics,
            "streaming fixture did not settle or violated its lifecycle contract"
        );
        profiler.event("streaming-fixture", "failed", None);
        state.finished = true;
    }
}

#[derive(Component, Debug, Clone, Copy)]
enum CanonicalMaterialKind {
    Opaque,
    Cutout,
    Blend,
    Emissive,
    DoubleSided,
    NormalMapped,
}

#[derive(Resource, Default)]
struct CanonicalMaterialFixtureState {
    finished: bool,
}

fn fixture_image(data: Vec<u8>, srgb: bool) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        if srgb {
            TextureFormat::Rgba8UnormSrgb
        } else {
            TextureFormat::Rgba8Unorm
        },
        RenderAssetUsages::default(),
    );
    image.sampler = bevy::image::ImageSampler::linear();
    image
}

/// A terrain layer image of the synthetic fixture: one flat colour, carried by a sampler that
/// repeats like the one a streamed layer gets, since the shader tiles every layer across a
/// cell. Bevy's default sampler clamps to the edge, which stretches the outermost texels over the
/// rest of the tiles.
fn terrain_fixture_image(pixel: [u8; 4]) -> Image {
    let mut image = fixture_image((0..16).flat_map(|_| pixel).collect(), true);
    image.sampler = bevy::image::ImageSampler::Descriptor(terrain_layer_sampler());
    image
}

fn setup_material_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<CanonicalMaterialFixtureState>();
    let checker = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                let alpha = if (index + index / 4) % 2 == 0 { 255 } else { 0 };
                [78, 166, 88, alpha]
            })
            .collect(),
        true,
    ));
    let normal = images.add(fixture_image(
        (0..16).flat_map(|_| [128, 128, 255, 255]).collect(),
        false,
    ));
    let definitions = [
        (
            CanonicalMaterialKind::Opaque,
            StandardMaterial {
                base_color: Color::srgb(0.55, 0.42, 0.25),
                perceptual_roughness: 0.75,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Cutout,
            StandardMaterial {
                base_color_texture: Some(checker),
                alpha_mode: AlphaMode::Mask(0.5),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Blend,
            StandardMaterial {
                base_color: Color::srgba(0.15, 0.45, 0.9, 0.45),
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Emissive,
            StandardMaterial {
                base_color: Color::srgb(0.08, 0.08, 0.08),
                emissive: LinearRgba::new(6.0, 1.2, 0.15, 1.0),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::DoubleSided,
            StandardMaterial {
                base_color: Color::srgb(0.75, 0.2, 0.18),
                double_sided: true,
                cull_mode: None,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::NormalMapped,
            StandardMaterial {
                base_color: Color::srgb(0.45, 0.48, 0.52),
                normal_map_texture: Some(normal),
                ..default()
            },
        ),
    ];
    let mesh = meshes.add(Cuboid::new(2.2, 2.2, 2.2));
    for (index, (kind, material)) in definitions.into_iter().enumerate() {
        commands.spawn((
            Name::new(format!("Canonical {kind:?}")),
            kind,
            Mesh3d(mesh.clone()),
            MeshMaterial3d(materials.add(material)),
            Transform::from_xyz((index as f32 - 2.5) * 2.8, 0.0, 0.0),
        ));
    }
    commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Transform::from_xyz(0.0, 5.0, 18.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 120.0,
        ..default()
    });
}

fn validate_material_fixture(
    query: Query<(&CanonicalMaterialKind, &MeshMaterial3d<StandardMaterial>)>,
    materials: Res<Assets<StandardMaterial>>,
    images: Res<Assets<Image>>,
    mut state: ResMut<CanonicalMaterialFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || query.iter().count() != 6 {
        return;
    }
    let mut validated_images = 0usize;
    for (kind, handle) in &query {
        let result = materials
            .get(handle)
            .ok_or_else(|| "material is not loaded".to_owned())
            .and_then(|material| {
                match kind {
                    CanonicalMaterialKind::Opaque if material.alpha_mode != AlphaMode::Opaque => {
                        Err("opaque mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Cutout
                        if !matches!(material.alpha_mode, AlphaMode::Mask(_)) =>
                    {
                        Err("mask mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Blend if material.alpha_mode != AlphaMode::Blend => {
                        Err("blend mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Emissive if material.emissive.red <= 0.0 => {
                        Err("emissive intensity was lost".to_owned())
                    }
                    CanonicalMaterialKind::DoubleSided
                        if !material.double_sided || material.cull_mode.is_some() =>
                    {
                        Err("double-sided culling was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::NormalMapped
                        if material.normal_map_texture.is_none() =>
                    {
                        Err("normal map was not preserved".to_owned())
                    }
                    _ => Ok(()),
                }?;
                validate_standard_material(material, &images)
            });
        match result {
            Ok(count) => validated_images += count,
            Err(reason) => {
                metrics.asset_load_failures += 1;
                metrics.material_validation_failures += 1;
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!("canonical-material-fixture/{kind:?}"),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: 0,
                    dependency_chain: vec![reason],
                });
                profiler.increment("assets/load_failures", 1);
            }
        }
    }
    metrics.materials_validated += 6;
    metrics.images_validated += validated_images as u64;
    metrics.canonical_fixture_validated = metrics.material_validation_failures == 0;
    state.finished = true;
}

#[derive(Component)]
struct TerrainWaterFixtureTerrain;

#[derive(Component)]
struct TerrainWaterFixtureWater;

#[derive(Resource, Default)]
struct TerrainWaterFixtureState {
    finished: bool,
}

/// The synthetic LAND snapshot the terrain/water fixture draws: a rolling height field whose four
/// quadrants each carry a full base-and-five-overlays stack, every overlay strongest around its own
/// centre so the weight field is visible in the scene. Built without game data, like every fixture.
fn terrain_water_fixture_snapshot() -> TerrainSnapshot {
    let mut layers = Vec::new();
    for quadrant in 0..4 {
        layers.push(TerrainLayerSnapshot {
            texture_form_id: 1,
            quadrant,
            layer: 0,
            is_base: true,
            weights: Vec::new(),
        });
        for layer in 1..=5u16 {
            let weights = (0usize..17 * 17)
                .filter_map(|vertex| {
                    let x = vertex % 17;
                    let y = vertex / 17;
                    let center = (layer as usize * 3).min(16);
                    let distance = x.abs_diff(center).min(y.abs_diff(center));
                    (distance < 3).then(|| (vertex as u16, (3 - distance) as f32 * 0.12))
                })
                .collect();
            layers.push(TerrainLayerSnapshot {
                texture_form_id: u32::from(layer) + 1,
                quadrant,
                layer,
                is_base: false,
                weights,
            });
        }
    }
    TerrainSnapshot {
        cell_id: 0xF170_0001,
        width: 33,
        height: 33,
        heights: (0..33 * 33)
            .map(|index| {
                let x = (index % 33) as f32 - 16.0;
                let y = (index / 33) as f32 - 16.0;
                45.0 * (x * 0.22).sin() + 35.0 * (y * 0.18).cos()
            })
            .collect(),
        normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
        vertex_colors: (0..33 * 33)
            .flat_map(|index| {
                let shade = 190 + (index % 33) as u8;
                [shade, shade, shade]
            })
            .collect(),
        layers,
        water_height: Some(12.0),
        water_type_form_id: Some(1),
    }
}

fn setup_terrain_water_fixture(
    mut commands: Commands,
    config: Res<EngineConfig>,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<TerrainWaterFixtureState>();
    let palette = [
        [82, 116, 58, 255],
        [122, 101, 70, 255],
        [83, 92, 102, 255],
        [146, 138, 103, 255],
        [60, 91, 54, 255],
        [113, 82, 62, 255],
    ];
    let texture_handles: [Handle<Image>; 6] =
        palette.map(|pixel| images.add(terrain_fixture_image(pixel)));
    let flow_normal = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                if index % 2 == 0 {
                    [150, 110, 255, 255]
                } else {
                    [110, 150, 255, 255]
                }
            })
            .collect(),
        false,
    ));
    let terrain = terrain_water_fixture_snapshot();
    for quadrant in 0..4 {
        commands.spawn((
            Name::new(format!("Terrain/water fixture quadrant {quadrant}")),
            Mesh3d(
                meshes.add(
                    build_terrain_quadrant_mesh(&terrain, quadrant)
                        .expect("canonical terrain fixture must build"),
                ),
            ),
            MeshMaterial3d(
                terrain_materials.add(TerrainMaterial {
                    base: StandardMaterial {
                        base_color: Color::WHITE,
                        perceptual_roughness: 0.92,
                        cull_mode: None,
                        double_sided: true,
                        ..default()
                    },
                    extension: TerrainExtension::fixture(
                        &terrain,
                        quadrant,
                        texture_handles.clone(),
                    )
                    .expect("canonical terrain fixture must build"),
                }),
            ),
            TerrainWaterFixtureTerrain,
        ));
    }
    commands.spawn((
        Name::new("Terrain/water fixture water"),
        Mesh3d(meshes.add(Plane3d::default().mesh().size(2200.0, 2200.0))),
        MeshMaterial3d(water_materials.add(WaterMaterial {
            base: StandardMaterial {
                base_color: Color::srgba(0.04, 0.2, 0.32, 0.7),
                metallic: 0.15,
                perceptual_roughness: 0.06,
                reflectance: 0.9,
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
            extension: WaterExtension::with_reflection(reflection.0.clone(), Some(flow_normal)),
        })),
        Transform::from_xyz(CELL_SIZE_HALF, 12.0, -CELL_SIZE_HALF),
        crate::world::components::WaterSurface,
        TerrainWaterFixtureWater,
        RenderLayers::layer(WATER_LAYER),
    ));
    let target = Vec3::new(CELL_SIZE_HALF, 0.0, -CELL_SIZE_HALF);
    commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Transform::from_xyz(CELL_SIZE_HALF, 1800.0, 2600.0).looking_at(target, Vec3::Y),
        StreamingCamera,
        FogCamera,
        config.terrain_water_fixture_msaa,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(MAIN_VIEW_LAYERS),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        brightness: 160.0,
        ..default()
    });
}

fn validate_terrain_water_fixture(
    terrain: Query<(&Mesh3d, &MeshMaterial3d<TerrainMaterial>), With<TerrainWaterFixtureTerrain>>,
    water: Query<&MeshMaterial3d<WaterMaterial>, With<TerrainWaterFixtureWater>>,
    meshes: Res<Assets<Mesh>>,
    terrain_materials: Res<Assets<TerrainMaterial>>,
    water_materials: Res<Assets<WaterMaterial>>,
    mut state: ResMut<TerrainWaterFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
) {
    if state.finished || terrain.iter().count() != 4 || water.iter().count() != 1 {
        return;
    }
    // The fixture exists to show terrain without game data, so it is only valid if its materials
    // render the overlay weight field the streamed path uses - the point of the scene.
    let valid_terrain = terrain.iter().all(|(mesh, handle)| {
        let Some(material) = terrain_materials.get(handle) else {
            return false;
        };
        meshes.get(mesh).is_some() && material.extension.reads_weight_field()
    });
    let valid_water = water
        .single()
        .ok()
        .and_then(|material| water_materials.get(material))
        .is_some();
    if valid_terrain && valid_water {
        metrics.terrain_patches_validated += 4;
        metrics.water_surfaces_validated += 1;
        metrics.materials_validated += 5;
        metrics.images_validated += 7;
        metrics.terrain_water_fixture_validated = true;
    } else {
        metrics.terrain_validation_failures += (!valid_terrain) as u64;
        metrics.water_validation_failures += (!valid_water) as u64;
    }
    state.finished = true;
}

#[derive(Component)]
struct TransformBoundsFixtureRoot;

#[derive(Resource, Default)]
struct TransformBoundsFixtureState {
    frames: u8,
    finished: bool,
}

fn setup_transform_bounds_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<TransformBoundsFixtureState>();
    let beam_mesh = meshes.add(Cuboid::new(2.0, 4.0, 1.5));
    let cube_mesh = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let cap_mesh = meshes.add(Cuboid::new(6.5, 0.7, 1.2));
    let stone = materials.add(StandardMaterial {
        base_color: Color::srgb(0.38, 0.46, 0.58),
        perceptual_roughness: 0.72,
        ..default()
    });
    let bronze = materials.add(StandardMaterial {
        base_color: Color::srgb(0.72, 0.39, 0.12),
        metallic: 0.45,
        perceptual_roughness: 0.42,
        ..default()
    });
    let moss = materials.add(StandardMaterial {
        base_color: Color::srgb(0.22, 0.48, 0.24),
        perceptual_roughness: 0.86,
        ..default()
    });

    let left = Transform::from_xyz(-2.5, 0.0, 0.0)
        .with_rotation(Quat::from_rotation_z(0.28))
        .with_scale(Vec3::new(1.0, 1.35, 0.75));
    let group = Transform::from_xyz(2.0, 0.5, 0.0)
        .with_rotation(Quat::from_rotation_y(-0.42))
        .with_scale(Vec3::new(0.8, 1.3, 0.65));
    let nested = Transform::from_xyz(1.0, 1.0, 0.0)
        .with_rotation(Quat::from_rotation_x(0.31))
        .with_scale(Vec3::new(1.2, 0.5, 1.7));
    let cap = Transform::from_xyz(0.0, 3.8, 0.0)
        .with_rotation(Quat::from_euler(EulerRot::YXZ, 0.18, -0.12, 0.08))
        .with_scale(Vec3::new(1.05, 0.8, 1.25));

    let mut expected_min = Vec3::splat(f32::INFINITY);
    let mut expected_max = Vec3::splat(f32::NEG_INFINITY);
    for bounds in [
        InstanceBounds::transformed(
            Vec3::new(-1.0, -2.0, -0.75),
            Vec3::new(1.0, 2.0, 0.75),
            left.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::splat(-1.0),
            Vec3::splat(1.0),
            group.to_matrix() * nested.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::new(-3.25, -0.35, -0.6),
            Vec3::new(3.25, 0.35, 0.6),
            cap.to_matrix(),
        ),
    ] {
        expected_min = expected_min.min(bounds.min);
        expected_max = expected_max.max(bounds.max);
    }

    commands
        .spawn((
            Name::new("Canonical transform/bounds assembly"),
            TransformBoundsFixtureRoot,
            ExpectedModelBounds {
                min: expected_min,
                max: expected_max,
            },
            Transform::from_xyz(0.0, -1.0, 0.0)
                .with_rotation(Quat::from_rotation_y(0.48))
                .with_scale(Vec3::new(1.1, 0.9, 1.2)),
            Visibility::default(),
        ))
        .with_children(|parent| {
            parent.spawn((
                Name::new("Rotated left support"),
                Mesh3d(beam_mesh),
                MeshMaterial3d(stone),
                left,
            ));
            parent
                .spawn((
                    Name::new("Non-uniform hierarchy pivot"),
                    group,
                    Visibility::default(),
                ))
                .with_child((
                    Name::new("Nested rotated support"),
                    Mesh3d(cube_mesh),
                    MeshMaterial3d(bronze),
                    nested,
                ));
            parent.spawn((
                Name::new("Rotated top cap"),
                Mesh3d(cap_mesh),
                MeshMaterial3d(moss),
                cap,
            ));
        });
    commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Transform::from_xyz(2.0, 5.5, 16.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.55, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 140.0,
        ..default()
    });
}

#[allow(clippy::too_many_arguments)]
fn validate_transform_bounds_fixture(
    roots: Query<
        (Entity, &ExpectedModelBounds, &GlobalTransform),
        With<TransformBoundsFixtureRoot>,
    >,
    children: Query<&Children>,
    nodes: Query<(&Transform, &GlobalTransform, Option<&Mesh3d>)>,
    meshes: Res<Assets<Mesh>>,
    mut state: ResMut<TransformBoundsFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    if state.frames < 3 {
        return;
    }
    let result = (|| -> Result<(usize, usize), String> {
        let (root, expected, root_global) = roots
            .single()
            .map_err(|_| "canonical transform fixture root is missing".to_owned())?;
        let root_inverse = root_global.affine().inverse();
        let mut actual_min = Vec3::splat(f32::INFINITY);
        let mut actual_max = Vec3::splat(f32::NEG_INFINITY);
        let mut node_count = 0usize;
        let mut mesh_count = 0usize;
        for descendant in children.iter_descendants(root) {
            let (local, global, mesh) = nodes
                .get(descendant)
                .map_err(|_| format!("fixture node {descendant:?} has no transform"))?;
            if !local.to_matrix().is_finite()
                || !global.to_matrix().is_finite()
                || local.scale.abs().min_element() <= 1.0e-6
            {
                return Err(format!(
                    "fixture node {descendant:?} has an invalid transform"
                ));
            }
            node_count += 1;
            let Some(mesh) = mesh else { continue };
            let aabb = meshes
                .get(mesh)
                .and_then(MeshAabb::compute_aabb)
                .ok_or_else(|| format!("fixture mesh {:?} has no bounds", mesh.id()))?;
            let center = Vec3::from(aabb.center);
            let half = Vec3::from(aabb.half_extents);
            let bounds = InstanceBounds::transformed(
                center - half,
                center + half,
                Mat4::from(root_inverse * global.affine()),
            );
            actual_min = actual_min.min(bounds.min);
            actual_max = actual_max.max(bounds.max);
            mesh_count += 1;
        }
        let error = (actual_min - expected.min)
            .abs()
            .max((actual_max - expected.max).abs())
            .max_element();
        (mesh_count == 3 && error <= 1.0e-4)
            .then_some((node_count, mesh_count))
            .ok_or_else(|| {
                format!(
                    "hierarchy bounds mismatch: expected {:?}..{:?}, actual {:?}..{:?}",
                    expected.min, expected.max, actual_min, actual_max
                )
            })
    })();
    match result {
        Ok((nodes, meshes)) => {
            metrics.transform_instances_validated += 1;
            metrics.transform_nodes_validated += nodes as u64;
            metrics.bounds_validated += meshes as u64;
            metrics.transform_bounds_fixture_validated = true;
            profiler.increment("transforms/fixture_validated", 1);
        }
        Err(reason) => {
            metrics.asset_load_failures += 1;
            metrics.transform_bounds_validation_failures += 1;
            metrics.asset_failures.push(AssetFailure {
                model_path: "fixtures/transform-bounds-assembly".to_owned(),
                reference_form_id: 0,
                base_form_id: 0,
                cell_id: 0,
                dependency_chain: vec![reason],
            });
            profiler.increment("transforms/validation_failures", 1);
        }
    }
    state.finished = true;
}

#[derive(Component)]
struct RendererFixtureCenterVisible;

#[derive(Component)]
struct RendererFixtureRightVisible;

#[derive(Component)]
struct RendererFixtureLeftVisible;

#[derive(Resource, Default)]
struct RendererFixtureState {
    frames: u16,
    phase_started: u16,
    phase: u8,
    center_seen: bool,
    right_seen: bool,
    left_seen: bool,
    finished: bool,
}

fn setup_renderer_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<RendererFixtureState>();
    let cube = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let wall = meshes.add(Cuboid::new(12.0, 10.0, 1.0));
    let opaque = materials.add(StandardMaterial {
        base_color: Color::srgb(0.28, 0.3, 0.34),
        perceptual_roughness: 0.9,
        ..default()
    });
    let green = materials.add(StandardMaterial {
        base_color: Color::srgb(0.12, 0.8, 0.2),
        ..default()
    });
    let red = materials.add(StandardMaterial {
        base_color: Color::srgb(0.85, 0.08, 0.05),
        ..default()
    });
    let blue = materials.add(StandardMaterial {
        base_color: Color::srgb(0.08, 0.35, 0.9),
        ..default()
    });
    let gold = materials.add(StandardMaterial {
        base_color: Color::srgb(0.9, 0.55, 0.08),
        metallic: 0.25,
        ..default()
    });
    commands.spawn((
        Name::new("Renderer fixture occluder"),
        Mesh3d(wall),
        MeshMaterial3d(opaque),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture front visible"),
        RendererFixtureCenterVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(green),
        Transform::from_xyz(0.0, 0.0, 5.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture fully occluded"),
        Mesh3d(cube.clone()),
        MeshMaterial3d(red),
        Transform::from_xyz(0.0, 0.0, -4.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after right turn"),
        RendererFixtureRightVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(blue),
        Transform::from_xyz(10.0, 0.0, -2.0)
            .with_rotation(Quat::from_rotation_y(0.45))
            .with_scale(Vec3::new(1.8, 0.7, 1.2)),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after left turn"),
        RendererFixtureLeftVisible,
        Mesh3d(cube),
        MeshMaterial3d(gold),
        Transform::from_xyz(-10.0, 0.0, -2.0)
            .with_rotation(Quat::from_euler(EulerRot::XYZ, 0.25, -0.5, 0.18))
            .with_scale(Vec3::new(0.65, 2.1, 1.4)),
    ));
    commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.65, -0.45, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 130.0,
        ..default()
    });
}

fn validate_renderer_fixture(
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    center: Query<&ViewVisibility, With<RendererFixtureCenterVisible>>,
    right: Query<&ViewVisibility, With<RendererFixtureRightVisible>>,
    left: Query<&ViewVisibility, With<RendererFixtureLeftVisible>>,
    mut state: ResMut<RendererFixtureState>,
    mut renderer: ResMut<RendererMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let phase_frames = state.frames.saturating_sub(state.phase_started);
    let center_visible = center.single().is_ok_and(|visibility| visibility.get());
    let right_visible = right.single().is_ok_and(|visibility| visibility.get());
    let left_visible = left.single().is_ok_and(|visibility| visibility.get());
    match state.phase {
        0 if phase_frames >= 10 && renderer.final_path_active() && center_visible => {
            state.center_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 1;
            state.phase_started = state.frames;
        }
        1 if phase_frames >= 8 && right_visible => {
            state.right_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(-10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 2;
            state.phase_started = state.frames;
        }
        2 if phase_frames >= 8 && left_visible => {
            state.left_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y);
            }
            state.phase = 3;
            state.phase_started = state.frames;
        }
        3 if phase_frames >= 8 && center_visible && renderer.final_path_active() => {
            renderer.renderer_fixture_validated =
                state.center_seen && state.right_seen && state.left_seen;
            renderer.renderer_validation_failures += (!renderer.renderer_fixture_validated) as u64;
            profiler.increment("renderer/fixture_validated", 1);
            state.finished = true;
        }
        _ if state.frames >= 180 => {
            renderer.renderer_validation_failures =
                renderer.renderer_validation_failures.saturating_add(1);
            profiler.increment("renderer/validation_failures", 1);
            state.finished = true;
        }
        _ => {}
    }
}

#[derive(Deserialize)]
struct RuntimeManifest {
    schema_version: u32,
    complete: bool,
}

#[derive(Deserialize)]
struct RuntimeIntegrationReport {
    schema_version: u32,
    passed: bool,
}

fn validate_runtime_assets(config: &EngineConfig) -> Result<()> {
    for required in ["skyrim_world.db", "cell_cache.rkyv"] {
        color_eyre::eyre::ensure!(
            config.assets_dir.join(required).is_file(),
            "converted asset set is missing {required}: {}",
            config.assets_dir.display()
        );
    }
    if config.allow_incomplete_assets {
        crate::world::database::validate_lod_build_contract(
            &config.assets_dir,
            converter_schema_version(),
        )?;
        return Ok(());
    }
    let manifest_path = config.assets_dir.join("conversion-manifest.json");
    let manifest: RuntimeManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .wrap_err("invalid conversion manifest")?;
    if !(shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION..=converter_schema_version())
        .contains(&manifest.schema_version)
    {
        let rejection = if manifest.schema_version < shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION {
            AssetSetRejection::ConverterSchemaOlder {
                found: manifest.schema_version,
            }
        } else {
            AssetSetRejection::ConverterSchemaNewer {
                found: manifest.schema_version,
            }
        };
        color_eyre::eyre::bail!(
            "{}",
            asset_set_rejection_message(&config.assets_dir, rejection)
        );
    }
    color_eyre::eyre::ensure!(
        manifest.complete,
        "{}",
        asset_set_rejection_message(
            &config.assets_dir,
            AssetSetRejection::IncompleteConversion {
                schema: manifest.schema_version
            }
        )
    );
    let report_path = config.assets_dir.join("integration-report.json");
    let report: RuntimeIntegrationReport = serde_json::from_slice(
        &std::fs::read(&report_path)
            .wrap_err_with(|| format!("failed to read {}", report_path.display()))?,
    )
    .wrap_err("invalid integration report")?;
    if !shared::supports_runtime_world_database_schema(report.schema_version) {
        color_eyre::eyre::bail!(
            "{}",
            asset_set_rejection_message(
                &config.assets_dir,
                AssetSetRejection::WorldDatabaseSchema {
                    found: report.schema_version,
                }
            )
        );
    }
    color_eyre::eyre::ensure!(
        report.passed,
        "{}",
        asset_set_rejection_message(
            &config.assets_dir,
            AssetSetRejection::IntegrationReportFailed
        )
    );
    let database_schema = crate::world::database::validate_lod_build_contract(
        &config.assets_dir,
        converter_schema_version(),
    )?;
    color_eyre::eyre::ensure!(
        database_schema == report.schema_version,
        "world database schema {database_schema} does not match integration report schema {}",
        report.schema_version
    );
    Ok(())
}

/// Why the runtime refused a converted asset set.
///
/// Each situation needs a different fix, so each gets its own message instead
/// of one "incomplete or stale" error covering all of them.
#[derive(Clone, Copy, Debug)]
enum AssetSetRejection {
    /// The converter that wrote the set is older than every schema this engine accepts.
    ConverterSchemaOlder { found: u32 },
    /// The converter that wrote the set is newer than every schema this engine accepts.
    ConverterSchemaNewer { found: u32 },
    /// The converter stopped early or skipped inputs (`complete: false`); `schema` is the one the
    /// manifest names.
    IncompleteConversion { schema: u32 },
    /// The integration report names a world database schema outside the accepted range.
    WorldDatabaseSchema { found: u32 },
    /// The integration report ran and reported failures.
    IntegrationReportFailed,
}

/// Names the failed check, what it found, the range it accepts, and the
/// command that fixes it.
fn asset_set_rejection_message(assets_dir: &Path, rejection: AssetSetRejection) -> String {
    let converter_min = shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
    let converter_max = converter_schema_version();
    let database_min = shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION;
    let database_max = shared::WORLD_DATABASE_SCHEMA_VERSION;
    let manifest = assets_dir.join("conversion-manifest.json");
    let report = assets_dir.join("integration-report.json");
    // The converter's usage string takes the Skyrim Data folder first and the
    // output directory second. The engine knows only the directory it was
    // given, so the Data folder stays a placeholder.
    let reconvert = format!(
        "cargo run --release -p converter -- <Skyrim Data folder> \"{}\"",
        assets_dir.display()
    );
    match rejection {
        AssetSetRejection::ConverterSchemaOlder { found } => format!(
            "converted assets are stale: {} was written by converter schema {found}, but this \
             engine accepts converter schemas {converter_min} through {converter_max}; reconvert \
             with `{reconvert}` (the converter reuses what it can from the previous conversion)",
            manifest.display()
        ),
        AssetSetRejection::ConverterSchemaNewer { found } => format!(
            "converted assets are newer than this engine: {} was written by converter schema \
             {found}, but this engine understands only converter schemas {converter_min} through \
             {converter_max}; update the engine and rebuild it (`cargo build --release -p \
             engine`), or reconvert with a converter at schema {converter_max}",
            manifest.display()
        ),
        AssetSetRejection::IncompleteConversion { schema } => format!(
            "the asset conversion did not finish: {} reports complete=false at converter schema \
             {schema}, so the converter stopped early or skipped inputs; rerun `{reconvert}` (it \
             reuses unchanged work), or start the engine with --allow-incomplete-assets to use \
             what is there",
            manifest.display()
        ),
        AssetSetRejection::WorldDatabaseSchema { found } => format!(
            "the converted assets use an unsupported world database schema: {} reports world \
             database schema {found} is unsupported; this engine accepts world database schemas \
             {database_min} through {database_max}; reconvert with a converter built from the \
             same revision as this engine: `{reconvert}`",
            report.display()
        ),
        AssetSetRejection::IntegrationReportFailed => format!(
            "the asset integration report did not pass: {} reports passed=false; read that report \
             for the failing check, then reconvert with `{reconvert}`",
            report.display()
        ),
    }
}

const fn converter_schema_version() -> u32 {
    // Shared with the producer without linking the converter into the runtime.
    shared::LOD_CONVERTER_SCHEMA_VERSION
}

fn setup_synthetic_benchmark(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let mesh = Mesh3d(meshes.add(Cuboid::new(18.0, 60.0, 18.0)));
    let material = MeshMaterial3d(terrain_materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.16, 0.36, 0.12),
            perceptual_roughness: 0.9,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    let side = (config.synthetic_instances as f64).sqrt().ceil() as usize;
    commands.spawn_batch((0..config.synthetic_instances).map(move |index| {
        let x = index % side;
        let z = index / side;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(x as f32 * 32.0, 30.0, -(z as f32 * 32.0)),
        )
    }));
    info!(
        instances = config.synthetic_instances,
        "synthetic indirect-render benchmark initialized"
    );
    profiler.increment("synthetic/instances", config.synthetic_instances as u64);
    profiler.record_elapsed("startup/synthetic_scene", started);
}

/// Creation units in a metre. A Creation unit is about 1.43 cm, and this engine renders one
/// Creation unit as one Bevy world unit, so a distance a renderer's default expresses in metres
/// is this many times larger here.
const CREATION_UNITS_PER_METRE: f32 = 70.0;

/// The number of cascades the sun's shadow map is split into. Four is Bevy's default and its
/// per-light maximum on desktop (`MAX_CASCADES_PER_LIGHT`, `bevy_pbr-0.19.0`
/// `src/render/light.rs:228`), and with [`sun_shadow_cascades`] reaching the far corner of the
/// drawn grid, fewer would mean larger cascades and coarser shadows everywhere the player looks.
const SUN_SHADOW_CASCADES: usize = 4;

/// The size of each of the sun's cascades, in texels a side. This is Bevy's own default, which
/// `DirectionalLightShadowMap` documents as having to be a power of two, and it is stated here
/// rather than left implicit so that the resolution the sun's shadows are drawn at is this
/// engine's decision instead of a Bevy default that can move under it.
const SUN_SHADOW_MAP_SIZE: usize = 2048;

/// The most cells, from the camera's cell to the far edge of the drawn grid, that
/// [`sun_shadow_cascades`] fits the sun's shadow range to. `--stream-radius` accepts any integer,
/// and a radius of 100000 would ask for a range of hundreds of millions of units, in which the
/// outermost cascade's texels are wider than the cells they are meant to shadow. The engine's own
/// worlds are a handful of cells across, so the cap is far past anything that streams at a usable
/// frame rate: it only keeps a nonsense radius from asking for gigametre cascades.
const SUN_SHADOW_MAX_GRID_CELLS: i32 = 256;

/// The sun's shadow cascades, fitted to a world whose unit is about 1.43 cm.
///
/// # Why the sun cast no shadows
///
/// `DirectionalLight::shadow_maps_enabled` is already true, but without a [`CascadeShadowConfig`]
/// of its own the sun gets Bevy's default, which is built for a metre-scale world: four cascades
/// with a first far bound of 10 and a maximum distance of 150, split geometrically into the far
/// bounds 10 / 24.7 / 60.8 / 150. Read in Creation units that is a shadow map spent on the two
/// metres of ground around the camera, with every house, tree and cell of the streamed world
/// outside the last cascade - the sun lights the scene and none of it falls in shadow.
///
/// # The distances
///
/// The first two are Bevy's defaults read in metres and converted at [`CREATION_UNITS_PER_METRE`]:
/// a first cascade far bound of 10 m (700 units), and a near clamp of 0.1 m (7 units) below which
/// no shadow is drawn, which is the same 10 cm Bevy's default works at. The overlap between
/// cascades is left at the builder's default; only the distances are fitted to this world.
///
/// [`maximum_distance`] starts as a property of the drawn world:
/// `streaming::plan_cells` requests `config.stream_radius` cells around the camera's cell and keeps
/// them until they leave `config.unload_radius`, which the command line sets one ring wider, so
/// cells - and the geometry they carry - keep drawing out to the unload ring rather than to the
/// requested radius. The camera stands somewhere inside its own cell, so that grid is at most
/// `unload_radius + 1` cells from it to the edge and [sqrt(2)] times that to the far corner, and
/// the range is the distance from the camera to that corner, the camera being [`camera_offset`]
/// above the point it looks at. `[Display] fShadowDistance` may cap that range to match Skyrim's
/// configured exterior shadow draw distance. Without that INI key it stops at the drawn grid:
/// nothing is drawn past the corner, so a wider range would only spend the same shadow map on
/// coarser cascades. Terrain standing above the ground plane at the corner is further from the
/// camera than the corner and is not accounted for.
/// [SUN_SHADOW_MAX_GRID_CELLS] caps the fit for radii the command line accepts but nothing could
/// stream. At the default `unload_radius` of 3 the last bound is about 23,200 units, a third of a
/// kilometre.
///
/// # The cost
///
/// This is the first configuration in which the shadow pass draws the streamed world rather than a
/// patch of ground in front of the camera: it draws the union of the cascades, which is the
/// camera's view out to `maximum_distance`, so it draws on the order of what the main pass draws
/// again. A frame rate that suffers is turned back up by, in order of how much they give,
/// `maximum_distance`, which is the geometry the pass draws at all, [`SUN_SHADOW_CASCADES`], which
/// is how many passes it is split over, and [`SUN_SHADOW_MAP_SIZE`], which is fill rate rather than
/// geometry. The biases are left at Bevy's defaults. Measure that cost rather than assume it: the
/// synthetic scenario (`scripts/phase2-profile.ps1 -Scenario synthetic`) runs this setup without
/// game data, and `docs/roadmap/02-profiling.md` holds the campaign and its regression thresholds.
///
/// [sqrt(2)]: std::f32::consts::SQRT_2
/// [`maximum_distance`]: CascadeShadowConfigBuilder::maximum_distance
pub(crate) fn sun_shadow_cascades(config: &EngineConfig) -> CascadeShadowConfig {
    // Cells draw out to `unload_radius`, one ring past the requested `stream_radius`, and the
    // `+ 1` is the camera's own cell: the grid is `unload_radius` cells around that cell rather
    // than around the camera, which may stand at the far edge of its own. The radius is clamped
    // at both ends: a negative one, which the command line allows, streams nothing and the range
    // derived from it would fall under the first cascade's far bound, which
    // `CascadeShadowConfigBuilder::build` rejects by panic, while one past
    // [SUN_SHADOW_MAX_GRID_CELLS] is more grid than the shadow map can usefully cover.
    let unload_cells = config.unload_radius.saturating_add(1);
    let cells = unload_cells.clamp(1, SUN_SHADOW_MAX_GRID_CELLS);
    let corner = crate::world::components::CELL_SIZE * cells as f32 * std::f32::consts::SQRT_2;
    let grid_distance = corner.hypot(camera_offset(config).y);
    let maximum_distance = config
        .sun_shadow_distance_limit
        .map_or(grid_distance, |limit| grid_distance.min(limit));
    CascadeShadowConfigBuilder {
        minimum_distance: 0.1 * CREATION_UNITS_PER_METRE,
        maximum_distance,
        first_cascade_far_bound: (10.0 * CREATION_UNITS_PER_METRE).min(maximum_distance * 0.5),
        num_cascades: SUN_SHADOW_CASCADES,
        ..default()
    }
    .into()
}

/// Rotation for a sun at the configured Creation bearing and elevation. Directional lights emit
/// along local -Z, so that vector points from the sun toward the ground, opposite its position.
fn sun_rotation(config: &EngineConfig) -> Quat {
    let Some((azimuth_degrees, elevation_degrees)) = config.sun_position else {
        return Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0);
    };
    let azimuth = azimuth_degrees.to_radians();
    let elevation = elevation_degrees.to_radians();
    let (sin_azimuth, cos_azimuth) = azimuth.sin_cos();
    let (sin_elevation, cos_elevation) = elevation.sin_cos();
    let sun_direction = Vec3::new(
        sin_azimuth * cos_elevation,
        sin_elevation,
        -cos_azimuth * cos_elevation,
    );
    Quat::from_rotation_arc(Vec3::NEG_Z, -sun_direction)
}

/// Where `setup_world` stands the camera relative to the point it looks at, the centre of the cell
/// at the origin of the world grid: the acceptance screenshot is taken from far above it, the
/// walk-around view from behind and above it. [`sun_shadow_cascades`] measures its range from the
/// camera, so the offsets live here rather than in two places.
fn camera_offset(config: &EngineConfig) -> Vec3 {
    if config.streaming_benchmark_route_speed.is_some() {
        Vec3::new(0.0, 6000.0, 0.0)
    } else if config.acceptance_screenshot.is_some() {
        config
            .screenshot_camera_offset
            .map(Vec3::from)
            .unwrap_or(Vec3::new(0.0, 20_000.0, 1000.0))
    } else {
        Vec3::new(0.0, 1200.0, 2500.0)
    }
}

fn setup_world(
    mut commands: Commands,
    shot_output: Option<Res<ShotOutputImage>>,
    config: Res<EngineConfig>,
    ground_height: Option<Res<InitialCameraGroundHeight>>,
    tuning: Option<Res<MovementTuning>>,
    start_camera: Option<Res<InitialWorldCameraPose>>,
) {
    let ground_height = ground_height.as_deref().map_or(0.0, |height| height.0);
    let target = Vec3::new(CELL_SIZE_HALF, ground_height, -CELL_SIZE_HALF);
    let camera_offset = if config.interactive_world_physics() {
        let tuning = tuning.as_deref().cloned().unwrap_or_default();
        Vec3::Y * (tuning.eye_height + tuning.capsule_standing_height * 0.5 + 16.0)
    } else {
        camera_offset(&config)
    };
    let camera_position = target + camera_offset;
    let far = crate::world::components::CELL_SIZE * (config.stream_radius.max(1) + 2) as f32 * 2.0;
    let mut camera_transform = if config.interactive_world_physics() {
        Transform::from_translation(camera_position)
            .looking_at(camera_position + Vec3::NEG_Z, Vec3::Y)
    } else if config.streaming_benchmark_route_speed.is_some() {
        Transform::from_translation(camera_position)
            .looking_at(target + Vec3::new(0.0, 0.0, -8000.0), Vec3::Y)
    } else {
        Transform::from_translation(camera_position).looking_at(target, Vec3::Y)
    };
    let mut perspective = PerspectiveProjection { far, ..default() };
    if let Some(pose) = start_camera {
        camera_transform.translation = crate::shots::shot_camera_translation(
            pose.shot.position,
            IVec2::new(config.start_grid.0, config.start_grid.1),
        );
        camera_transform.rotation =
            crate::shots::shot_camera_rotation(pose.shot.yaw, pose.shot.pitch);
        perspective.fov =
            crate::shots::vertical_fov_degrees(pose.shot.hfov, pose.aspect).to_radians();
    }
    let mut camera = commands.spawn((
        Camera3d::default(),
        crate::color_pipeline::SceneColorPipeline::default(),
        Projection::Perspective(perspective),
        camera_transform,
        StreamingCamera,
        IsDefaultUiCamera,
        // This camera draws the streamed world, so the weather's distance fog covers it.
        FogCamera,
        // The sky draws a dome around this camera and clears it to the weather's fog colour.
        SkyCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(MAIN_VIEW_LAYERS),
    ));
    if let Some(output) = shot_output {
        camera.insert((
            bevy::camera::RenderTarget::Image(output.0.clone().into()),
            bevy::camera::ShadowLodOrigin,
        ));
    }
    // The sun's shadows. `shadow_maps_enabled` was never the missing piece - the cascades were:
    // without a configuration of its own the sun gets Bevy's, which reaches 150 metres of a world
    // whose unit is 1.43 cm, and a shadow map spent on that patch is a world lit flat
    // (`sun_shadow_cascades`).
    commands.insert_resource(DirectionalLightShadowMap {
        size: SUN_SHADOW_MAP_SIZE,
    });
    // The sun and ambient colours are the clear weather's own (`WTHR` 0x81A, day). Their levels are
    // the configured input ratio in `crate::lights::AMBIENT_ILLUMINANCE` (a simplified heuristic,
    // not a rendered-parity calibration).
    let weather_lighting = crate::sky::WeatherLighting::default();
    commands.spawn((
        DirectionalLight {
            color: weather_lighting.sunlight,
            illuminance: crate::lights::SUN_ILLUMINANCE,
            shadow_maps_enabled: true,
            ..default()
        },
        sun_shadow_cascades(&config),
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(sun_rotation(&config)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: weather_lighting.mean_ambient(),
        // The one definition of the ambient this world path applies: the converted lights are
        // scaled against it (`crate::lights`).
        brightness: crate::lights::AMBIENT_ILLUMINANCE,
        ..default()
    });
    info!(
        assets = %config.assets_dir.display(),
        worldspace = format_args!("{:08X}", config.worldspace_id),
        ground_height,
        camera = ?camera_position,
        target = ?target,
        "Mudcrab runtime initialized"
    );
}

fn initial_camera_ground_height(
    config: &EngineConfig,
    database_path: &std::path::Path,
    cache: &CellCache,
) -> Result<f32> {
    Ok(cell_centre_ground_height(
        config.worldspace_id,
        config.start_grid,
        database_path,
        cache,
    )?
    .unwrap_or(0.0))
}

/// Sample finite local terrain below the actual startup camera pose, if available.
fn initial_walk_ground_height(
    config: &EngineConfig,
    database_path: &std::path::Path,
    cache: &CellCache,
    shot: Option<&crate::shots::Shot>,
) -> Result<Option<f32>> {
    let sample_position = shot.map_or_else(
        || {
            Vec2::new(
                (config.start_grid.0 as f32 + 0.5) * crate::world::components::CELL_SIZE,
                (config.start_grid.1 as f32 + 0.5) * crate::world::components::CELL_SIZE,
            )
        },
        |shot| Vec2::new(shot.position[0], shot.position[1]),
    );
    if !sample_position.is_finite()
        || (sample_position.x / crate::world::components::CELL_SIZE).floor() as i32
            != config.start_grid.0
        || (sample_position.y / crate::world::components::CELL_SIZE).floor() as i32
            != config.start_grid.1
    {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open {}", database_path.display()))?;
    let cell_id = connection
        .query_row(
            crate::world::database::EXTERIOR_CELL_ID_SQL,
            params![
                config.worldspace_id,
                config.start_grid.0,
                config.start_grid.1
            ],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    let Some(terrain) = cell_id.and_then(|cell_id| cache.terrain(cell_id)) else {
        return Ok(None);
    };
    if usize::from(terrain.width) != 33
        || usize::from(terrain.height) != 33
        || terrain.heights.iter().any(|height| !height.is_finite())
    {
        return Ok(None);
    }
    let height =
        RouteGround::from_cells([(config.start_grid, terrain.heights)]).height_at(sample_position);
    Ok(height.filter(|height| height.is_finite()))
}

/// The terrain height at the middle of one exterior cell, or `None` when the cell has no terrain
/// (or the cache has no height there).
fn cell_centre_ground_height(
    worldspace_id: u32,
    grid: (i32, i32),
    database_path: &std::path::Path,
    cache: &CellCache,
) -> Result<Option<f32>> {
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open {}", database_path.display()))?;
    let cell_id = connection
        .query_row(
            crate::world::database::EXTERIOR_CELL_ID_SQL,
            params![worldspace_id, grid.0, grid.1],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    Ok(cell_id.and_then(|cell_id| cache.centre_height(cell_id)))
}

/// Where `--benchmark-jump` puts the camera: the middle of exterior cell `grid`, at that cell's
/// ground height plus the same height offset `setup_world` gives the start position, in the
/// coordinates of the current render `origin`.
pub(crate) fn benchmark_jump_position(
    config: &EngineConfig,
    cache: &CellCache,
    origin: IVec2,
    grid: (i32, i32),
) -> Vec3 {
    let database_path = config.assets_dir.join("skyrim_world.db");
    let ground_height =
        match cell_centre_ground_height(config.worldspace_id, grid, &database_path, cache) {
            Ok(Some(height)) => height,
            // A jump target with no terrain is legitimate (a cell past the map edge the run may
            // still want to view), but a silent 0 puts the camera at the wrong height, so say so.
            Ok(None) => {
                warn!(
                    grid_x = grid.0,
                    grid_y = grid.1,
                    "benchmark jump: no terrain at the target cell, using ground height 0"
                );
                0.0
            }
            Err(error) => {
                warn!(%error, "benchmark jump: ground height unavailable, using 0");
                0.0
            }
        };
    jump_camera_position(grid, origin, ground_height + camera_offset(config).y)
}

/// The middle of grid square `grid` at height `y`, in the render space around `origin`: +x is east,
/// +grid_y runs towards -z.
fn jump_camera_position(grid: (i32, i32), origin: IVec2, y: f32) -> Vec3 {
    Vec3::new(
        ((grid.0 - origin.x) as f32 + 0.5) * crate::world::components::CELL_SIZE,
        y,
        -((grid.1 - origin.y) as f32 + 0.5) * crate::world::components::CELL_SIZE,
    )
}

const CELL_SIZE_HALF: f32 = crate::world::components::CELL_SIZE * 0.5;
const AUTO_FLIGHT_HALF_SPAN: f32 = crate::world::components::CELL_SIZE * 4.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamingBenchmarkPhase {
    #[default]
    Waiting,
    Moving,
    Tail,
    Finished,
}

/// An opt-in real-clock camera workload. CPU settlement plus a fixed quiet
/// interval does not certify GPU preparation or collision safety.
#[derive(Resource, Debug, Default)]
pub(crate) struct StreamingBenchmarkRoute {
    pub phase: StreamingBenchmarkPhase,
    pub cpu_settled_real_seconds: Option<f64>,
    pub movement_started_real_seconds: Option<f64>,
    pub movement_finished_real_seconds: Option<f64>,
    pub tail_finished_real_seconds: Option<f64>,
    pub latest_real_seconds: f64,
    pub route_distance_units: f64,
    pub start_world_position: Option<[f64; 3]>,
}

impl StreamingBenchmarkRoute {
    fn advance(&mut self, now: f64, cpu_ready: bool, config: &EngineConfig) {
        self.latest_real_seconds = now;
        let speed = config.streaming_benchmark_route_speed.unwrap_or(0.0);
        if self.phase == StreamingBenchmarkPhase::Waiting {
            if !cpu_ready {
                self.cpu_settled_real_seconds = None;
                return;
            }
            let settled = *self.cpu_settled_real_seconds.get_or_insert(now);
            if now - settled + f64::EPSILON < config.streaming_benchmark_settle_secs {
                return;
            }
            self.movement_started_real_seconds = Some(now);
            self.phase = if speed == 0.0 {
                self.movement_finished_real_seconds = Some(now);
                StreamingBenchmarkPhase::Tail
            } else {
                StreamingBenchmarkPhase::Moving
            };
        }
        let Some(started) = self.movement_started_real_seconds else {
            return;
        };
        let moving_seconds = if speed == 0.0 {
            0.0
        } else {
            (now - started).clamp(0.0, config.streaming_benchmark_route_secs)
        };
        self.route_distance_units = speed * moving_seconds;
        if self.phase == StreamingBenchmarkPhase::Moving
            && now - started >= config.streaming_benchmark_route_secs
        {
            self.movement_finished_real_seconds =
                Some(started + config.streaming_benchmark_route_secs);
            self.phase = StreamingBenchmarkPhase::Tail;
        }
        if self.phase == StreamingBenchmarkPhase::Tail
            && let Some(finished) = self.movement_finished_real_seconds
            && now - finished >= config.streaming_benchmark_tail_secs
        {
            self.tail_finished_real_seconds = Some(finished + config.streaming_benchmark_tail_secs);
            self.phase = StreamingBenchmarkPhase::Finished;
        }
    }

    fn local_position(&self, origin: IVec2) -> Option<Vec3> {
        let mut position = DVec3::from_array(self.start_world_position?);
        position.z -= self.route_distance_units;
        position.x -= f64::from(origin.x) * f64::from(crate::world::components::CELL_SIZE);
        position.z += f64::from(origin.y) * f64::from(crate::world::components::CELL_SIZE);
        Some(position.as_vec3())
    }
}

fn streaming_benchmark_cpu_ready(
    config: &EngineConfig,
    metrics: &StreamingMetrics,
    admission: &crate::streaming::admission::SceneAdmission,
    renderer: &RendererMetrics,
    raw_responses: usize,
    deferred_responses: usize,
) -> bool {
    let expected_cells =
        ((i64::from(config.stream_radius) * 2 + 1).max(0) as usize).saturating_pow(2);
    metrics.resident_cells == expected_cells
        && metrics.loading_cells == 0
        && metrics.active_requests == 0
        && metrics.requests_submitted == metrics.responses_received
        && metrics.retiring_cells == 0
        && metrics.pending_asset_instances == 0
        && metrics.pending_surface_instances == 0
        && metrics.arming_queue_depth == 0
        && metrics.pending_lod_queries == 0
        && metrics.pending_lod_chunks == 0
        && admission.active_jobs() == 0
        && admission.queued_jobs() == 0
        && raw_responses == 0
        && deferred_responses == 0
        && config
            .streaming_benchmark_expected_models
            .is_none_or(|expected| metrics.assets_ready == expected)
        && config
            .streaming_benchmark_expected_lod_chunks
            .is_none_or(|expected| metrics.resident_lod_chunks == expected)
        && config
            .streaming_benchmark_expected_terrain
            .is_none_or(|expected| metrics.terrain_patches_validated == expected)
        && config
            .streaming_benchmark_expected_water
            .is_none_or(|expected| metrics.water_surfaces_validated == expected)
        && metrics.failed_cells == 0
        && metrics.asset_load_failures == 0
        && metrics.material_validation_failures == 0
        && metrics.terrain_validation_failures == 0
        && metrics.water_validation_failures == 0
        && metrics.transform_bounds_validation_failures == 0
        && metrics.diagnostic_fallbacks == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.failed_lod_queries == 0
        && metrics.unrecovered_lod_queries == 0
        && metrics.failed_lod_chunks == 0
        && metrics.unrecovered_lod_chunks == 0
        && renderer.renderer_validation_failures == 0
        && renderer.final_path_active()
}

#[allow(clippy::too_many_arguments)]
fn drive_streaming_benchmark_route(
    time: Res<Time<Real>>,
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    metrics: Res<StreamingMetrics>,
    admission: Res<crate::streaming::admission::SceneAdmission>,
    renderer: Res<RendererMetrics>,
    database: Res<WorldDatabase>,
    runtime: Option<Res<crate::streaming::runtime::StreamingRuntime>>,
    mut route: ResMut<StreamingBenchmarkRoute>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };
    route.start_world_position.get_or_insert([
        f64::from(transform.translation.x)
            + f64::from(origin.0.x) * f64::from(crate::world::components::CELL_SIZE),
        f64::from(transform.translation.y),
        f64::from(transform.translation.z)
            - f64::from(origin.0.y) * f64::from(crate::world::components::CELL_SIZE),
    ]);
    let ready = streaming_benchmark_cpu_ready(
        &config,
        &metrics,
        &admission,
        &renderer,
        database.pending_cell_responses(),
        runtime
            .as_ref()
            .map_or(0, |runtime| runtime.deferred_responses.len()),
    );
    let before = route.phase;
    route.advance(time.elapsed_secs_f64(), ready, &config);
    if let Some(position) = route.local_position(origin.0) {
        transform.translation = position;
    }
    if route.phase != before {
        let stage = match route.phase {
            StreamingBenchmarkPhase::Waiting => "route_waiting",
            StreamingBenchmarkPhase::Moving => "route_moving",
            StreamingBenchmarkPhase::Tail => "route_tail",
            StreamingBenchmarkPhase::Finished => "route_finished",
        };
        info!(phase = ?route.phase, distance = route.route_distance_units, "streaming benchmark route phase");
        profiler.event("streaming-benchmark-route", stage, None);
    }
}

#[derive(Default)]
struct AutoFlightState {
    initialized: bool,
    axis: Vec3,
    sign: f32,
    offset: f32,
}

fn bounded_auto_flight_direction(
    forward: Vec3,
    step_distance: f32,
    state: &mut AutoFlightState,
) -> Vec3 {
    if !state.initialized {
        state.initialized = true;
        state.axis = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
        state.sign = 1.0;
    }
    let next_offset = state.offset + state.sign * step_distance.max(0.0);
    if next_offset >= AUTO_FLIGHT_HALF_SPAN {
        state.sign = -1.0;
    } else if next_offset <= -AUTO_FLIGHT_HALF_SPAN {
        state.sign = 1.0;
    }
    state.offset += state.sign * step_distance.max(0.0);
    state.axis * state.sign
}

/// The two `Update` systems every run gets, both gated on the console being closed. The console
/// owns the keyboard while it is open and on the frame it closes, so the free camera must not fly
/// on those keys and the acceptance screenshot must not enqueue a capture of the open overlay into
/// the PNG the acceptance check reads. Both frames are rejected by the predicate, and
/// `refresh_console_ui` hides the overlay on the closing frame, before the capture resumes on the
/// next update.
fn add_camera_and_capture_systems(app: &mut App) {
    app.add_systems(
        Update,
        (
            fly_camera.run_if(console_closed),
            capture_acceptance_screenshot.run_if(console_closed),
        ),
    );
}

fn fly_camera(
    time: Res<Time>,
    config: Res<EngineConfig>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
    mut auto_flight: Local<AutoFlightState>,
) {
    // Interactive player paths own the camera; automated camera paths keep legacy controls. A
    // shots run poses the camera itself, and a key press must not move a pose.
    // A route owns the camera as well, like the matched and streaming benchmark routes.
    if config.physics_fixture
        || config.interactive_world_physics()
        || config.shots.is_some()
        || config.route.is_some()
        || config.streaming_benchmark_route_speed.is_some()
        || config.matched_route.is_some()
    {
        return;
    }
    let started = std::time::Instant::now();
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };
    let mut direction = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        direction += *transform.forward();
    }
    if keyboard.pressed(KeyCode::KeyS) {
        direction += *transform.back();
    }
    if keyboard.pressed(KeyCode::KeyA) {
        direction += *transform.left();
    }
    if keyboard.pressed(KeyCode::KeyD) {
        direction += *transform.right();
    }
    if keyboard.pressed(KeyCode::Space) {
        direction += Vec3::Y;
    }
    if keyboard.pressed(KeyCode::ShiftLeft) {
        direction -= Vec3::Y;
    }
    let acceptance_capture_pending = config
        .acceptance_screenshot
        .as_ref()
        .is_some_and(|path| !path.is_file());
    let speed = if config.auto_fly_speed > 0.0 {
        config.auto_fly_speed
    } else if keyboard.pressed(KeyCode::ControlLeft) {
        4000.0
    } else {
        900.0
    };
    if config.auto_fly_speed > 0.0 && !acceptance_capture_pending {
        direction += bounded_auto_flight_direction(
            *transform.forward(),
            speed * time.delta_secs(),
            &mut auto_flight,
        );
    }
    transform.translation += direction.normalize_or_zero() * speed * time.delta_secs();
    profiler.record_elapsed("world/fly_camera", started);
}

/// Frames the capture system waits past the benchmark warmup before it takes the screenshot.
const SCREENSHOT_SETTLE_FRAMES: u32 = 10;

/// Real time the capture system gives the renderer to warm up (shader compilation and first use of
/// the final path) before it takes the screenshot.
///
/// Real time, not `Time`: the console pauses virtual time while it is open, and the wait is for the
/// GPU, which is not paused with it.
const SCREENSHOT_GPU_WARMUP: std::time::Duration = std::time::Duration::from_secs(2);

#[allow(clippy::too_many_arguments)]
fn capture_acceptance_screenshot(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut state: Local<ScreenshotCaptureState>,
    real_time: Res<Time<Real>>,
    streaming: Option<Res<StreamingMetrics>>,
    world_database: Option<Res<WorldDatabase>>,
    renderer: Res<RendererMetrics>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
) {
    let Some(path) = &config.acceptance_screenshot else {
        return;
    };
    state.frames = state.frames.saturating_add(1);
    let settled_frames = config
        .benchmark_warmup_frames
        .saturating_add(SCREENSHOT_SETTLE_FRAMES);
    if state.captured
        || state.frames < settled_frames
        || real_time.elapsed() < SCREENSHOT_GPU_WARMUP
        || windows.is_empty()
    {
        return;
    }
    let assets_ready = streaming
        .as_deref()
        .is_none_or(|metrics| screenshot_assets_ready(metrics, world_database.is_some(), &config));
    let renderer_ready = renderer.final_path_active()
        && (!config.renderer_fixture || renderer.renderer_fixture_validated);
    if !assets_ready || !renderer_ready {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        error!(%error, path = %path.display(), "failed to create screenshot directory");
        return;
    }
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path.clone()));
    state.captured = true;
}

fn screenshot_assets_ready(
    metrics: &StreamingMetrics,
    world_streaming_active: bool,
    config: &EngineConfig,
) -> bool {
    metrics.pending_asset_instances == 0
        && metrics.pending_surface_instances == 0
        && metrics.loading_cells == 0
        && metrics.pending_lod_queries == 0
        && metrics.pending_lod_chunks == 0
        && metrics.failed_cells == 0
        && metrics.unrecovered_lod_queries == 0
        && metrics.unrecovered_lod_chunks == 0
        && (!world_streaming_active || metrics.resident_cells > 0)
        && metrics.asset_load_failures == 0
        && metrics.material_validation_failures == 0
        && metrics.transform_bounds_validation_failures == 0
        && metrics.diagnostic_fallbacks == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.streaming_fixture_failures == 0
        && (!config.material_fixture || metrics.canonical_fixture_validated)
        && (!config.terrain_water_fixture || metrics.terrain_water_fixture_validated)
        && (!config.transform_bounds_fixture || metrics.transform_bounds_fixture_validated)
        && (!config.streaming_fixture || metrics.streaming_fixture_validated)
        && (!config.physics_fixture || metrics.physics_fixture_validated)
}

#[derive(Default)]
struct ScreenshotCaptureState {
    frames: u32,
    captured: bool,
}

#[cfg(test)]
mod tests {
    /// A `--route` file shares the camera with neither `--shots` nor `--matched-route`. The refusal
    /// comes before the shots file is read, so the file names here need not exist.
    #[test]
    fn a_route_refuses_shots_and_matched_route_before_reading_either() {
        for config in [
            EngineConfig {
                route: Some("travel.json".into()),
                shots: Some("poses.json".into()),
                ..EngineConfig::default()
            },
            EngineConfig {
                route: Some("travel.json".into()),
                matched_route: Some("route.json".into()),
                ..EngineConfig::default()
            },
        ] {
            let mut config = config;
            let error = match super::prepare_shots(&mut config) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("a route with a shots or matched route must be refused"),
            };
            assert!(
                error.contains("renders the converted world on its own")
                    && error.contains("--route"),
                "{error}"
            );
        }
    }

    use super::*;
    use crate::console::ConsoleState;
    use bevy::asset::{AssetApp, AssetPlugin};
    use bevy::input::ButtonState;
    use bevy::input::keyboard::{Key, KeyboardInput};
    use bevy::time::TimeUpdateStrategy;
    use bevy::window::{PrimaryWindow, Window};
    use bevy::world_serialization::WorldSerializationPlugin;
    use std::time::Duration;

    #[test]
    fn benchmark_route_requires_an_uninterrupted_cpu_settled_interval() {
        let config = EngineConfig {
            streaming_benchmark_route_speed: Some(370.0),
            ..default()
        };
        let mut route = StreamingBenchmarkRoute::default();
        route.advance(1.0, false, &config);
        route.advance(2.0, true, &config);
        route.advance(6.0, true, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Waiting);
        assert_eq!(route.cpu_settled_real_seconds, Some(2.0));
        route.advance(6.5, false, &config);
        assert_eq!(route.cpu_settled_real_seconds, None);
        route.advance(8.0, true, &config);
        route.advance(13.0, true, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Moving);
        assert_eq!(route.cpu_settled_real_seconds, Some(8.0));
        assert_eq!(route.movement_started_real_seconds, Some(13.0));
        // New streaming demand during the route must not restart the wait.
        route.advance(14.0, false, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Moving);
        assert_eq!(route.route_distance_units, 370.0);
    }

    #[test]
    fn benchmark_route_uses_absolute_real_time_across_large_deltas_and_rebases() {
        let config = EngineConfig {
            streaming_benchmark_route_speed: Some(3000.0),
            streaming_benchmark_settle_secs: 0.0,
            ..default()
        };
        let mut route = StreamingBenchmarkRoute {
            start_world_position: Some([22_528.0, 5976.0, 47_104.0]),
            ..default()
        };
        route.advance(10.0, true, &config);
        let mut partitioned = StreamingBenchmarkRoute {
            start_world_position: route.start_world_position,
            ..default()
        };
        partitioned.advance(10.0, true, &config);
        for now in [10.1, 10.4, 10.7, 11.0] {
            partitioned.advance(now, false, &config);
        }
        // A one-second real interval is not reduced to virtual time's 250 ms cap.
        route.advance(11.0, false, &config);
        assert_eq!(route.route_distance_units, 3000.0);
        assert_eq!(route.route_distance_units, partitioned.route_distance_units);
        let before = route.local_position(IVec2::new(5, -12)).unwrap();
        let after = route.local_position(IVec2::new(5, -11)).unwrap();
        assert_eq!(before.x, after.x);
        assert_eq!(before.y, after.y);
        assert_eq!(after.z - before.z, crate::world::components::CELL_SIZE);
        let stable = |position: Vec3, origin: IVec2| {
            f64::from(position.z)
                - f64::from(origin.y) * f64::from(crate::world::components::CELL_SIZE)
        };
        assert_eq!(
            stable(before, IVec2::new(5, -12)),
            stable(after, IVec2::new(5, -11))
        );
        // A long stalled frame clamps at the planned endpoint and crosses both
        // phase boundaries without extending the route or the tail.
        route.advance(1000.0, false, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Finished);
        assert_eq!(route.route_distance_units, 135_000.0);
        assert_eq!(route.movement_finished_real_seconds, Some(55.0));
        assert_eq!(route.tail_finished_real_seconds, Some(65.0));
    }

    #[test]
    fn stationary_benchmark_route_enters_tail_without_movement() {
        let config = EngineConfig {
            streaming_benchmark_route_speed: Some(0.0),
            streaming_benchmark_settle_secs: 0.0,
            streaming_benchmark_tail_secs: 3.0,
            ..default()
        };
        let mut route = StreamingBenchmarkRoute::default();
        route.advance(4.0, true, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Tail);
        assert_eq!(route.movement_started_real_seconds, Some(4.0));
        assert_eq!(route.movement_finished_real_seconds, Some(4.0));
        assert_eq!(route.route_distance_units, 0.0);
        route.advance(6.0, false, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Tail);
        route.advance(7.0, false, &config);
        assert_eq!(route.phase, StreamingBenchmarkPhase::Finished);
        assert_eq!(route.tail_finished_real_seconds, Some(7.0));
        assert_eq!(route.route_distance_units, 0.0);
    }

    #[test]
    fn benchmark_cpu_gate_checks_expected_counts_and_pending_database_payloads() {
        let config = EngineConfig {
            streaming_benchmark_expected_models: Some(7),
            streaming_benchmark_expected_lod_chunks: Some(3),
            streaming_benchmark_expected_terrain: Some(4),
            streaming_benchmark_expected_water: Some(1),
            ..default()
        };
        let metrics = StreamingMetrics {
            resident_cells: 25,
            requests_submitted: 25,
            responses_received: 25,
            assets_ready: 7,
            resident_lod_chunks: 3,
            terrain_patches_validated: 4,
            water_surfaces_validated: 1,
            ..default()
        };
        let admission = crate::streaming::admission::SceneAdmission::default();
        let renderer = RendererMetrics {
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
        let ready = |metrics: &StreamingMetrics, raw, deferred| {
            streaming_benchmark_cpu_ready(&config, metrics, &admission, &renderer, raw, deferred)
        };
        assert!(ready(&metrics, 0, 0));
        assert!(!ready(&metrics, 1, 0));
        assert!(!ready(&metrics, 0, 1));
        assert!(!ready(
            &StreamingMetrics {
                assets_ready: 6,
                ..metrics.clone()
            },
            0,
            0
        ));
        assert!(!ready(
            &StreamingMetrics {
                pending_lod_queries: 1,
                ..metrics.clone()
            },
            0,
            0
        ));
        assert!(!ready(
            &StreamingMetrics {
                pending_surface_instances: 1,
                ..metrics.clone()
            },
            0,
            0
        ));
        assert!(!ready(
            &StreamingMetrics {
                asset_load_failures: 1,
                ..metrics
            },
            0,
            0
        ));
    }

    #[test]
    fn automated_camera_conflicts_name_the_selected_drivers() {
        let mut both = EngineConfig {
            shots: Some("shots.json".into()),
            matched_route: Some("route.json".into()),
            ..default()
        };
        let Err(error) = prepare_shots(&mut both) else {
            panic!("drivers must conflict");
        };
        let message = error.to_string();
        assert!(message.contains("--shots"));
        assert!(message.contains("--matched-route"));

        let mut route_benchmark = EngineConfig {
            matched_route: Some("route.json".into()),
            benchmark_frames: Some(60),
            ..default()
        };
        let Err(error) = prepare_shots(&mut route_benchmark) else {
            panic!("benchmark must conflict with route");
        };
        let message = error.to_string();
        assert!(message.contains("--matched-route"));
        assert!(message.contains("benchmark"));
    }

    #[test]
    fn live_start_pose_rejects_modes_that_replace_its_world_location() {
        let directory = tempfile::tempdir().unwrap();
        let pose_file = directory.path().join("pose.json");
        std::fs::write(
            &pose_file,
            r#"{"width":1920,"height":1080,"shots":[{"name":"inn","worldspace_id":60,"position":[21750,-44530,140],"yaw":90,"pitch":-3,"hfov":80}]}"#,
        )
        .unwrap();
        let conflicting_modes = [
            (
                "camera route",
                EngineConfig {
                    route: Some("missing-route.json".into()),
                    ..default()
                },
            ),
            (
                "acceptance screenshot",
                EngineConfig {
                    acceptance_screenshot: Some("acceptance.png".into()),
                    ..default()
                },
            ),
            (
                "streaming route benchmark",
                EngineConfig {
                    streaming_benchmark_route_speed: Some(100.0),
                    ..default()
                },
            ),
            (
                "benchmark jump",
                EngineConfig {
                    benchmark_jump: Some((12, -7)),
                    ..default()
                },
            ),
            (
                "matched route",
                EngineConfig {
                    matched_route: Some("missing-route.json".into()),
                    ..default()
                },
            ),
        ];

        for (mode, mut config) in conflicting_modes {
            config.start_shot = Some(pose_file.clone());
            config.worldspace_id = 60;
            config.start_grid = (5, -11);
            let original_worldspace = config.worldspace_id;
            let original_grid = config.start_grid;

            let Err(error) = prepare_start_camera(&mut config) else {
                panic!("{mode} must be rejected before loading the camera pose");
            };

            assert!(
                error.to_string().contains("another automated camera mode"),
                "{mode} should fail at the mode guard, got: {error}"
            );
            assert_eq!(config.worldspace_id, original_worldspace, "{mode}");
            assert_eq!(config.start_grid, original_grid, "{mode}");
        }
    }

    #[test]
    fn no_start_pose_leaves_existing_camera_modes_unchanged() {
        let mut config = EngineConfig {
            route: Some("route.json".into()),
            acceptance_screenshot: Some("acceptance.png".into()),
            streaming_benchmark_route_speed: Some(100.0),
            benchmark_jump: Some((12, -7)),
            ..default()
        };

        assert!(prepare_start_camera(&mut config).unwrap().is_none());
        assert_eq!(
            config.route.as_deref(),
            Some(std::path::Path::new("route.json"))
        );
        assert_eq!(
            config.acceptance_screenshot.as_deref(),
            Some(std::path::Path::new("acceptance.png"))
        );
        assert_eq!(config.streaming_benchmark_route_speed, Some(100.0));
        assert_eq!(config.benchmark_jump, Some((12, -7)));
    }

    /// A capture window remains hidden if an update system tries to show it.
    #[test]
    fn hidden_capture_suppresses_visibility_changes_before_window_sync() {
        let mut app = App::new();
        app.insert_resource(EngineConfig {
            hidden_window: true,
            ..default()
        })
        .add_systems(PostUpdate, keep_windows_hidden);
        let entity = app
            .world_mut()
            .spawn(Window {
                visible: true,
                ..default()
            })
            .id();
        app.update();
        assert!(!app.world().get::<Window>(entity).unwrap().visible);
        app.world_mut().resource_mut::<EngineConfig>().hidden_window = false;
        app.world_mut().get_mut::<Window>(entity).unwrap().visible = true;
        app.update();
        assert!(app.world().get::<Window>(entity).unwrap().visible);
    }

    /// The origin cell's centre is (2048, y, -2048), the start position's convention, and +grid_y
    /// runs towards -z, so the sign and the half-cell offset are pinned per axis.
    #[test]
    fn jump_position_is_the_target_cells_centre_around_the_origin() {
        for (grid, origin, expected) in [
            ((0, 0), IVec2::ZERO, Vec3::new(2048.0, 9.0, -2048.0)),
            ((1, 0), IVec2::ZERO, Vec3::new(6144.0, 9.0, -2048.0)),
            ((0, 1), IVec2::ZERO, Vec3::new(2048.0, 9.0, -6144.0)),
            ((-1, -1), IVec2::ZERO, Vec3::new(-2048.0, 9.0, 2048.0)),
            (
                (4, -21),
                IVec2::new(4, -12),
                Vec3::new(2048.0, 9.0, 34816.0),
            ),
        ] {
            assert_eq!(
                jump_camera_position(grid, origin, 9.0),
                expected,
                "grid {grid:?} origin {origin:?}"
            );
        }
    }

    #[test]
    fn relative_asset_root_is_shared_by_database_and_streamed_files() {
        use bevy::asset::io::{AssetReader, Reader, file::FileAssetReader};

        // Keep the process CWD and environment intact so this can run alongside other tests.
        let cwd = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir_in(&cwd).unwrap();
        let mut config = EngineConfig {
            assets_dir: directory.path().strip_prefix(&cwd).unwrap().to_owned(),
            ..default()
        };
        let plugin = asset_plugin(&mut config).unwrap();
        assert!(config.assets_dir.is_absolute());
        assert_eq!(config.assets_dir, directory.path());

        let database_path = config.assets_dir.join("skyrim_world.db");
        Connection::open(&database_path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE fixture (value TEXT); INSERT INTO fixture VALUES ('same root');",
            )
            .unwrap();
        fs::create_dir(config.assets_dir.join("meshes")).unwrap();
        fs::write(config.assets_dir.join("meshes/model.glb"), b"model").unwrap();
        fs::create_dir(config.assets_dir.join("textures")).unwrap();
        fs::write(config.assets_dir.join("textures/image.png"), b"image").unwrap();

        let reader = FileAssetReader::new(plugin.file_path);
        assert_eq!(reader.root_path(), &config.assets_dir);
        bevy::tasks::block_on(async {
            for (path, expected) in [
                ("meshes/model.glb", b"model".as_slice()),
                ("textures/image.png", b"image".as_slice()),
            ] {
                let mut file = reader.read(Path::new(path)).await.unwrap();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, expected);
            }
        });
        let database =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let value: String = database
            .query_row("SELECT value FROM fixture", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "same root");
    }

    #[test]
    fn asset_root_resolution_allows_missing_directories_and_preserves_absolute_paths() {
        let directory = tempfile::tempdir().unwrap();
        let absolute = directory.path().join("not-created");
        let mut config = EngineConfig {
            assets_dir: absolute.clone(),
            ..default()
        };
        let plugin = asset_plugin(&mut config).unwrap();
        assert_eq!(config.assets_dir, absolute);
        assert_eq!(PathBuf::from(plugin.file_path), absolute);
        assert!(!absolute.exists());

        config.assets_dir = EngineConfig::default().assets_dir;
        asset_plugin(&mut config).unwrap();
        assert_eq!(
            config.assets_dir,
            std::env::current_dir().unwrap().join("modern_assets")
        );
    }

    #[test]
    fn an_existing_io_pool_of_another_size_is_an_error_only_for_an_explicit_request() {
        // No pool yet, or one that already has the wanted size: nothing to report.
        check_existing_io_pool(4, 4, None).unwrap();
        check_existing_io_pool(4, 4, Some(4)).unwrap();
        // An explicit request the existing pool cannot meet is an actionable error.
        let error = check_existing_io_pool(4, 4, Some(2))
            .unwrap_err()
            .to_string();
        assert!(error.contains("io_threads is 4"), "{error}");
        assert!(error.contains("2 threads"), "{error}");
        // The automatic size is a default: the host's pool is kept.
        check_existing_io_pool(0, 2, Some(3)).unwrap();
    }

    #[test]
    fn io_pool_is_a_quarter_of_the_hardware_threads_unless_requested() {
        assert_eq!(io_pool_threads(0, 16).unwrap(), 4);
        assert_eq!(io_pool_threads(0, 8).unwrap(), 2);
        assert_eq!(io_pool_threads(0, 4).unwrap(), 1);
        assert_eq!(io_pool_threads(0, 1).unwrap(), 1);
        assert_eq!(io_pool_threads(0, 64).unwrap(), 4);
        assert_eq!(io_pool_threads(6, 16).unwrap(), 6);
        assert_eq!(io_pool_threads(32, 16).unwrap(), 32);
        assert_eq!(io_pool_threads(MAX_IO_THREADS, 16).unwrap(), MAX_IO_THREADS);
        // A hand-built `EngineConfig` skips the `--io-threads` check; the pool still refuses it.
        let error = io_pool_threads(MAX_IO_THREADS + 1, 16)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most 64 threads"), "{error}");
    }

    /// Uses real loaders with a generated external normal image, without game assets or a GPU.
    fn world_normal_loader_fixture(wrap_s: u32, wrap_t: u32) -> (tempfile::TempDir, App) {
        use bevy::{
            image::{CompressedImageFormats, ImageLoader, ImagePlugin},
            scene::ScenePlugin,
        };

        let directory = tempfile::tempdir().unwrap();
        Image::new_fill(
            Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[128, 128, 255, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::MAIN_WORLD,
        )
        .try_into_dynamic()
        .unwrap()
        .save(directory.path().join("shared.png"))
        .unwrap();
        fs::write(
            directory.path().join("road.gltf"),
            serde_json::to_vec(&serde_json::json!({
                "asset": {"version": "2.0"},
                "images": [{"uri": "shared.png"}],
                "samplers": [{
                    "wrapS": wrap_s, "wrapT": wrap_t,
                    "magFilter": 9728, "minFilter": 9984
                }],
                "textures": [{"source": 0, "sampler": 0}],
                "materials": [{"normalTexture": {"index": 0}}]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                file_path: directory.path().to_string_lossy().into_owned(),
                ..default()
            },
            ImagePlugin::default(),
            ScenePlugin,
            world_gltf_plugin(),
        ))
        .init_asset::<Mesh>()
        .register_asset_loader(ImageLoader::new(CompressedImageFormats::NONE));
        app.finish();
        app.cleanup();
        (directory, app)
    }

    /// Waits for dependencies too, so each test controls which consumer finishes loading first.
    fn wait_for_world_asset<A: Asset>(app: &mut App, handle: &Handle<A>) {
        use bevy::asset::LoadState;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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

    fn loaded_world_normal(app: &App, handle: &Handle<bevy::gltf::Gltf>) -> Handle<Image> {
        use bevy::gltf::{Gltf, GltfMaterial};

        let gltf = app.world().resource::<Assets<Gltf>>().get(handle).unwrap();
        app.world()
            .resource::<Assets<GltfMaterial>>()
            .get(&gltf.materials[0])
            .unwrap()
            .normal_map_texture
            .as_ref()
            .unwrap()
            .clone()
    }

    #[test]
    fn shared_world_normal_sampler_is_independent_of_first_consumer() {
        use bevy::{
            gltf::Gltf,
            image::{ImageLoaderSettings, ImageSampler},
        };

        for land_first in [false, true] {
            let (_directory, mut app) = world_normal_loader_fixture(10497, 10497);
            let server = app.world().resource::<AssetServer>().clone();
            let load_land = || {
                server
                    .load_builder()
                    .with_settings(|settings: &mut ImageLoaderSettings| {
                        settings.is_srgb = false;
                        settings.sampler = ImageSampler::Descriptor(terrain_layer_sampler());
                    })
                    .load::<Image>("shared.png")
            };
            let (land, road) = if land_first {
                let land = load_land();
                wait_for_world_asset(&mut app, &land);
                let road = server.load::<Gltf>("road.gltf");
                wait_for_world_asset(&mut app, &road);
                (land, road)
            } else {
                let road = server.load::<Gltf>("road.gltf");
                wait_for_world_asset(&mut app, &road);
                let land = load_land();
                wait_for_world_asset(&mut app, &land);
                (land, road)
            };
            assert_eq!(loaded_world_normal(&app, &road), land);
            let image = app.world().resource::<Assets<Image>>().get(&land).unwrap();
            assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba8Unorm);
            assert_eq!(
                image.sampler,
                ImageSampler::Descriptor(terrain_layer_sampler())
            );
        }
    }

    #[test]
    fn v15_distinct_wrap_aliases_keep_samplers_in_both_load_orders() {
        use bevy::{
            gltf::Gltf,
            image::{ImageAddressMode, ImageSampler},
        };
        for clamp_first in [false, true] {
            let (directory, mut app) = world_normal_loader_fixture(10497, 10497);
            fs::copy(
                directory.path().join("shared.png"),
                directory.path().join("shared.opensky-wrap0.png"),
            )
            .unwrap();
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(directory.path().join("road.gltf")).unwrap())
                    .unwrap();
            document["images"][0]["uri"] = serde_json::json!("shared.opensky-wrap0.png");
            document["samplers"][0]["wrapS"] = serde_json::json!(33071);
            document["samplers"][0]["wrapT"] = serde_json::json!(33071);
            fs::write(
                directory.path().join("clamp.gltf"),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
            let names = if clamp_first {
                ["clamp.gltf", "road.gltf"]
            } else {
                ["road.gltf", "clamp.gltf"]
            };
            let mut retained = Vec::new();
            for name in names {
                let handle = app.world().resource::<AssetServer>().load::<Gltf>(name);
                wait_for_world_asset(&mut app, &handle);
                retained.push((name, handle));
            }
            assert_ne!(
                loaded_world_normal(&app, &retained[0].1),
                loaded_world_normal(&app, &retained[1].1)
            );
            for (name, handle) in &retained {
                let image_handle = loaded_world_normal(&app, handle);
                let image = app
                    .world()
                    .resource::<Assets<Image>>()
                    .get(&image_handle)
                    .unwrap();
                let ImageSampler::Descriptor(sampler) = &image.sampler else {
                    panic!("missing sampler")
                };
                let expected = if *name == "clamp.gltf" {
                    ImageAddressMode::ClampToEdge
                } else {
                    ImageAddressMode::Repeat
                };
                assert_eq!(sampler.address_mode_u, expected);
                assert_eq!(sampler.address_mode_v, expected);
            }
        }
    }

    #[test]
    fn world_gltf_sampler_preserves_authored_wrapping_with_linear_anisotropy() {
        use bevy::{
            gltf::Gltf,
            image::{ImageAddressMode, ImageFilterMode, ImageSampler},
        };

        for (wrap_s, wrap_t, expected_u, expected_v) in [
            (
                33071,
                33648,
                ImageAddressMode::ClampToEdge,
                ImageAddressMode::MirrorRepeat,
            ),
            (
                33648,
                33071,
                ImageAddressMode::MirrorRepeat,
                ImageAddressMode::ClampToEdge,
            ),
        ] {
            let (_directory, mut app) = world_normal_loader_fixture(wrap_s, wrap_t);
            let road = app
                .world()
                .resource::<AssetServer>()
                .load::<Gltf>("road.gltf");
            wait_for_world_asset(&mut app, &road);
            let normal = loaded_world_normal(&app, &road);
            let image = app
                .world()
                .resource::<Assets<Image>>()
                .get(&normal)
                .unwrap();
            let ImageSampler::Descriptor(sampler) = &image.sampler else {
                panic!("glTF image sampler descriptor missing");
            };
            let default_sampler = terrain_layer_sampler();
            assert_eq!(sampler.address_mode_u, expected_u);
            assert_eq!(sampler.address_mode_v, expected_v);
            assert_eq!(sampler.anisotropy_clamp, default_sampler.anisotropy_clamp);
            assert_eq!(sampler.mag_filter, ImageFilterMode::Linear);
            assert_eq!(sampler.min_filter, ImageFilterMode::Linear);
            assert_eq!(sampler.mipmap_filter, ImageFilterMode::Linear);
            assert_eq!(sampler.lod_min_clamp, default_sampler.lod_min_clamp);
            assert_eq!(sampler.lod_max_clamp, default_sampler.lod_max_clamp);
        }
    }

    #[test]
    fn a_failed_run_is_an_error_and_a_successful_one_is_not() {
        assert!(finished(&AppExit::Success).is_ok());
        let message = finished(&AppExit::error())
            .expect_err("a failed run must not report success")
            .to_string();
        assert!(message.contains("exit code"), "{message}");
    }

    fn render_error(ty: ErrorType) -> RenderError {
        RenderError {
            ty,
            description: String::new(),
            source: None,
        }
    }

    #[test]
    fn device_loss_exits_with_its_own_code() {
        assert_eq!(
            exit_for_render_error(&render_error(ErrorType::DeviceLost)),
            AppExit::from_code(EXIT_DEVICE_LOST)
        );
    }

    #[test]
    fn other_render_errors_keep_the_generic_failure_code() {
        for ty in [
            ErrorType::OutOfMemory,
            ErrorType::Validation,
            ErrorType::Internal,
        ] {
            assert_eq!(exit_for_render_error(&render_error(ty)), AppExit::error());
        }
    }

    /// Calls the handler the way Bevy's render state machine does and returns its policy and the
    /// exits it wrote to the main world.
    fn handle(ty: ErrorType) -> (RenderErrorPolicy, Vec<AppExit>) {
        let mut main_world = World::new();
        main_world.init_resource::<Messages<AppExit>>();
        let mut render_world = World::new();
        let policy = handle_render_error(&render_error(ty), &mut main_world, &mut render_world);
        let exits = main_world
            .resource_mut::<Messages<AppExit>>()
            .drain()
            .collect();
        (policy, exits)
    }

    #[test]
    fn the_handler_quits_with_the_device_loss_code_and_stops_rendering() {
        let (policy, exits) = handle(ErrorType::DeviceLost);
        assert!(matches!(policy, RenderErrorPolicy::StopRendering));
        assert_eq!(exits, vec![AppExit::from_code(EXIT_DEVICE_LOST)]);
    }

    /// Bevy's default handler quits with the generic failure and stops rendering for every error;
    /// any other error must still do exactly that.
    #[test]
    fn the_handler_keeps_bevys_default_for_other_render_errors() {
        let (policy, exits) = handle(ErrorType::Validation);
        assert!(matches!(policy, RenderErrorPolicy::StopRendering));
        assert_eq!(exits, vec![AppExit::error()]);
    }

    #[test]
    fn a_lost_device_run_is_reported_as_device_loss() {
        let error = finished(&AppExit::from_code(EXIT_DEVICE_LOST))
            .expect_err("a lost device must not report success");
        assert!(error.downcast_ref::<DeviceLost>().is_some());
        assert!(error.to_string().contains("GPU device was lost"), "{error}");
        let other = finished(&AppExit::error()).expect_err("a failed run is an error");
        assert!(other.downcast_ref::<DeviceLost>().is_none());
    }

    /// The streaming fixture's own systems over its own fixture database, with no window, GPU or
    /// game data: the camera crosses exteriors, the fixture loads its interior by id, and the
    /// camera finishes on a fully streamed exterior ring around the grid it started on.
    fn streaming_fixture_app() -> (App, StreamingFixtureDirectory) {
        let mut config = EngineConfig {
            streaming_fixture: true,
            ..default()
        };
        let directory =
            StreamingFixtureDirectory::create(config.worldspace_id, config.start_grid).unwrap();
        config.assets_dir = directory.path.clone();
        let database_path = config.assets_dir.join("skyrim_world.db");
        let cache_path = config.assets_dir.join("cell_cache.rkyv");
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                watch_for_changes_override: Some(false),
                ..default()
            },
            WorldSerializationPlugin,
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .init_asset::<TerrainMaterial>()
        .init_asset::<WaterMaterial>()
        .insert_resource(config)
        .insert_resource(WorldDatabase::open(&database_path).unwrap())
        .insert_resource(AssetCatalog::open(&database_path).unwrap())
        .init_resource::<ActorCatalog>()
        .insert_resource(CellCache::open(&cache_path).unwrap())
        .insert_resource(RenderOrigin(IVec2::ZERO))
        .insert_resource(WaterReflectionTexture(Handle::<Image>::default()))
        .init_resource::<ProfilingState>()
        .init_resource::<StreamingFixtureState>()
        .add_plugins(StreamingPlugin)
        .add_systems(
            PreUpdate,
            (drive_streaming_fixture, cross_streaming_fixture_interior).chain(),
        )
        .add_systems(PostUpdate, validate_streaming_fixture);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        (app, directory)
    }

    #[test]
    fn streaming_fixture_loads_its_interior_by_id_and_keeps_it_while_the_camera_crosses_exteriors()
    {
        let (mut app, _directory) = streaming_fixture_app();
        let expected_resident = {
            let config = app.world().resource::<EngineConfig>();
            ((config.stream_radius * 2 + 1).max(0) as usize).pow(2)
        };
        let mut settled_frames = 0;
        let mut updates = 0;
        while updates < 1_200 {
            app.update();
            // The fixture's frames are vsynced in the acceptance run, and its frame budget assumes
            // that pacing. Pace the headless loop like a 60 Hz frame, so a loaded test machine
            // cannot make the fixture give up before the database worker has answered its cells.
            std::thread::sleep(std::time::Duration::from_millis(16));
            updates += 1;
            let metrics = app.world().resource::<StreamingMetrics>();
            let state = app.world().resource::<StreamingFixtureState>();
            // Settling alone can come before the validator's frame 90 on a fast database, so also
            // wait for the validator's verdict, pass or fail.
            let judged =
                metrics.streaming_fixture_validated || metrics.streaming_fixture_failures > 0;
            let crossed_back = judged
                && state.interior.resident_frame.is_some()
                && metrics.resident_cells >= expected_resident
                && metrics.active_requests == 0
                && metrics.loading_cells == 0;
            settled_frames = if crossed_back { settled_frames + 1 } else { 0 };
            if settled_frames >= 5 {
                break;
            }
        }
        assert!(settled_frames >= 5, "the fixture never settled");

        let interior = app.world().resource::<StreamingFixtureState>().interior;
        assert_eq!(
            interior.requested_frame,
            Some(STREAMING_FIXTURE_INTERIOR_FRAME)
        );
        assert!(interior.resident_frame.is_some());
        assert!(interior.away_samples > 0, "camera never left the grid");
        assert!(
            interior.max_away_roots <= 1,
            "the interior must never have two roots"
        );
        // Current behaviour, not the contract: nothing unloads an interior on this tree, so it is
        // still one root when the camera comes back.
        assert_eq!(interior.min_away_roots, 1);

        let world = app.world_mut();
        let mut roots = world.query_filtered::<&CellRef, With<StreamedCellRoot>>();
        let interior_roots = roots
            .iter(world)
            .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
            .count();
        let mut form_ids = world.query::<&FormId>();
        let interior_references = form_ids
            .iter(world)
            .filter(|form_id| form_id.0 == STREAMING_FIXTURE_INTERIOR_REFERENCE_ID)
            .count();
        assert_eq!(interior_roots, 1);
        assert_eq!(interior_references, 1);

        let metrics = app.world().resource::<StreamingMetrics>();
        assert!(
            metrics.streaming_fixture_validated,
            "the fixture's own validation did not accept the run"
        );
        assert_eq!(metrics.duplicate_cell_roots, 0);
        assert_eq!(metrics.orphaned_cell_roots, 0);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
        assert!(metrics.unloaded_cells > 0);
        assert!(metrics.commit_frames > 0);
    }

    #[test]
    fn interior_crossing_fails_on_a_duplicated_or_mismatched_interior() {
        let crossed = InteriorCrossing {
            requested_frame: Some(STREAMING_FIXTURE_INTERIOR_FRAME),
            resident_frame: Some(40),
            away_samples: 12,
            min_away_roots: 1,
            max_away_roots: 1,
        };
        assert!(interior_crossing_valid(&crossed, 1, 1));
        // A second root for the same cell, or a second copy of its reference.
        assert!(!interior_crossing_valid(&crossed, 2, 1));
        assert!(!interior_crossing_valid(&crossed, 1, 2));
        // Never loaded.
        assert!(!interior_crossing_valid(&InteriorCrossing::default(), 1, 1));
        // Unloaded again is allowed: the contract does not require an interior to stay loaded.
        assert!(interior_crossing_valid(&crossed, 0, 0));
        // A reference left behind without its root, or a root without its reference.
        assert!(!interior_crossing_valid(&crossed, 0, 1));
        assert!(!interior_crossing_valid(&crossed, 1, 0));
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                requested_frame: Some(STREAMING_FIXTURE_INTERIOR_FRAME),
                ..default()
            },
            1,
            1
        ));
        // Gone on some frames while the camera was away is allowed too.
        assert!(interior_crossing_valid(
            &InteriorCrossing {
                min_away_roots: 0,
                ..crossed
            },
            1,
            1
        ));
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                max_away_roots: 2,
                ..crossed
            },
            1,
            1
        ));
        // No observation from outside the unload radius at all: the final count alone would pass
        // for an interior that was loaded and dropped again.
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                away_samples: 0,
                min_away_roots: 0,
                max_away_roots: 0,
                ..crossed
            },
            1,
            1
        ));
    }

    fn write_runtime_asset_database(path: &std::path::Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE schema_info(version INTEGER NOT NULL);
                 INSERT INTO schema_info VALUES({});
                 CREATE TABLE lod_chunks(id INTEGER PRIMARY KEY);",
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ))
            .unwrap();
    }

    #[test]
    fn runtime_converter_schema_matches_the_actual_producer() {
        assert_eq!(
            converter_schema_version(),
            converter::cache::CONVERTER_SCHEMA_VERSION
        );
    }

    /// The IO pool's stack has to hold a whole queue of loads nested inside one another, because a
    /// scope opened on a pool thread (bevy_gltf's texture scope) runs the other queued tasks on its
    /// own stack while it waits. This queues `LOADS` tasks behind a blocked single-thread pool,
    /// each of which takes a 64 KiB frame and then opens a scope, the shape of a glTF load. They
    /// nest far past the 8 MiB the pool used to have (the overflow seen streaming a dense city), and must finish
    /// on [`IO_TASK_STACK_BYTES`].
    #[test]
    fn the_io_pool_stack_holds_a_queue_of_loads_nested_in_scopes() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        const LOADS: usize = 300;
        const FRAME_BYTES: usize = 64 * 1024;

        fn load(pool: &bevy::tasks::TaskPool, depth: &AtomicUsize, deepest: &AtomicUsize) {
            let frame = [0u8; FRAME_BYTES];
            std::hint::black_box(&frame);
            let now = depth.fetch_add(1, Ordering::SeqCst) + 1;
            deepest.fetch_max(now, Ordering::SeqCst);
            pool.scope(|scope| scope.spawn(async {}));
            depth.fetch_sub(1, Ordering::SeqCst);
            std::hint::black_box(&frame);
        }

        let pool = Arc::new(io_task_pool_builder(1).build());
        let depth = Arc::new(AtomicUsize::new(0));
        let deepest = Arc::new(AtomicUsize::new(0));

        // Hold the only thread until every load is queued, so the nesting does not depend on
        // how fast this thread can spawn.
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocker = pool.spawn(async move {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        let tasks: Vec<_> = (0..LOADS)
            .map(|_| {
                let (pool_ref, depth, deepest) = (pool.clone(), depth.clone(), deepest.clone());
                pool.spawn(async move { load(&pool_ref, &depth, &deepest) })
            })
            .collect();
        release_tx.send(()).unwrap();
        bevy::tasks::block_on(blocker);
        for task in tasks {
            bevy::tasks::block_on(task);
        }

        let deepest = deepest.load(Ordering::SeqCst);
        assert!(
            deepest * FRAME_BYTES > 8 * 1024 * 1024,
            "the loads nested only {deepest} deep, too shallow to test the stack"
        );
    }

    #[test]
    fn rejects_conflicting_fixture_modes() {
        let mut config = EngineConfig {
            physics_fixture: true,
            ..Default::default()
        };
        assert!(validate_fixture_selection(&config).is_ok());
        config.streaming_fixture = true;
        assert!(validate_fixture_selection(&config).is_err());
    }

    #[test]
    fn screenshot_readiness_requires_resident_cells_only_with_world_streaming() {
        let metrics = StreamingMetrics::default();
        let config = EngineConfig::default();

        assert!(screenshot_assets_ready(&metrics, false, &config));
        assert!(!screenshot_assets_ready(&metrics, true, &config));

        let mut settled_metrics = metrics;
        settled_metrics.resident_cells = 1;
        assert!(screenshot_assets_ready(&settled_metrics, true, &config));

        let mut failed_metrics = settled_metrics.clone();
        failed_metrics.failed_cells = 1;
        assert!(!screenshot_assets_ready(&failed_metrics, true, &config));

        let mut pending_lod = settled_metrics.clone();
        pending_lod.pending_lod_queries = 1;
        assert!(!screenshot_assets_ready(&pending_lod, true, &config));

        let mut failed_lod = settled_metrics;
        failed_lod.failed_lod_chunks = 1;
        assert!(screenshot_assets_ready(&failed_lod, true, &config));
        failed_lod.unrecovered_lod_chunks = 1;
        assert!(!screenshot_assets_ready(&failed_lod, true, &config));
        failed_lod.unrecovered_lod_chunks = 0;
        failed_lod.unrecovered_lod_queries = 1;
        assert!(!screenshot_assets_ready(&failed_lod, true, &config));
    }

    /// Frames `capture_acceptance_screenshot` needs before the screenshot: the run's warmup plus
    /// its own settle frames.
    fn capture_warmup_frames() -> u32 {
        EngineConfig::default()
            .benchmark_warmup_frames
            .saturating_add(SCREENSHOT_SETTLE_FRAMES)
    }

    /// The real-time step that lets the frame gate open while the time gate stays shut: after
    /// `capture_warmup_frames` updates the warmup frames have passed and `SCREENSHOT_GPU_WARMUP`
    /// has not, so a capture that shows up later cannot be the frame gate's doing.
    fn capture_step_below_the_time_warmup() -> Duration {
        let step = SCREENSHOT_GPU_WARMUP / (capture_warmup_frames() + 1);
        assert!(step * capture_warmup_frames() < SCREENSHOT_GPU_WARMUP);
        step
    }

    /// The real capture system on the `Update` schedule every run gets, with everything else it
    /// reads otherwise ready: a primary window, a renderer on its final path, no pending or failed
    /// asset work, and the console plugin for its real input and close transition. Real time is
    /// stepped by `step` per update rather than slept through.
    fn acceptance_capture_app(directory: &Path, step: Duration) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, ConsolePlugin))
            .insert_resource(EngineConfig {
                acceptance_screenshot: Some(directory.join("acceptance.png")),
                ..default()
            })
            // Settled: nothing pending, nothing failed, a resident cell for the world case.
            .insert_resource(StreamingMetrics {
                resident_cells: 1,
                ..default()
            })
            // Every final-path flag, so `final_path_active` holds without a GPU.
            .insert_resource(RendererMetrics {
                gpu_preprocessing_active: true,
                gpu_culling_active: true,
                indirect_drawing_active: true,
                occlusion_culling_views: 1,
                hzb_views: 1,
                indirect_phase_buffers: 1,
                indirect_batch_sets: 1,
                proof_frames: 1,
                ..default()
            })
            .insert_resource(ProfilingState::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(step));
        add_camera_and_capture_systems(&mut app);
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app
    }

    /// One update of `step` of real time, the way a test moves the clock `Time<Real>` reads.
    fn step_capture(app: &mut App, step: Duration) {
        app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
        app.update();
    }

    /// A backquote press or release through the same message path the window sends, one update.
    fn send_backquote(app: &mut App, state: ButtonState, step: Duration) {
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Backquote,
            logical_key: Key::Character("`".into()),
            state,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        step_capture(app, step);
    }

    /// The `Screenshot` entities the capture system has queued for the renderer.
    fn queued_screenshots(app: &mut App) -> usize {
        let world = app.world_mut();
        let mut query = world.query_filtered::<(), With<Screenshot>>();
        query.iter(world).count()
    }

    fn console_state(app: &App) -> &ConsoleState {
        app.world().resource::<ConsoleState>()
    }

    /// The capture is queued once when the console is closed throughout, and not again after.
    #[test]
    fn acceptance_capture_is_queued_once_with_the_console_closed() {
        let directory = tempfile::tempdir().unwrap();
        let step = capture_step_below_the_time_warmup();
        let mut app = acceptance_capture_app(directory.path(), step);

        // The frame gate opens here and the time gate does not: the capture is held back still.
        for _ in 0..capture_warmup_frames() {
            step_capture(&mut app, step);
        }
        assert_eq!(
            queued_screenshots(&mut app),
            0,
            "the real-time warmup still holds the capture back"
        );

        step_capture(&mut app, SCREENSHOT_GPU_WARMUP);
        assert_eq!(
            queued_screenshots(&mut app),
            1,
            "a warm renderer with a closed console queues one capture"
        );

        for _ in 0..3 {
            step_capture(&mut app, SCREENSHOT_GPU_WARMUP);
        }
        assert_eq!(
            queued_screenshots(&mut app),
            1,
            "later updates do not queue a second capture"
        );
        // The PNG is written by the render world's observer, which needs a GPU: this test pins the
        // queue decision the schedule makes, not the write.
        assert!(!directory.path().join("acceptance.png").exists());
    }

    /// The console's real close transition: no capture while it is open or on the closing frame,
    /// exactly one on the next update, and no duplicates after that.
    #[test]
    fn acceptance_capture_waits_for_the_console_to_close() {
        let directory = tempfile::tempdir().unwrap();
        let step = capture_step_below_the_time_warmup();
        let mut app = acceptance_capture_app(directory.path(), step);

        // Both other gates open before the console is: from here every frame the capture is held
        // back on has the warmup frames and the warmup time behind it.
        for _ in 0..capture_warmup_frames() {
            step_capture(&mut app, step);
        }
        assert_eq!(queued_screenshots(&mut app), 0);

        // Open the console the way the game does: a backquote through Bevy's input.
        send_backquote(&mut app, ButtonState::Pressed, SCREENSHOT_GPU_WARMUP);
        assert!(console_state(&app).open);
        send_backquote(&mut app, ButtonState::Released, SCREENSHOT_GPU_WARMUP);

        // Real time runs well past the warmup while the console is open, so the capture is held
        // back by the gate and not by the renderer still warming up.
        for _ in 0..3 {
            step_capture(&mut app, SCREENSHOT_GPU_WARMUP);
        }
        assert_eq!(
            queued_screenshots(&mut app),
            0,
            "no capture is queued while the console is open"
        );

        // Close it: the frame the close lands on still carries `closing`, and is rejected too.
        send_backquote(&mut app, ButtonState::Pressed, SCREENSHOT_GPU_WARMUP);
        let state = console_state(&app);
        assert!(
            !state.open && state.closing,
            "the close lands on the closing frame"
        );
        assert_eq!(
            queued_screenshots(&mut app),
            0,
            "the closing frame is not a closed console"
        );

        // The next update clears `closing` in `PreUpdate`, and the capture resumes.
        send_backquote(&mut app, ButtonState::Released, SCREENSHOT_GPU_WARMUP);
        assert_eq!(
            queued_screenshots(&mut app),
            1,
            "the update after the closing frame queues exactly one capture"
        );

        for _ in 0..3 {
            step_capture(&mut app, SCREENSHOT_GPU_WARMUP);
        }
        assert_eq!(
            queued_screenshots(&mut app),
            1,
            "later updates do not queue a second capture"
        );
    }
    /// The sun's shadows reach the grid the streamer draws, whichever way the camera faces, and the
    /// shader's view of them is usable: far bounds that increase (the shader takes the first bound a
    /// fragment is inside, so a bound out of order hands everything beyond it to a cascade that
    /// cannot see it) and a near clamp below the first far bound. The two near distances are Bevy's
    /// defaults read in metres; the last is this engine's drawn grid.
    #[test]
    fn the_sun_shadows_cover_the_drawn_grid() {
        let config = EngineConfig::default();
        let cascades = sun_shadow_cascades(&config);
        let cell = crate::world::components::CELL_SIZE;
        assert_eq!(config.stream_radius, 2);
        // Cells are requested inside the stream radius and kept until they leave the unload ring,
        // one wider, so the unload ring is the grid that keeps drawing.
        assert_eq!(config.unload_radius, 3);
        assert_eq!(cascades.bounds.len(), SUN_SHADOW_CASCADES);

        assert_eq!(
            cascades.minimum_distance,
            0.1 * CREATION_UNITS_PER_METRE,
            "Bevy's 10 cm near clamp, in Creation units"
        );
        assert_eq!(
            cascades.bounds[0],
            10.0 * CREATION_UNITS_PER_METRE,
            "and its 10 m first cascade, which is 700 units here"
        );
        assert_eq!(
            cascades.overlap_proportion,
            CascadeShadowConfigBuilder::default().overlap_proportion,
            "the cascade overlap is the builder's default, not a number of this engine's"
        );

        // The camera stands somewhere inside its own cell and cells stay resident - and keep
        // drawing - out to `unload_radius` cells around that cell, so the grid is
        // `unload_radius + 1` cells from the camera to its edge and sqrt(2) times that to its far
        // corner.
        let axis = cell * (config.unload_radius as f32 + 1.0);
        let corner = axis * std::f32::consts::SQRT_2;
        let reach = corner.hypot(camera_offset(&config).y);
        assert!(
            cascades.bounds[3] > axis,
            "the grid straight ahead is shadowed: {} covers {axis}",
            cascades.bounds[3]
        );
        // `calculate_cascade_bounds` reaches the maximum distance geometrically, so the last bound
        // is the requested distance to within a rounding error rather than to the bit.
        assert!(
            (cascades.bounds[3] - reach).abs() < 1.0,
            "and the far corner is inside the last cascade too: {} against {reach}",
            cascades.bounds[3]
        );
        // The requested radius is not the drawn grid: a range fitted to `stream_radius` alone would
        // leave the ring the streamer keeps beyond it lit flat.
        let requested = cell * (config.stream_radius as f32 + 1.0) * std::f32::consts::SQRT_2;
        assert!(
            cascades.bounds[3] > requested,
            "the unload ring is covered as well: {} against {requested}",
            cascades.bounds[3]
        );

        assert!(
            cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]),
            "the far bounds increase: {:?}",
            cascades.bounds
        );
        assert!(cascades.minimum_distance < cascades.bounds[0]);

        // "Not metre-scale" is what this fix is for: the first cascade on its own reaches further
        // than everything Bevy's default covered, 150 of its world units away.
        let metre_scale = CascadeShadowConfig::default();
        assert!(
            cascades.bounds[0] > *metre_scale.bounds.last().unwrap(),
            "a first cascade of {} units against Bevy's whole default range of {:?}",
            cascades.bounds[0],
            metre_scale.bounds
        );
    }

    #[test]
    fn the_skyrim_ini_shadow_distance_caps_the_grid_range() {
        let cap = EngineConfig {
            sun_shadow_distance_limit: Some(8_000.0),
            ..EngineConfig::default()
        };
        let cascades = sun_shadow_cascades(&cap);
        assert!((cascades.bounds[3] - 8_000.0).abs() < 0.01);
        assert!(cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]));

        let max_grid = EngineConfig {
            sun_shadow_distance_limit: Some(23_200.0),
            ..EngineConfig::default()
        };
        let cascades = sun_shadow_cascades(&max_grid);
        assert!((cascades.bounds[3] - 23_200.0).abs() < 0.01);

        let short_range = EngineConfig {
            sun_shadow_distance_limit: Some(700.0),
            ..EngineConfig::default()
        };
        let cascades = sun_shadow_cascades(&short_range);
        assert!((cascades.bounds[3] - 700.0).abs() < 0.01);
        assert!(cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn sun_position_maps_clockwise_bearing_and_elevation_to_light_rays() {
        for (azimuth, elevation, expected) in [
            (0.0, 0.0, Vec3::Z),
            (90.0, 0.0, Vec3::NEG_X),
            (180.0, 0.0, Vec3::NEG_Z),
            (270.0, 0.0, Vec3::X),
            (0.0, 90.0, Vec3::NEG_Y),
        ] {
            let config = EngineConfig {
                sun_position: Some((azimuth, elevation)),
                ..EngineConfig::default()
            };
            let rays = sun_rotation(&config) * Vec3::NEG_Z;
            assert!(
                rays.distance(expected) < 1.0e-5,
                "{azimuth},{elevation}: {rays:?}"
            );
        }
    }

    #[test]
    fn absent_sun_position_preserves_the_existing_light_rotation() {
        assert_eq!(
            sun_rotation(&EngineConfig::default()),
            Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)
        );
    }

    /// The range is the camera's own distance to the far corner of the drawn grid, not the corner's
    /// distance from the grid centre: the acceptance screenshot camera is 20,000 units above the
    /// grid it looks at, and its shadows have to reach as far down and out as it looks.
    #[test]
    fn the_shadow_range_counts_the_camera_height() {
        let overhead = EngineConfig {
            acceptance_screenshot: Some(std::path::PathBuf::from("acceptance.png")),
            ..EngineConfig::default()
        };
        let cell = crate::world::components::CELL_SIZE;
        let corner = cell * (overhead.unload_radius as f32 + 1.0) * std::f32::consts::SQRT_2;
        let walk_around = corner.hypot(camera_offset(&EngineConfig::default()).y);
        let above = corner.hypot(camera_offset(&overhead).y);
        assert!(above > walk_around, "the camera's height counts");

        let cascades = sun_shadow_cascades(&overhead);
        assert!(
            (cascades.bounds[3] - above).abs() < 1.0,
            "the overhead camera's range reaches its own corner: {} against {above}",
            cascades.bounds[3]
        );

        let walk_around_cascades = sun_shadow_cascades(&EngineConfig::default());
        assert!(
            (walk_around_cascades.bounds[3] - walk_around).abs() < 1.0,
            "and the walk-around camera's reaches its: {} against {walk_around}",
            walk_around_cascades.bounds[3]
        );
    }

    /// The range follows the grid the streamer draws: at each stream radius the command line takes,
    /// the far corner of the unload ring is inside the last cascade, while the two near distances
    /// stay Bevy's defaults in Creation units rather than following the grid.
    #[test]
    fn the_shadow_range_follows_the_drawn_grid() {
        let cell = crate::world::components::CELL_SIZE;
        let mut previous = 0.0;
        for radius in [0, 1, 2, 4, 8, 16] {
            // `--stream-radius` takes one radius and keeps cells a ring wider than it.
            let args = ["--stream-radius".to_owned(), radius.to_string()];
            let config = EngineConfig::run_from_args(args);
            assert_eq!(
                (config.stream_radius, config.unload_radius),
                (radius, radius + 1)
            );
            let cascades = sun_shadow_cascades(&config);
            assert_eq!(cascades.bounds.len(), SUN_SHADOW_CASCADES);
            assert_eq!(
                cascades.bounds[0],
                10.0 * CREATION_UNITS_PER_METRE,
                "the first cascade is set in metres, not by the grid, at radius {radius}"
            );
            assert!(cascades.minimum_distance < cascades.bounds[0]);
            assert!(
                cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]),
                "the far bounds increase at radius {radius}: {:?}",
                cascades.bounds
            );

            // The unload ring is what is still drawn: one ring wider than `stream_radius`.
            let axis = cell * (config.unload_radius as f32 + 1.0);
            let corner = axis * std::f32::consts::SQRT_2;
            let reach = corner.hypot(camera_offset(&config).y);
            assert!(
                cascades.bounds[3] > axis,
                "radius {radius} shadows the grid straight ahead: {} covers {axis}",
                cascades.bounds[3]
            );
            assert!(
                (cascades.bounds[3] - reach).abs() < 1.0,
                "radius {radius} reaches the far corner: {} against {reach}",
                cascades.bounds[3]
            );
            assert!(
                cascades.bounds[3] > previous,
                "a wider grid is more world to shadow"
            );
            previous = cascades.bounds[3];
        }

        // The command line refuses a negative stream radius, but a configuration built in code can
        // still hold one. It streams nothing, and the range derived from it would fall under the
        // first cascade's far bound - which `CascadeShadowConfigBuilder::build` rejects by panic.
        // The engine clamps it and starts.
        let nothing = EngineConfig {
            stream_radius: -4,
            unload_radius: -3,
            ..EngineConfig::default()
        };
        let cascades = sun_shadow_cascades(&nothing);
        assert!(cascades.bounds[3] > 10.0 * CREATION_UNITS_PER_METRE);
        assert!(cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]));

        // A radius no engine could stream is capped rather than asked for: the range is fitted to
        // the widest grid `sun_shadow_cascades` will fit one to.
        let args = ["--stream-radius".to_owned(), "100000".to_owned()];
        let gigametres = EngineConfig::run_from_args(args);
        let widest = cell * SUN_SHADOW_MAX_GRID_CELLS as f32 * std::f32::consts::SQRT_2;
        let reach = widest.hypot(camera_offset(&gigametres).y);
        let cascades = sun_shadow_cascades(&gigametres);
        assert!(
            (cascades.bounds[3] - reach).abs() < 1.0,
            "a radius of 100000 cells is capped at {SUN_SHADOW_MAX_GRID_CELLS}: {} against {reach}",
            cascades.bounds[3]
        );
    }

    #[test]
    fn v1_world_and_visual_fixture_cameras_have_explicit_hdr_output() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<TerrainMaterial>>()
            .init_resource::<Assets<WaterMaterial>>()
            .insert_resource(WaterReflectionTexture(Handle::default()))
            .add_systems(
                Startup,
                (
                    setup_world,
                    setup_material_fixture,
                    setup_terrain_water_fixture,
                    setup_transform_bounds_fixture,
                    setup_renderer_fixture,
                ),
            );
        app.update();
        crate::color_pipeline::assert_scene_camera_output(app.world_mut(), 5);
    }

    /// The engine's startup path is `setup_world`, not the helper above, so the sun it spawns is
    /// what has to carry the cascades - along with the shadow map size they are drawn at.
    #[test]
    fn setup_world_spawns_the_sun_with_its_cascades() {
        let config = EngineConfig::default();
        let mut app = App::new();
        app.insert_resource(config.clone())
            .add_systems(Startup, setup_world);
        app.update();

        let world = app.world_mut();
        let mut suns = world.query::<(&DirectionalLight, &CascadeShadowConfig)>();
        let Ok((light, cascades)) = suns.single(world) else {
            panic!("setup_world spawns one shadow-casting directional light");
        };
        assert!(light.shadow_maps_enabled);

        let expected = sun_shadow_cascades(&config);
        assert_eq!(cascades.bounds, expected.bounds);
        assert_eq!(cascades.minimum_distance, expected.minimum_distance);

        // `resource` panics if `setup_world` left the shadow map size unset, which is one of the
        // two ways this test can fail.
        let shadow_map = world.resource::<DirectionalLightShadowMap>();
        assert_eq!(shadow_map.size, SUN_SHADOW_MAP_SIZE);
    }

    /// A placed object is kept out of the reflection pass by its layer, so the streaming camera has
    /// to keep drawing that layer or the objects would vanish from the player's own view.
    #[test]
    fn the_streaming_camera_renders_every_layer_the_world_uses() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .add_systems(Startup, setup_world);
        app.update();

        let mut cameras = app
            .world_mut()
            .query_filtered::<&RenderLayers, With<StreamingCamera>>();
        let layers = cameras
            .single(app.world())
            .expect("the streaming camera must be spawned");
        for layer in [
            crate::render::WORLD_LAYER,
            WATER_LAYER,
            crate::render::PLACED_OBJECT_LAYER,
        ] {
            assert!(
                layers.intersects(&RenderLayers::layer(layer)),
                "the streaming camera must render layer {layer}"
            );
        }
    }

    #[test]
    fn live_start_pose_seeds_streaming_and_camera_without_selecting_shots_mode() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("pose.json");
        std::fs::write(&file, r#"{"width":1920,"height":1080,"shots":[{"name":"inn","worldspace_id":60,"position":[21750,-44530,140],"yaw":90,"pitch":-3,"hfov":80}]}"#).unwrap();
        let mut config = EngineConfig {
            start_shot: Some(file),
            ..default()
        };
        let pose = prepare_start_camera(&mut config).unwrap().unwrap();
        assert_eq!(config.start_grid, (5, -11));
        assert_eq!(config.worldspace_id, 60);
        assert!(config.interactive_world_physics());
        let mut app = App::new();
        app.insert_resource(config)
            .insert_resource(pose)
            .add_systems(Startup, setup_world);
        app.update();
        let mut cameras = app
            .world_mut()
            .query_filtered::<(&Transform, &Projection), With<StreamingCamera>>();
        let (transform, projection) = cameras.single(app.world()).unwrap();
        assert_eq!(
            transform.translation,
            crate::shots::shot_camera_translation([21750.0, -44530.0, 140.0], IVec2::new(5, -11))
        );
        assert_eq!(
            transform.rotation,
            crate::shots::shot_camera_rotation(90.0, -3.0)
        );
        let Projection::Perspective(projection) = projection else {
            panic!("perspective camera")
        };
        assert_eq!(
            projection.fov,
            crate::shots::vertical_fov_degrees(80.0, 1920.0 / 1080.0).to_radians()
        );
        assert!(app.world().get_resource::<ShotsRun>().is_none());
    }

    #[test]
    fn shots_camera_uses_capture_image_and_drives_shadow_lod() {
        let mut app = App::new();
        let output = Handle::<Image>::default();
        app.insert_resource(EngineConfig::default())
            .insert_resource(ShotOutputImage(output.clone()))
            .add_systems(Startup, setup_world);
        app.update();
        let mut query = app.world_mut().query_filtered::<(
            &bevy::camera::RenderTarget,
            Has<bevy::camera::ShadowLodOrigin>,
        ), With<StreamingCamera>>();
        let (target, shadow_origin) = query.single(app.world()).unwrap();
        assert!(
            matches!(target, bevy::camera::RenderTarget::Image(image) if image.handle == output)
        );
        assert!(shadow_origin);
    }

    #[test]
    fn automatic_flight_reverses_before_leaving_the_representative_world_area() {
        let mut state = AutoFlightState::default();
        let direction = bounded_auto_flight_direction(Vec3::new(0.0, -1.0, -1.0), 1.0, &mut state);
        assert_eq!(direction, Vec3::NEG_Z);

        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, AUTO_FLIGHT_HALF_SPAN, &mut state),
            Vec3::Z
        );
        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, 2.0, &mut state),
            Vec3::NEG_Z
        );
        assert!(state.offset.abs() <= AUTO_FLIGHT_HALF_SPAN);
    }

    #[test]
    fn the_upload_budget_resource_carries_the_configured_option() {
        let mut app = App::new();
        app.insert_resource(upload_budget(&EngineConfig::default()));
        assert_eq!(
            app.world().resource::<RenderAssetBytesPerFrame>().max_bytes,
            Some(16 * 1024 * 1024)
        );

        let unlimited = EngineConfig {
            max_upload_mib_per_frame: 0,
            ..default()
        };
        app.insert_resource(upload_budget(&unlimited));
        assert_eq!(
            app.world().resource::<RenderAssetBytesPerFrame>().max_bytes,
            None
        );
    }

    /// An assets directory the engine was given.
    const EXAMPLE_ASSETS: &str = "converted-assets";

    fn asset_set_message(rejection: AssetSetRejection) -> String {
        asset_set_rejection_message(Path::new(EXAMPLE_ASSETS), rejection)
    }

    /// Runs the real gate against a temporary asset set and returns its error.
    fn runtime_asset_error(manifest: &str, report: &str) -> String {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            manifest.as_bytes(),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            report.as_bytes(),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        format!("{:#}", validate_runtime_assets(&config).unwrap_err())
    }

    #[test]
    fn rejects_stale_or_incomplete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        write_runtime_asset_database(&directory.path().join("skyrim_world.db"));
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"schema_version":3,"complete":true}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":3,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        assert!(validate_runtime_assets(&config).is_err());
    }

    #[test]
    fn older_converter_schema_names_the_accepted_range_and_the_reconvert_command() {
        let oldest = shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        let newest = converter_schema_version();
        let message =
            asset_set_message(AssetSetRejection::ConverterSchemaOlder { found: oldest - 1 });
        assert!(message.contains("are stale"), "{message}");
        assert!(
            message.contains(&format!("was written by converter schema {}", oldest - 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "accepts converter schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
        assert!(message.contains("reuses what it can"), "{message}");
    }

    #[test]
    fn newer_converter_schema_names_the_accepted_range_and_the_engine_rebuild() {
        let oldest = shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        let newest = converter_schema_version();
        let message =
            asset_set_message(AssetSetRejection::ConverterSchemaNewer { found: newest + 1 });
        assert!(message.contains("newer than this engine"), "{message}");
        assert!(
            message.contains(&format!("was written by converter schema {}", newest + 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "understands only converter schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains("cargo build --release -p engine"),
            "{message}"
        );
    }

    #[test]
    fn incomplete_conversion_names_the_rerun_and_the_incomplete_assets_option() {
        let engine = converter_schema_version();
        let message = asset_set_message(AssetSetRejection::IncompleteConversion { schema: engine });
        assert!(message.contains("did not finish"), "{message}");
        assert!(message.contains("complete=false"), "{message}");
        assert!(
            message.contains(&format!("at converter schema {engine}")),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
        assert!(message.contains("--allow-incomplete-assets"), "{message}");
    }

    #[test]
    fn incomplete_conversion_names_the_schema_the_manifest_reports() {
        let oldest = shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        assert_ne!(oldest, converter_schema_version());
        let message = runtime_asset_error(
            &format!(r#"{{"schema_version":{oldest},"complete":false}}"#),
            &format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ),
        );
        assert!(
            message.contains(&format!("complete=false at converter schema {oldest},")),
            "{message}"
        );
    }

    #[test]
    fn world_database_schema_mismatch_names_the_accepted_range_and_the_reconvert_command() {
        let oldest = shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION;
        let newest = shared::WORLD_DATABASE_SCHEMA_VERSION;
        let message =
            asset_set_message(AssetSetRejection::WorldDatabaseSchema { found: oldest - 1 });
        assert!(message.contains("world database schema"), "{message}");
        assert!(
            message.contains(&format!("world database schema {}", oldest - 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "accepts world database schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
    }

    #[test]
    fn failed_integration_report_points_at_the_report_file() {
        let message = asset_set_message(AssetSetRejection::IntegrationReportFailed);
        assert!(
            message.contains("integration report did not pass"),
            "{message}"
        );
        assert!(message.contains("passed=false"), "{message}");
        assert!(message.contains("integration-report.json"), "{message}");
        assert!(message.contains(EXAMPLE_ASSETS), "{message}");
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
    }

    #[test]
    fn stale_incomplete_and_failed_runtime_assets_report_distinct_reasons() {
        let engine = converter_schema_version();
        let world = shared::WORLD_DATABASE_SCHEMA_VERSION;
        let passing_report = format!(r#"{{"schema_version":{world},"passed":true}}"#);
        let stale =
            runtime_asset_error(r#"{"schema_version":14,"complete":true}"#, &passing_report);
        let incomplete = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":false}}"#),
            &passing_report,
        );
        let failed_report = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":true}}"#),
            &format!(r#"{{"schema_version":{world},"passed":false}}"#),
        );
        let schema_report = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":true}}"#),
            &format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION - 1
            ),
        );

        assert!(stale.contains("converted assets are stale"), "{stale}");
        assert!(
            incomplete.contains("--allow-incomplete-assets"),
            "{incomplete}"
        );
        assert!(failed_report.contains("passed=false"), "{failed_report}");
        assert!(
            schema_report.contains("world database schema"),
            "{schema_report}"
        );
        for (left, right) in [
            (&stale, &incomplete),
            (&stale, &failed_report),
            (&stale, &schema_report),
            (&incomplete, &failed_report),
            (&incomplete, &schema_report),
            (&failed_report, &schema_report),
        ] {
            assert_ne!(left, right);
        }
    }

    /// Current, complete assets load, and an integration report newer than the engine is rejected.
    #[test]
    fn accepts_current_complete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        write_runtime_asset_database(&directory.path().join("skyrim_world.db"));
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            format!(
                r#"{{"schema_version":{},"complete":true}}"#,
                converter_schema_version()
            ),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();
        assert!(
            validate_runtime_assets(&config)
                .unwrap_err()
                .to_string()
                .contains(&format!(
                    "schema {} is unsupported",
                    shared::WORLD_DATABASE_SCHEMA_VERSION + 1
                ))
        );
    }

    #[test]
    fn accepts_schema_15_assets_with_matching_legacy_reports() {
        // Cover both legacy world schemas and retain the report/database mismatch gate.
        for schema in [3, 4] {
            let directory = tempfile::tempdir().unwrap();
            let database = Connection::open(directory.path().join("skyrim_world.db")).unwrap();
            database.execute_batch(&format!("CREATE TABLE schema_info(version INTEGER NOT NULL); INSERT INTO schema_info VALUES({schema});")).unwrap();
            std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
            std::fs::write(
                directory.path().join("conversion-manifest.json"),
                br#"{"schema_version":15,"complete":true}"#,
            )
            .unwrap();
            let report = directory.path().join("integration-report.json");
            std::fs::write(
                &report,
                format!(r#"{{"schema_version":{schema},"passed":true}}"#),
            )
            .unwrap();
            let config = EngineConfig {
                assets_dir: directory.path().to_owned(),
                ..default()
            };
            validate_runtime_assets(&config).unwrap();
            let wrong = schema + 1;
            std::fs::write(
                &report,
                format!(r#"{{"schema_version":{wrong},"passed":true}}"#),
            )
            .unwrap();
            assert!(
                validate_runtime_assets(&config)
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("does not match integration report schema {wrong}"))
            );
        }
    }

    #[test]
    fn v7_accepts_unchanged_schema_16_and_new_emission_schema_17() {
        assert_eq!(
            converter_schema_version(),
            converter::cache::CONVERTER_SCHEMA_VERSION
        );
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        let database = Connection::open(directory.path().join("skyrim_world.db")).unwrap();
        database.execute_batch("CREATE TABLE schema_info(version INTEGER NOT NULL); INSERT INTO schema_info VALUES(4);").unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":4,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        for schema in [16, 17, 18, 19, 20, 21, 22, 23, 24] {
            std::fs::write(
                directory.path().join("conversion-manifest.json"),
                format!(r#"{{"schema_version":{schema},"complete":true}}"#),
            )
            .unwrap();
            validate_runtime_assets(&config).unwrap();
        }
    }

    #[test]
    fn rejects_truncated_manifest_and_integration_report() {
        for truncated_file in ["conversion-manifest.json", "integration-report.json"] {
            let directory = tempfile::tempdir().unwrap();
            write_runtime_asset_database(&directory.path().join("skyrim_world.db"));
            std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
            std::fs::write(
                directory.path().join("conversion-manifest.json"),
                format!(
                    r#"{{"schema_version":{},"complete":true}}"#,
                    converter_schema_version()
                ),
            )
            .unwrap();
            std::fs::write(
                directory.path().join("integration-report.json"),
                format!(
                    r#"{{"schema_version":{},"passed":true}}"#,
                    shared::WORLD_DATABASE_SCHEMA_VERSION
                ),
            )
            .unwrap();
            std::fs::write(directory.path().join(truncated_file), b"{").unwrap();
            let config = EngineConfig {
                assets_dir: directory.path().to_owned(),
                ..default()
            };
            assert!(
                validate_runtime_assets(&config).is_err(),
                "{truncated_file}"
            );
        }
    }

    #[test]
    fn ground_height_query_reads_the_right_cell() {
        let database_directory = tempfile::tempdir().unwrap();
        let database_path = database_directory.path().join("skyrim_world.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                 CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                 INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(42,7,0,0);
                 INSERT INTO land(cell_id) VALUES(42);",
            )
            .unwrap();
        drop(connection);

        let cache_directory = tempfile::tempdir().unwrap();
        let cache_path = cache_directory.path().join("cell_cache.rkyv");
        let source = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id: 42,
                width: 2,
                height: 2,
                heights: vec![1.0, 2.0, 3.0, 4.0],
                normals: vec![0; 12],
                vertex_colors: vec![255; 12],
                layers: vec![],
                water_height: None,
                water_type_form_id: None,
            }],
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&source).unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        let cache = CellCache::open(&cache_path).unwrap();

        let config = EngineConfig {
            worldspace_id: 7,
            start_grid: (0, 0),
            ..default()
        };

        let ground_height = initial_camera_ground_height(&config, &database_path, &cache).unwrap();
        assert_eq!(ground_height, 4.0);
    }

    #[test]
    fn walk_spawn_ground_query_interpolates_at_authored_shot_xy() {
        let database_directory = tempfile::tempdir().unwrap();
        let database_path = database_directory.path().join("skyrim_world.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                 CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                 INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(42,7,0,0);
                 INSERT INTO land(cell_id) VALUES(42);",
            )
            .unwrap();
        drop(connection);

        let cache_directory = tempfile::tempdir().unwrap();
        let cache_path = cache_directory.path().join("cell_cache.rkyv");
        let heights = (0..33)
            .flat_map(|y| (0..33).map(move |x| (x + y) as f32))
            .collect();
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id: 42,
                width: 33,
                height: 33,
                heights,
                normals: vec![0; 33 * 33 * 3],
                vertex_colors: vec![255; 33 * 33 * 3],
                layers: vec![],
                water_height: None,
                water_type_form_id: None,
            }],
        })
        .unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        let cache = CellCache::open(&cache_path).unwrap();
        let config = EngineConfig {
            worldspace_id: 7,
            start_grid: (0, 0),
            ..default()
        };
        let shot = crate::shots::Shot {
            name: "elevated".to_owned(),
            worldspace_id: Some(7),
            interior_cell_id: None,
            position: [
                crate::world::components::CELL_SIZE * 0.25,
                crate::world::components::CELL_SIZE * 0.75,
                5_000.0,
            ],
            yaw: 123.0,
            pitch: -17.0,
            hfov: 60.0,
            reference: None,
            note: None,
            probes: Vec::new(),
        };

        assert_eq!(
            initial_walk_ground_height(&config, &database_path, &cache, Some(&shot)).unwrap(),
            Some(32.0)
        );
    }

    #[test]
    fn ground_height_query_does_not_create_a_missing_database() {
        // The old code opened with `Connection::open`, which is
        // read-write-and-create-if-missing: querying a database that does not
        // exist yet silently created an empty one as a side effect. An
        // explicit read-only open must instead fail without creating
        // anything.
        let database_directory = tempfile::tempdir().unwrap();
        let database_path = database_directory.path().join("skyrim_world.db");
        assert!(!database_path.exists());

        let cache_directory = tempfile::tempdir().unwrap();
        let cache_path = cache_directory.path().join("cell_cache.rkyv");
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![],
        })
        .unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        let cache = CellCache::open(&cache_path).unwrap();

        let config = EngineConfig {
            worldspace_id: 7,
            start_grid: (0, 0),
            ..default()
        };

        assert!(initial_camera_ground_height(&config, &database_path, &cache).is_err());
        assert!(
            !database_path.exists(),
            "a read-only ground-height query must not create a missing database file"
        );
    }

    /// The terrain/water fixture is the only scene that draws terrain without game data, so its
    /// materials have to carry the overlay weight field the streamed path reads: on the packed
    /// vertex attributes it would show the sharpened carrier instead, and the field would go
    /// unexercised outside a converted asset set.
    #[test]
    fn terrain_water_fixture_quadrants_carry_their_weight_field() {
        let terrain = terrain_water_fixture_snapshot();
        for quadrant in 0..4 {
            let extension = TerrainExtension::fixture(
                &terrain,
                quadrant,
                std::array::from_fn(|_| Handle::<Image>::default()),
            )
            .expect("the canonical terrain fixture must build");
            assert!(
                extension.reads_weight_field(),
                "quadrant {quadrant} must render its overlays through the weight field"
            );
        }
    }

    /// The shader tiles every terrain layer across a cell, so the fixture's layer images
    /// need the sampler a streamed layer is loaded with. Bevy's default clamps to the edge, which
    /// stretches the outermost texels over the rest of the tiles.
    #[test]
    fn terrain_water_fixture_layers_are_sampled_with_a_repeating_sampler() {
        let expected = bevy::image::ImageSampler::Descriptor(terrain_layer_sampler());
        assert_eq!(terrain_fixture_image([82, 116, 58, 255]).sampler, expected);
    }
    #[test]
    fn fly_camera_stands_still_while_the_console_is_open() {
        use crate::console::ConsoleState;
        use bevy::time::TimeUpdateStrategy;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ProfilingState>()
            .insert_resource(EngineConfig {
                headless: true,
                ..default()
            })
            .insert_resource(ConsoleState {
                open: true,
                ..default()
            })
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(100),
            ))
            .add_systems(Update, fly_camera.run_if(console_closed));
        let camera = app
            .world_mut()
            .spawn((StreamingCamera, Transform::default()))
            .id();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Space);
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            app.world().get::<Transform>(camera).unwrap().translation,
            Vec3::ZERO
        );
        app.world_mut().resource_mut::<ConsoleState>().open = false;
        for _ in 0..3 {
            app.update();
        }
        assert!(app.world().get::<Transform>(camera).unwrap().translation.y > 0.0);
    }
}
