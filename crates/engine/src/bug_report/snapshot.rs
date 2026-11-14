//! Bounded, allowlisted runtime context. Only `content_metadata` performs IO, on a worker.

use crate::{
    config::EngineConfig,
    physics::{MoveMode, PlayerBody},
    render::RendererMetrics,
    streaming::{RenderOrigin, StreamingMetrics},
    world::components::{CELL_SIZE, StreamingCamera, WaterSurface},
};
use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    diagnostic::{DiagnosticsStore, FrameCount, SystemInfo, SystemInformationDiagnosticsPlugin},
    prelude::*,
    render::{renderer::RenderAdapterInfo, view::Msaa},
    window::PrimaryWindow,
};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value, json};
use std::{collections::VecDeque, fs, io::Read, path::Path, time::Duration};

const MAX_FIELD_BYTES: usize = 1024;
const MAX_FAILURES: usize = 32;
const MAX_DEPENDENCIES: usize = 16;
const MAX_FRAME_SAMPLES: usize = 2048;
const FRAME_WINDOW_SECONDS: f64 = 10.0;
const METADATA_READ_LIMIT: u64 = 256 * 1024;

#[derive(Resource, Default)]
pub(super) struct FrameHistory {
    samples: VecDeque<(f64, f64)>,
    invalid_samples: u64,
    capacity_evictions: u64,
}

/// Real-time main-world update intervals, including paused sessions. No benchmark is enabled.
pub(super) fn sample_frame(time: Res<Time<Real>>, mut history: ResMut<FrameHistory>) {
    history.record(time.elapsed_secs_f64(), time.delta_secs_f64() * 1000.0);
}

impl FrameHistory {
    fn record(&mut self, elapsed: f64, milliseconds: f64) {
        if !elapsed.is_finite() {
            self.invalid_samples = self.invalid_samples.saturating_add(1);
            return;
        }
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| *at < elapsed - FRAME_WINDOW_SECONDS)
        {
            self.samples.pop_front();
        }
        if !milliseconds.is_finite() || milliseconds <= 0.0 {
            self.invalid_samples = self.invalid_samples.saturating_add(1);
            return;
        }
        if self.samples.len() == MAX_FRAME_SAMPLES {
            self.samples.pop_front();
            self.capacity_evictions = self.capacity_evictions.saturating_add(1);
        }
        self.samples.push_back((elapsed, milliseconds));
    }

    fn snapshot(&self) -> Value {
        if self.samples.is_empty() {
            return json!({"sample_count": 0, "frame_ms": null,
                "unavailable": {"frame_ms": "no positive real-time frame interval measured"},
                "invalid_samples": self.invalid_samples});
        }
        let mut ordered: Vec<f64> = self
            .samples
            .iter()
            .map(|(_, milliseconds)| *milliseconds)
            .collect();
        ordered.sort_by(f64::total_cmp);
        let percentile =
            |quantile: f64| ordered[((ordered.len() - 1) as f64 * quantile).ceil() as usize];
        json!({
            "scope": "main-world real-time update interval; includes rendering waits and paused sessions",
            "performance_acceptance": false,
            "sample_count": ordered.len(),
            "sample_interval_seconds": self.samples.back().unwrap().0 - self.samples.front().unwrap().0,
            "window_seconds": FRAME_WINDOW_SECONDS,
            "sample_limit": MAX_FRAME_SAMPLES,
            "frame_ms": {"mean": ordered.iter().map(|value| value / ordered.len() as f64).sum::<f64>(),
                "p50": percentile(0.50), "p95": percentile(0.95), "p99": percentile(0.99),
                "max": ordered.last()},
            "invalid_samples": self.invalid_samples,
            "capacity_evictions": self.capacity_evictions,
        })
    }
}

#[derive(Default)]
struct Section {
    fields: Map<String, Value>,
    unavailable: Map<String, Value>,
    truncated: Vec<String>,
}

impl Section {
    fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.fields.insert(key.to_owned(), value.into());
    }
    fn unknown(&mut self, key: &str, reason: &str) {
        self.set(key, Value::Null);
        self.unavailable
            .insert(key.to_owned(), Value::String(reason.into()));
    }
    fn text(&mut self, key: &str, value: &str) {
        if value.len() > MAX_FIELD_BYTES {
            self.truncated.push(key.to_owned());
        }
        self.set(key, truncate(value, MAX_FIELD_BYTES));
    }
    fn numeric(&mut self, key: &str, value: f64) {
        if let Some(value) = serde_json::Number::from_f64(value) {
            self.set(key, Value::Number(value));
        } else {
            self.unknown(key, "non-finite measurement");
        }
    }
    fn finish(mut self) -> Value {
        if !self.unavailable.is_empty() {
            let unavailable = std::mem::take(&mut self.unavailable);
            self.set("unavailable", Value::Object(unavailable));
        }
        if !self.truncated.is_empty() {
            self.set("truncated_fields", json!(self.truncated));
        }
        Value::Object(self.fields)
    }
}

fn truncate(text: &str, maximum: usize) -> String {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Export only reviewed fields, never the whole world or EngineConfig. No filesystem/Git IO.
pub(super) fn capture(world: &mut World) -> Value {
    let config = world.get_resource::<EngineConfig>();
    let mut build = Section::default();
    build.text("version", env!("CARGO_PKG_VERSION"));
    build.text("bevy_version", "0.19.0");
    for (key, value) in [
        ("git_commit", option_env!("MUDCRAB_BUILD_GIT_COMMIT")),
        ("target", option_env!("MUDCRAB_BUILD_TARGET")),
        ("profile", option_env!("MUDCRAB_BUILD_PROFILE")),
    ] {
        match value.filter(|value| *value != "unknown") {
            Some(value) => build.text(key, value),
            None => build.unknown(key, "build provenance not supplied"),
        }
    }
    match option_env!("MUDCRAB_BUILD_GIT_DIRTY") {
        Some("true") => build.set("dirty", true),
        Some("false") => build.set("dirty", false),
        _ => build.unknown("dirty", "build-time worktree state unknown"),
    }
    let platform = platform_snapshot(world);
    let origin = world.get_resource::<RenderOrigin>().map(|origin| origin.0);
    let mut session = Section::default();
    if let Some(time) = world.get_resource::<Time<Real>>() {
        session.numeric("uptime_seconds", time.elapsed_secs_f64());
    } else {
        session.unknown("uptime_seconds", "real-time clock resource absent");
    }
    if let Some(time) = world.get_resource::<Time<Virtual>>() {
        session.set("paused", time.is_paused());
    } else {
        session.unknown("paused", "virtual-time resource absent");
    }
    if let Some(config) = config {
        let fixture = config.material_fixture
            || config.terrain_water_fixture
            || config.transform_bounds_fixture
            || config.renderer_fixture
            || config.streaming_fixture
            || config.physics_fixture;
        session.set(
            "mode",
            if config.headless {
                "headless"
            } else if fixture {
                "fixture"
            } else if config.interactive_world_physics() {
                "interactive"
            } else {
                "automated"
            },
        );
        session.set("launch_worldspace", config.worldspace_id);
    } else {
        session.unknown("mode", "EngineConfig absent");
    }
    session.unknown("active_space", "this engine has no runtime active-space resource; launch worldspace is not current-scene proof");
    session.unknown("cell_identity", "runtime current-cell identity not exposed");
    match world.get_resource::<MoveMode>() {
        Some(MoveMode::Walk) => session.set("movement", "WALK"),
        Some(MoveMode::Noclip) => session.set("movement", "NOCLIP"),
        None => session.unknown("movement", "movement-mode resource absent"),
    }
    let rendering_config = configured_rendering(config);
    let mut context = Section::default();
    if let Some(frame) = world.get_resource::<FrameCount>() {
        context.set("main_world_frame", frame.0);
    } else {
        context.unknown("main_world_frame", "frame count absent");
    }
    context.unknown(
        "rendered_frame",
        "main-world snapshot does not identify a submitted rendered frame",
    );
    let mut pose = Section::default();
    pose.set("units", "Creation units; 4096 units per exterior cell");
    pose.set(
        "axis_convention",
        "Creation: X east, Y north, Z up; Bevy: X east, Y up, Z south",
    );
    pose.set(
        "coordinate_scope",
        "exterior render-origin mapping; active-space identity is unavailable",
    );
    if let Some(origin) = origin {
        pose.set("render_origin_grid", json!([origin.x, origin.y]));
    } else {
        pose.unknown("render_origin_grid", "RenderOrigin absent");
    }
    // StreamingCamera excludes auxiliary water-reflection and sky views. Fixtures have no
    // StreamingCamera, so fall back to one active Camera3d without scanning an unbounded list.
    let camera = {
        let mut query = world.query_filtered::<Entity, (With<Camera3d>, With<StreamingCamera>)>();
        query.iter(world).next()
    }
    .or_else(|| {
        let mut query = world.query_filtered::<(Entity, &Camera), With<Camera3d>>();
        query
            .iter(world)
            .take(16)
            .find(|(_, camera)| camera.is_active)
            .map(|(entity, _)| entity)
    });
    let mut effective = Section::default();
    if let Some(camera) = camera {
        if let Some(transform) = entity_transform(world, camera) {
            pose.set("camera", position_snapshot(&transform, origin));
        } else {
            pose.unknown("camera", "camera Transform absent");
        }
        if let Some(projection) = world.get::<Projection>(camera) {
            pose.set("projection", projection_snapshot(projection));
        } else {
            pose.unknown("projection", "camera Projection absent");
        }
        if let Some(msaa) = world.get::<Msaa>(camera) {
            effective.set("msaa_samples", msaa.samples());
        } else {
            effective.unknown("msaa_samples", "camera MSAA component absent");
        }
        if let Some(exposure) = world.get::<Exposure>(camera) {
            effective.numeric("exposure_ev100", f64::from(exposure.ev100));
        } else {
            effective.unknown("exposure_ev100", "camera exposure absent");
        }
        if let Some(tonemapping) = world.get::<Tonemapping>(camera) {
            effective.text("tonemapping", &format!("{tonemapping:?}"));
        } else {
            effective.unknown("tonemapping", "camera tone mapping absent");
        }
        effective.set(
            "distance_fog_component_present",
            world.get::<DistanceFog>(camera).is_some(),
        );
        if let Some(fog) = world.get::<DistanceFog>(camera) {
            let mut settings = Section::default();
            let color = fog.color.to_srgba().to_f32_array();
            if color.iter().all(|value| value.is_finite()) {
                settings.set("color_srgba", json!(color));
            } else {
                settings.unknown("color_srgba", "non-finite fog color");
            }
            settings.numeric(
                "directional_light_exponent",
                f64::from(fog.directional_light_exponent),
            );
            settings.set("falloff", fog_falloff_snapshot(&fog.falloff));
            effective.set("distance_fog", settings.finish());
        }
    } else {
        pose.unknown("camera", "no game Camera3d");
        effective.unknown("camera_settings", "no game Camera3d");
    }
    let player = {
        let mut query = world.query_filtered::<Entity, With<PlayerBody>>();
        query
            .iter(world)
            .next()
            .and_then(|entity| entity_transform(world, entity))
            .map(|transform| position_snapshot(&transform, origin))
    };
    if let Some(player) = player {
        pose.set("player", player);
    } else {
        pose.unknown(
            "player",
            "player body absent; NOCLIP and automated sessions may have no player",
        );
    }
    if let Some(metrics) = world.get_resource::<RendererMetrics>() {
        effective.set(
            "renderer",
            json!({
                "gpu_preprocessing_active": metrics.gpu_preprocessing_active,
                "gpu_culling_active": metrics.gpu_culling_active,
                "indirect_drawing_active": metrics.indirect_drawing_active,
                "occlusion_culling_views": metrics.occlusion_culling_views,
                "hzb_views": metrics.hzb_views,
                "proof_frames": metrics.proof_frames,
                "validation_failures": metrics.renderer_validation_failures,
            }),
        );
    } else {
        effective.unknown("renderer", "renderer metrics absent");
    }
    effective.unknown(
        "lighting_enabled",
        "no runtime lighting-setting resource; configured lighting is recorded separately",
    );
    let mut point_lights = world.query::<&PointLight>();
    let sampled: Vec<_> = point_lights.iter(world).take(257).collect();
    effective.set("point_light_components", json!({
        "sample_count": sampled.len().min(256), "truncated": sampled.len() > 256,
        "positive_intensity_in_sample": sampled.iter().take(256).filter(|light| light.intensity.is_finite() && light.intensity > 0.0).count(),
        "scope": "at most 256 main-world components; presence does not prove the light affected the screenshot",
    }));
    let mut water_surfaces = world.query_filtered::<Entity, With<WaterSurface>>();
    let water_count = water_surfaces.iter(world).take(257).count();
    effective.set(
        "water_surface_components",
        json!({"count": water_count.min(256), "truncated": water_count > 256,
        "scope": "main-world components; presence does not prove surface visibility"}),
    );
    effective.unknown("water_enabled", "no runtime water-setting resource; surface validation counters do not prove current visibility");
    effective.unknown(
        "lod_enabled",
        "no public runtime LOD-setting resource; resident/visible counters are recorded separately",
    );
    let window = window_snapshot(world);
    let streaming = world
        .get_resource::<StreamingMetrics>()
        .map(streaming_snapshot)
        .unwrap_or_else(
            || json!({"value": null, "unavailable": {"value": "streaming metrics absent"}}),
        );
    let performance = world
        .get_resource::<FrameHistory>()
        .map(FrameHistory::snapshot)
        .unwrap_or_else(
            || json!({"frame_ms": null, "unavailable": {"frame_ms": "frame history absent"}}),
        );
    json!({
        "schema_version": 1,
        "collection_scope": "bounded allowlist of current main-world resources and camera components",
        "context": context.finish(), "build": build.finish(), "platform": platform,
        "session": session.finish(), "pose": pose.finish(),
        "rendering": {"configured": rendering_config, "effective": effective.finish(), "window": window},
        "streaming": streaming, "performance": performance,
    })
}

fn platform_snapshot(world: &World) -> Value {
    let mut platform = Section::default();
    platform.set("architecture", std::env::consts::ARCH);
    if let Some(system) = world.get_resource::<SystemInfo>() {
        for (key, value) in [
            ("os", &system.os),
            ("kernel", &system.kernel),
            ("cpu", &system.cpu),
            ("core_count", &system.core_count),
            ("total_memory", &system.memory),
        ] {
            platform.text(key, value);
        }
    } else {
        platform.set("os_family", std::env::consts::OS);
        for key in ["os", "kernel", "cpu", "core_count", "total_memory"] {
            platform.unknown(key, "SystemInfo unavailable");
        }
    }
    if let Some(adapter) = world.get_resource::<RenderAdapterInfo>() {
        platform.text("gpu", &adapter.name);
        platform.text("driver", &adapter.driver);
        platform.text("driver_info", &adapter.driver_info);
        platform.text("backend", &format!("{:?}", adapter.backend));
    } else {
        for key in ["gpu", "driver", "driver_info", "backend"] {
            platform.unknown(key, "GPU adapter information not exposed in this world");
        }
    }
    if let Some(measurement) = world
        .get_resource::<DiagnosticsStore>()
        .and_then(|store| store.get(&SystemInformationDiagnosticsPlugin::PROCESS_MEM_USAGE))
        .and_then(|diagnostic| diagnostic.value())
    {
        platform.numeric("process_memory_gib", measurement);
        platform.set(
            "process_memory_scope",
            "latest Bevy system diagnostic; process memory in GiB",
        );
    } else {
        platform.unknown(
            "process_memory_gib",
            "no ordinary-play process-memory measurement available",
        );
    }
    platform.finish()
}

fn configured_rendering(config: Option<&EngineConfig>) -> Value {
    let Some(config) = config else {
        return json!({"value": null, "unavailable": {"value": "EngineConfig absent"}});
    };
    let mut configured = Section::default();
    configured.text("visual_profile", config.visual_profile.name());
    configured.set("lighting", config.lights);
    configured.set(
        "diagnostic_asset_fallbacks",
        config.diagnostic_asset_fallbacks,
    );
    configured.set("allow_incomplete_assets", config.allow_incomplete_assets);
    configured.set("stream_radius_cells", config.stream_radius);
    configured.set("unload_radius_cells", config.unload_radius);
    configured.set("cell_commits_per_frame", config.max_cell_commits_per_frame);
    configured.set(
        "commit_budget_microseconds",
        config.max_commit_micros_per_frame,
    );
    configured.set("cell_unloads_per_frame", config.max_cell_unloads_per_frame);
    configured.set("model_spawns_per_frame", config.max_model_spawns_per_frame);
    configured.set("upload_mib_per_frame", config.max_upload_mib_per_frame);
    configured.set(
        "zero_budget_semantics",
        "unlimited for unload/model/upload budgets; io_threads=0 is automatic",
    );
    configured.set("io_threads_requested", config.io_threads);
    configured.unknown(
        "io_threads_effective",
        "resolved IO pool size not exposed as a runtime resource",
    );
    for (key, value) in [
        (
            "lod_block_level0_distance",
            config.terrain_lod.block_level0_distance,
        ),
        (
            "lod_block_level1_distance",
            config.terrain_lod.block_level1_distance,
        ),
        (
            "lod_block_maximum_distance",
            config.terrain_lod.block_maximum_distance,
        ),
        (
            "lod_split_distance_mult",
            config.terrain_lod.split_distance_mult,
        ),
    ] {
        configured.numeric(key, f64::from(value));
    }
    configured.set(
        "source",
        "current EngineConfig allowlist; paths and user-supplied profiling labels excluded",
    );
    configured.finish()
}

fn position_snapshot(transform: &Transform, origin: Option<IVec2>) -> Value {
    let mut pose = Section::default();
    if transform.translation.is_finite() {
        pose.set("render_local_bevy", json!(transform.translation.to_array()));
        if let Some(origin) = origin {
            let creation =
                shared::coordinates::runtime_to_creation_vector(transform.translation.to_array());
            // Use f64 for the grid addition so distant and negative origins do not overflow i32
            // or discard the local offset before export.
            let absolute = [
                f64::from(creation[0]) + f64::from(origin.x) * f64::from(CELL_SIZE),
                f64::from(creation[1]) + f64::from(origin.y) * f64::from(CELL_SIZE),
                f64::from(creation[2]),
            ];
            pose.set("exterior_absolute_creation", json!(absolute));
            pose.set(
                "exterior_coordinate_grid",
                json!([
                    (absolute[0] / f64::from(CELL_SIZE)).floor(),
                    (absolute[1] / f64::from(CELL_SIZE)).floor(),
                ]),
            );
        } else {
            pose.unknown("exterior_absolute_creation", "render-origin grid absent");
        }
    } else {
        pose.unknown("render_local_bevy", "non-finite position");
        pose.unknown("exterior_absolute_creation", "non-finite position");
    }
    if transform.rotation.is_finite() {
        pose.set(
            "orientation_bevy_xyzw",
            json!(transform.rotation.to_array()),
        );
    } else {
        pose.unknown("orientation_bevy_xyzw", "non-finite orientation");
    }
    pose.finish()
}

fn entity_transform(world: &World, entity: Entity) -> Option<Transform> {
    if world.get::<ChildOf>(entity).is_some() {
        world
            .get::<GlobalTransform>(entity)
            .map(GlobalTransform::compute_transform)
    } else {
        world.get::<Transform>(entity).copied()
    }
}

fn projection_snapshot(projection: &Projection) -> Value {
    let mut output = Section::default();
    match projection {
        Projection::Perspective(value) => {
            output.set("kind", "perspective");
            output.numeric("vertical_fov_radians", f64::from(value.fov));
            output.numeric("aspect_ratio", f64::from(value.aspect_ratio));
            output.numeric("near", f64::from(value.near));
            output.numeric("far", f64::from(value.far));
        }
        Projection::Orthographic(value) => {
            output.set("kind", "orthographic");
            output.numeric("scale", f64::from(value.scale));
            output.numeric("near", f64::from(value.near));
            output.numeric("far", f64::from(value.far));
        }
        Projection::Custom(_) => {
            output.set("kind", "custom");
            output.unknown("parameters", "custom projection has no reviewed exporter");
        }
    }
    output.finish()
}

fn fog_falloff_snapshot(falloff: &FogFalloff) -> Value {
    let mut settings = Section::default();
    match falloff {
        FogFalloff::Linear { start, end } => {
            settings.set("kind", "linear");
            settings.numeric("start", f64::from(*start));
            settings.numeric("end", f64::from(*end));
        }
        FogFalloff::Exponential { density } | FogFalloff::ExponentialSquared { density } => {
            settings.set(
                "kind",
                if matches!(falloff, FogFalloff::Exponential { .. }) {
                    "exponential"
                } else {
                    "exponential_squared"
                },
            );
            settings.numeric("density", f64::from(*density));
        }
        FogFalloff::Atmospheric {
            extinction,
            inscattering,
        } => {
            settings.set("kind", "atmospheric");
            for (key, value) in [("extinction", extinction), ("inscattering", inscattering)] {
                if value.is_finite() {
                    settings.set(key, json!(value.to_array()));
                } else {
                    settings.unknown(key, "non-finite fog parameter");
                }
            }
        }
    }
    settings.finish()
}

fn window_snapshot(world: &mut World) -> Value {
    let mut query = world.query_filtered::<&Window, With<PrimaryWindow>>();
    let Some(window) = query.iter(world).next() else {
        return json!({"value": null, "unavailable": {"value": "primary game window absent"}});
    };
    let mut output = Section::default();
    output.set("physical_width", window.physical_width());
    output.set("physical_height", window.physical_height());
    output.numeric("scale_factor", f64::from(window.scale_factor()));
    output.set("present_mode", format!("{:?}", window.present_mode));
    output.set("focused", window.focused);
    output.finish()
}

fn streaming_snapshot(metrics: &StreamingMetrics) -> Value {
    let failures: Vec<_> = metrics
        .asset_failures
        .iter()
        .rev()
        .take(MAX_FAILURES)
        .map(|failure| {
            let dependencies: Vec<_> = failure
                .dependency_chain
                .iter()
                .take(MAX_DEPENDENCIES)
                .map(|text| truncate(text, MAX_FIELD_BYTES))
                .collect();
            json!({"model_path": truncate(&failure.model_path, MAX_FIELD_BYTES),
            "reference_form_id": failure.reference_form_id, "base_form_id": failure.base_form_id,
            "cell_id": failure.cell_id, "dependency_chain": dependencies,
            "omitted_dependencies": failure.dependency_chain.len().saturating_sub(MAX_DEPENDENCIES),
            "truncated_model_path": failure.model_path.len() > MAX_FIELD_BYTES,
            "truncated_dependencies": failure.dependency_chain.iter().take(MAX_DEPENDENCIES)
                .filter(|text| text.len() > MAX_FIELD_BYTES).count()})
        })
        .collect();
    // The collector retains a bounded ring and counts every failure; the export shows the
    // newest of the retained entries and the exact total, so a repeated failure is never
    // silently dropped from the count.
    let exported_asset_failures = failures.len();
    let asset_failures_total = metrics.asset_failures.total();
    json!({
        "resident_cells": metrics.resident_cells, "loading_cells": metrics.loading_cells,
        "failed_cells_total": metrics.failed_cells, "retiring_cells": metrics.retiring_cells,
        "active_requests": metrics.active_requests, "pending_asset_instances": metrics.pending_asset_instances,
        "pending_surface_instances": metrics.pending_surface_instances, "arming_queue_depth": metrics.arming_queue_depth,
        "pending_lod_queries": metrics.pending_lod_queries, "pending_lod_chunks": metrics.pending_lod_chunks,
        "resident_lod_chunks": metrics.resident_lod_chunks, "visible_lod_terrain_patches": metrics.visible_lod_terrain_patches,
        "unrecovered_lod_queries": metrics.unrecovered_lod_queries, "unrecovered_lod_chunks": metrics.unrecovered_lod_chunks,
        "failed_lod_queries_total": metrics.failed_lod_queries, "failed_lod_chunks_total": metrics.failed_lod_chunks,
        "asset_load_failures": metrics.asset_load_failures, "material_validation_failures": metrics.material_validation_failures,
        "terrain_validation_failures": metrics.terrain_validation_failures, "water_validation_failures": metrics.water_validation_failures,
        "transform_bounds_validation_failures": metrics.transform_bounds_validation_failures,
        "streaming_invariant_failures": metrics.streaming_invariant_failures,
        "static_colliders_excluded_authored": metrics.static_colliders_excluded_authored,
        "diagnostic_fallbacks": metrics.diagnostic_fallbacks, "origin_rebases": metrics.origin_rebases,
        "commit_budget_violations": metrics.commit_budget_violations,
        "recent_asset_failures": failures,
        "failure_order": "newest first by insertion order; records have no timestamps",
        "asset_failures_total": asset_failures_total,
        "omitted_asset_failures": asset_failures_total.saturating_sub(exported_asset_failures as u64),
        "retained_asset_failures": metrics.asset_failures.len(),
        "failure_limit": MAX_FAILURES, "dependency_limit_per_failure": MAX_DEPENDENCIES,
    })
}

/// Read selected cached metadata on the packaging worker; do not walk or hash the asset pack.
pub(super) fn content_metadata(assets: &Path) -> Value {
    let conversion = selected_metadata(
        &assets.join("conversion-manifest.json"),
        &[
            "schema_version",
            "retained_mesh_schema_version",
            "retained_asset_configuration_hash",
            "complete",
            "configuration_hash",
        ],
        true,
    );
    let lod = selected_metadata(
        &assets.join("lod-manifest.json"),
        &[
            "build_identity",
            "converter_schema",
            "world_database_schema",
            "chunks",
            "land_texture_repeats_per_cell",
        ],
        true,
    );
    let integration = selected_metadata(
        &assets.join("integration-report.json"),
        &["schema_version", "passed"],
        false,
    );
    let mut database = Section::default();
    let path = assets.join("skyrim_world.db");
    if fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.is_symlink())
    {
        if let Ok(connection) = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            let _ = connection.busy_timeout(Duration::from_millis(250));
            match connection.query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
                row.get::<_, u32>(0)
            }) {
                Ok(version) => database.set("schema_version", version),
                Err(_) => database.unknown(
                    "schema_version",
                    "read-only database schema query unavailable",
                ),
            }
            match connection.query_row(
                "SELECT substr(build_identity,1,129) FROM lod_build WHERE id=1 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            ) {
                Ok(identity) if canonical_hash(&identity) => {
                    database.set("lod_build_identity", identity)
                }
                _ => database.unknown(
                    "lod_build_identity",
                    "no valid bounded database LOD identity",
                ),
            }
        } else {
            database.unknown("schema_version", "read-only database open failed");
            database.unknown("lod_build_identity", "database unavailable");
        }
    } else {
        database.unknown(
            "schema_version",
            "world database absent, nonregular, or symlinked",
        );
        database.unknown("lod_build_identity", "database unavailable");
    }
    json!({"conversion": conversion, "lod": lod, "integration": integration, "database": database.finish(),
        "hash_scope": "converter configuration hashes identify producer settings; LOD build identities identify compiled LOD metadata, not every asset",
        "metadata_read_limit_bytes_per_file": METADATA_READ_LIMIT})
}

fn canonical_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

const HEADER_STOP: &str = "bug-report selected metadata header complete";

// Converter manifests start with the selected header and can contain enormous entry inventories.
// Stop before the first unreviewed field rather than allocating, copying, or reading that inventory.
struct SelectedHeader<'a> {
    keys: &'a [&'a str],
    fields: &'a mut Map<String, Value>,
    stop_at_unreviewed: bool,
}

impl<'de> DeserializeSeed<'de> for SelectedHeader<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for SelectedHeader<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a metadata object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if !self.keys.contains(&key.as_str()) {
                if self.stop_at_unreviewed {
                    return Err(serde::de::Error::custom(HEADER_STOP));
                }
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let value = map.next_value::<Value>()?;
            if value.is_object() || value.is_array() {
                return Err(serde::de::Error::custom(
                    "selected metadata value must be scalar",
                ));
            }
            let valid_type = match key.as_str() {
                "complete" | "passed" => value.is_boolean(),
                "schema_version" | "converter_schema" | "world_database_schema" | "chunks" => {
                    value.as_u64().is_some()
                }
                "retained_mesh_schema_version" => value.is_null() || value.as_u64().is_some(),
                "land_texture_repeats_per_cell" => value.as_f64().is_some_and(f64::is_finite),
                _ => value.is_string(),
            };
            if !valid_type {
                return Err(serde::de::Error::custom("invalid selected metadata type"));
            }
            if let Some(value) = value.as_str()
                && (key.contains("hash") || key == "build_identity")
                && !canonical_hash(value)
            {
                return Err(serde::de::Error::custom("invalid metadata digest"));
            }
            self.fields.insert(key, value);
        }
        Ok(())
    }
}

fn selected_metadata(path: &Path, keys: &[&str], stop_at_unreviewed: bool) -> Value {
    let mut output = Section::default();
    for key in keys {
        output.unknown(key, "selected metadata field unavailable");
    }
    if !fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.is_symlink())
    {
        output.set(
            "omission_reason",
            "metadata file absent, nonregular, or symlinked",
        );
        return output.finish();
    }
    let Ok(file) = fs::File::open(path) else {
        output.set("omission_reason", "metadata file cannot be opened");
        return output.finish();
    };
    let mut fields = Map::new();
    let reader = std::io::BufReader::new(file.take(METADATA_READ_LIMIT));
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let result = SelectedHeader {
        keys,
        fields: &mut fields,
        stop_at_unreviewed,
    }
    .deserialize(&mut deserializer);
    let header_only = result
        .as_ref()
        .err()
        .is_some_and(|error| error.to_string().starts_with(HEADER_STOP));
    if result.is_err() && !header_only {
        output.set(
            "omission_reason",
            "metadata header invalid or exceeded bounded read limit",
        );
        return output.finish();
    }
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        output.unavailable.remove(&key);
        if let Some(text) = value.as_str() {
            output.text(&key, text);
        } else {
            output.set(&key, value);
        }
    }
    output.set(
        "validation_scope",
        if header_only {
            "selected leading header fields only; remaining manifest not read or validated"
        } else {
            "selected scalar metadata object"
        },
    );
    output.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::AssetFailure;

    #[test]
    fn exterior_pose_restores_negative_render_origin_and_shared_axis_conversion() {
        let pose = position_snapshot(
            &Transform::from_xyz(128.0, 72.0, -256.0),
            Some(IVec2::new(-3, -7)),
        );
        assert_eq!(pose["render_local_bevy"], json!([128.0, 72.0, -256.0]));
        assert_eq!(
            pose["exterior_absolute_creation"],
            json!([-12_160.0, -28_416.0, 72.0])
        );
    }

    #[test]
    fn render_origin_rebase_preserves_absolute_position() {
        let before = position_snapshot(
            &Transform::from_xyz(5000.0, 90.0, -20.0),
            Some(IVec2::new(-4, 1)),
        );
        let after = position_snapshot(
            &Transform::from_xyz(904.0, 90.0, -20.0),
            Some(IVec2::new(-3, 1)),
        );
        assert_eq!(
            before["exterior_absolute_creation"],
            after["exterior_absolute_creation"]
        );
        let extreme = position_snapshot(
            &Transform::from_xyz(1.0, 2.0, 3.0),
            Some(IVec2::new(i32::MAX, i32::MIN)),
        );
        assert_eq!(
            extreme["exterior_absolute_creation"][0].as_f64(),
            Some(f64::from(i32::MAX) * 4096.0 + 1.0)
        );
    }

    #[test]
    fn missing_resources_and_nonfinite_pose_remain_unknown() {
        let report = capture(&mut World::new());
        assert!(report["pose"]["player"].is_null());
        assert!(report["session"]["paused"].is_null());
        assert!(report["platform"]["gpu"].is_null());
        let invalid =
            position_snapshot(&Transform::from_xyz(f32::NAN, 2.0, 3.0), Some(IVec2::ZERO));
        assert!(invalid["render_local_bevy"].is_null());
        assert_eq!(
            invalid["unavailable"]["render_local_bevy"],
            "non-finite position"
        );
        assert!(serde_json::to_string(&invalid).is_ok());
    }

    #[test]
    fn session_mode_follows_world_ownership_without_renderer_state() {
        let cases = [
            ("ordinary play", EngineConfig::default(), "interactive"),
            (
                "route",
                EngineConfig {
                    route: Some("route.json".into()),
                    ..default()
                },
                "automated",
            ),
            (
                "matched route",
                EngineConfig {
                    matched_route: Some("matched.json".into()),
                    ..default()
                },
                "automated",
            ),
            (
                "auto fly",
                EngineConfig {
                    auto_fly_speed: 900.0,
                    ..default()
                },
                "automated",
            ),
            (
                "acceptance screenshot",
                EngineConfig {
                    acceptance_screenshot: Some("shot.png".into()),
                    ..default()
                },
                "automated",
            ),
            (
                "hidden window",
                EngineConfig {
                    hidden_window: true,
                    ..default()
                },
                "automated",
            ),
            (
                "benchmark jump",
                EngineConfig {
                    benchmark_jump: Some((3, 4)),
                    ..default()
                },
                "automated",
            ),
            (
                "fixture takes precedence over route",
                EngineConfig {
                    physics_fixture: true,
                    route: Some("route.json".into()),
                    ..default()
                },
                "fixture",
            ),
            (
                "headless takes precedence over fixture",
                EngineConfig {
                    headless: true,
                    physics_fixture: true,
                    ..default()
                },
                "headless",
            ),
        ];

        for (case, config, expected) in cases {
            let mut world = World::new();
            world.insert_resource(config);
            assert_eq!(capture(&mut world)["session"]["mode"], expected, "{case}");
        }
    }

    #[test]
    fn capture_uses_actual_camera_settings_and_excludes_config_paths_and_labels() {
        let mut world = World::new();
        world.insert_resource(EngineConfig {
            assets_dir: "/private/assets".into(),
            bug_report_dir: Some("/private/reports".into()),
            profile_commit: "untrusted-label".into(),
            ..default()
        });
        world.insert_resource(RenderOrigin(IVec2::new(-1, -1)));
        world.spawn((
            Camera3d::default(),
            StreamingCamera,
            Transform::from_xyz(10.0, 30.0, -20.0),
            Msaa::Sample2,
            Exposure { ev100: 7.0 },
            Tonemapping::None,
            Projection::Perspective(PerspectiveProjection {
                fov: 0.8,
                ..default()
            }),
        ));
        let report = capture(&mut world);
        assert_eq!(report["rendering"]["effective"]["msaa_samples"], 2);
        assert_eq!(report["rendering"]["effective"]["exposure_ev100"], 7.0);
        assert_eq!(
            report["pose"]["projection"]["vertical_fov_radians"].as_f64(),
            Some(f64::from(0.8f32))
        );
        let text = serde_json::to_string(&report).unwrap();
        for excluded in ["/private/assets", "/private/reports", "untrusted-label"] {
            assert!(!text.contains(excluded));
        }
    }

    #[test]
    fn failure_export_takes_recent_bounded_entries_and_utf8_fields() {
        let mut metrics = StreamingMetrics::default();
        // More failures than the collector retains: the export must show the newest retained
        // entries and still count every recorded failure, including evicted ones.
        let recorded = crate::streaming::MAX_ASSET_FAILURES as u32 + 100;
        for id in 0..recorded {
            metrics.asset_failures.push(AssetFailure {
                model_path: "🦀".repeat(400),
                reference_form_id: id,
                base_form_id: 0,
                cell_id: 1,
                dependency_chain: vec!["é".repeat(900); 40],
            });
        }
        let export = streaming_snapshot(&metrics);
        let failures = export["recent_asset_failures"].as_array().unwrap();
        assert_eq!(failures.len(), MAX_FAILURES);
        assert_eq!(failures[0]["reference_form_id"], recorded - 1);
        assert_eq!(export["asset_failures_total"], recorded);
        assert_eq!(
            export["retained_asset_failures"],
            crate::streaming::MAX_ASSET_FAILURES
        );
        assert_eq!(export["omitted_asset_failures"], recorded - 32);
        assert!(failures[0]["model_path"].as_str().unwrap().len() <= MAX_FIELD_BYTES);
        assert_eq!(
            failures[0]["dependency_chain"].as_array().unwrap().len(),
            MAX_DEPENDENCIES
        );
        assert_eq!(failures[0]["omitted_dependencies"], 24);
    }

    #[test]
    fn frame_history_caps_samples_and_prunes_real_time_window() {
        let mut history = FrameHistory::default();
        for frame in 0..3000 {
            history.record(frame as f64 / 1000.0, 1.0);
        }
        assert_eq!(history.samples.len(), MAX_FRAME_SAMPLES);
        assert_eq!(history.capacity_evictions, 952);
        history.record(20.0, 20.0);
        assert_eq!(history.samples.len(), 1);
        history.record(21.0, f64::NAN);
        assert_eq!(history.invalid_samples, 1);
        assert_eq!(history.snapshot()["frame_ms"]["mean"], 20.0);
        history.record(40.0, f64::NAN);
        assert!(history.samples.is_empty());
        assert!(history.snapshot()["frame_ms"].is_null());
    }

    #[test]
    fn exterior_coordinate_grid_floors_negative_local_positions() {
        let west = position_snapshot(&Transform::from_xyz(-0.5, 10.0, 0.5), Some(IVec2::ZERO));
        assert_eq!(west["exterior_coordinate_grid"], json!([-1.0, -1.0]));
        let before_north_edge = position_snapshot(
            &Transform::from_xyz(0.0, 10.0, -4095.5),
            Some(IVec2::new(-3, -2)),
        );
        assert_eq!(
            before_north_edge["exterior_coordinate_grid"],
            json!([-3.0, -2.0])
        );
        let after_north_edge = position_snapshot(
            &Transform::from_xyz(0.0, 10.0, -4096.5),
            Some(IVec2::new(-3, -2)),
        );
        assert_eq!(
            after_north_edge["exterior_coordinate_grid"],
            json!([-3.0, -1.0])
        );
    }

    #[test]
    fn fog_nonfinite_parameters_are_unknown_with_explicit_reasons() {
        let fog = fog_falloff_snapshot(&FogFalloff::Exponential {
            density: f32::INFINITY,
        });
        assert!(fog["density"].is_null());
        assert_eq!(fog["unavailable"]["density"], "non-finite measurement");
    }

    #[test]
    fn content_metadata_reads_only_header_and_named_readonly_database_rows() {
        let directory = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let manifest = format!(
            "{{\"schema_version\":24,\"complete\":true,\"configuration_hash\":\"{hash}\",\"entries\":{{\"private\":\"{}\"}}}}",
            "secret".repeat(100_000)
        );
        fs::write(directory.path().join("conversion-manifest.json"), manifest).unwrap();
        let connection =
            rusqlite::Connection::open(directory.path().join("skyrim_world.db")).unwrap();
        connection.execute_batch("CREATE TABLE schema_info(version INTEGER); INSERT INTO schema_info VALUES(19); CREATE TABLE lod_build(id INTEGER,build_identity TEXT);").unwrap();
        connection
            .execute("INSERT INTO lod_build VALUES (1,?1)", [&hash])
            .unwrap();
        drop(connection);
        let metadata = content_metadata(directory.path());
        assert_eq!(metadata["conversion"]["schema_version"], 24);
        assert_eq!(metadata["conversion"]["configuration_hash"], hash);
        assert_eq!(metadata["conversion"]["complete"], true);
        assert_eq!(metadata["database"]["schema_version"], 19);
        assert_eq!(metadata["database"]["lod_build_identity"], hash);
        assert!(!serde_json::to_string(&metadata).unwrap().contains("secret"));
        assert!(metadata["lod"]["build_identity"].is_null());
    }

    #[test]
    fn invalid_or_oversized_metadata_remains_unavailable_without_raw_errors() {
        let directory = tempfile::tempdir().unwrap();
        for manifest in [
            "{\"schema_version\":oops".to_owned(),
            format!(
                "{{\"configuration_hash\":\"{}\"}}",
                "b".repeat(METADATA_READ_LIMIT as usize)
            ),
        ] {
            fs::write(directory.path().join("conversion-manifest.json"), manifest).unwrap();
            let metadata = content_metadata(directory.path());
            assert!(metadata["conversion"]["schema_version"].is_null());
            assert!(metadata["conversion"]["configuration_hash"].is_null());
            assert!(metadata["conversion"]["omission_reason"].is_string());
        }
    }
}
