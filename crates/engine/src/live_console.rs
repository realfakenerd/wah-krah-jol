//! Opt-in process-local control for a visible developer tuning session.

use crate::{
    capture_frame::CaptureFrame,
    color_pipeline::{SceneGradeMode, SceneGradeSource, SceneVisualPreset},
    console::{AppConsoleExt, ConsoleCommand, ConsoleState, execute_line},
    streaming::StreamingMetrics,
    terrain_upload::TerrainMeshUploadReadiness,
    tone_mapping::SceneToneMapSource,
};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured},
};
use std::{
    io::{self, BufRead},
    path::PathBuf,
    sync::{Mutex, mpsc},
};

#[derive(Resource)]
struct IncomingCommands(Mutex<mpsc::Receiver<String>>);

pub(crate) struct StdinConsolePlugin;

impl Plugin for StdinConsolePlugin {
    fn build(&self, app: &mut App) {
        let (sender, receiver) = mpsc::sync_channel(32);
        std::thread::Builder::new()
            .name("mudcrab-stdin-console".into())
            .spawn(move || {
                for line in io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if line.chars().count() <= crate::console::INPUT_CAP
                        && sender.send(line).is_err()
                    {
                        break;
                    }
                }
            })
            .expect("start developer console reader");
        app.insert_resource(IncomingCommands(Mutex::new(receiver)))
            .init_resource::<ConsoleState>()
            .add_console_command(ConsoleCommand {
                name: "SceneState", short: None, usage: "SceneState",
                help: "report live grading and streaming readiness",
                handler: Box::new(|world, args| {
                    if !args.is_empty() { return Err("usage: SceneState".into()) }
                    Ok(scene_state(world, crate::shots::Failures::default()).to_string())
                }),
            })
            .add_console_command(ConsoleCommand {
                name: "SceneShot", short: None, usage: "SceneShot <png-path>",
                help: "capture the visible world after streaming and uploads settle",
                handler: Box::new(|world, args| {
                    let [path] = args else { return Err("usage: SceneShot <png-path>".into()) };
                    let path = PathBuf::from(path);
                    if path.extension().is_none_or(|ext| ext != "png") {
                        return Err("SceneShot requires a .png path".into());
                    }
                    let (state, ticket) = prepare_scene_shot(world)?;
                    if path.exists() || path.with_extension("json").exists() {
                        return Err("SceneShot refuses to replace existing evidence".into());
                    }
                    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
                    let destination = path.clone();
                    world.spawn((Screenshot::primary_window(), ticket.clone())).observe(move |event: On<ScreenshotCaptured>| {
                        let result = (|| -> Result<(), String> {
                            let frame = ticket.receipt().ok_or("missing live capture-frame receipt")?;
                            let image = event.image.clone().try_into_dynamic().map_err(|e| e.to_string())?;
                            let receipt = build_live_capture_receipt(&state, &frame, [image.width(), image.height()])?;
                            image.save(&destination).map_err(|e| e.to_string())?;
                            std::fs::write(destination.with_extension("json"), serde_json::to_string_pretty(&receipt).unwrap()).map_err(|e| e.to_string())?;
                            Ok(())
                        })();
                        match result {
                            Ok(()) => info!(target: "live_console", "saved live screenshot {}", destination.display()),
                            Err(error) => error!(target: "live_console", "live screenshot failed: {error}"),
                        }
                    });
                    Ok(format!("live screenshot requested: {}", path.display()))
                }),
            })
            .add_systems(Update, dispatch_stdin);
    }
}

fn build_live_capture_receipt(
    requested_state: &serde_json::Value,
    frame: &CaptureFrame,
    dimensions: [u32; 2],
) -> Result<serde_json::Value, String> {
    let grading = frame
        .grading
        .as_ref()
        .ok_or("live captured frame has no validated grading evidence")?;
    if !frame.work.is_quiet() {
        return Err(format!(
            "live captured frame is not settled: {}",
            frame.work.describe()
        ));
    }
    let tone_map = grading.source["tone_map_mode"]
        .as_str()
        .ok_or("live captured frame has no tone-map mode evidence")?;
    Ok(serde_json::json!({
        "ready": true,
        "grade": grading.color_grade,
        "preset": grading.visual_preset,
        "tone_map": tone_map,
        "preset_saturation": grading.preset_saturation,
        "saturation": grading.resolved_grade["saturation"],
        "brightness": grading.resolved_grade["brightness"],
        "contrast": grading.resolved_grade["contrast"],
        "tint_amount": grading.resolved_grade["tint_amount"],
        "tint_rgb": grading.resolved_grade["tint_rgb"],
        "resolved_grade": grading.resolved_grade,
        "grading_source": grading.source,
        "work": frame.work,
        "dimensions": dimensions,
        "capture_frame": frame,
        "requested_state": requested_state,
        "classification": "live screenshot; grading, camera and readiness bound to captured render frame; request-time state retained separately",
    }))
}

fn dispatch_stdin(world: &mut World) {
    let lines = {
        let receiver = world.resource::<IncomingCommands>().0.lock().unwrap();
        receiver.try_iter().take(16).collect::<Vec<_>>()
    };
    for line in lines {
        let output = execute_line(world, &line);
        for result in output.lines() {
            info!(target: "live_console", "{result}");
        }
    }
}

fn prepare_scene_shot(
    world: &World,
) -> Result<(serde_json::Value, crate::capture_frame::CaptureTicket), String> {
    let baseline = match (
        world.get_resource::<StreamingMetrics>(),
        world.get_resource::<crate::render::RendererMetrics>(),
    ) {
        (Some(streaming), Some(renderer)) => crate::shots::Failures::read(streaming, renderer),
        _ => crate::shots::Failures::default(),
    };
    let state = scene_state(world, baseline);
    if state["ready"] != true {
        return Err(format!("scene is still loading: {state}"));
    }
    Ok((state, crate::capture_frame::CaptureTicket::new(baseline)))
}

fn scene_state(world: &World, baseline: crate::shots::Failures) -> serde_json::Value {
    let mode = *world.resource::<SceneGradeMode>();
    let preset = *world.resource::<SceneVisualPreset>();
    let profile = world
        .get_resource::<crate::visual_profile::VisualProfile>()
        .map(|profile| profile.name());
    let streaming = world.get_resource::<StreamingMetrics>();
    let pending_terrain = world
        .get_resource::<TerrainMeshUploadReadiness>()
        .map_or(0, |uploads| uploads.outstanding());
    let renderer_ready = world
        .get_resource::<crate::render::RendererMetrics>()
        .is_some_and(|r| r.final_path_active());
    let pending_specializations = world
        .get_resource::<crate::capture_frame::RetryReadiness>()
        .and_then(|r| *r.0.lock().unwrap());
    let pending_transfers = world
        .get_resource::<crate::capture_frame::TransferReadiness>()
        .and_then(|r| *r.0.lock().unwrap());
    let profiler = world.get_resource::<crate::profiling::ProfilingState>();
    let pending_batches = [
        "lod/pending_terrain_batch_chunks",
        "lod/pending_initial_terrain_upload_chunks",
        "lod/pending_terrain_selection_uploads",
    ]
    .iter()
    .map(|key| profiler.and_then(|p| p.gauge(key)).unwrap_or(0.0))
    .sum::<f64>();
    let ready = renderer_ready
        && streaming.is_some_and(|s| {
            s.pending_asset_instances == 0
                && s.pending_surface_instances == 0
                && s.loading_cells == 0
                && s.pending_lod_queries == 0
                && s.pending_lod_chunks == 0
                && s.failed_cells.saturating_sub(baseline.failed_cells) == 0
                && s.asset_load_failures
                    .saturating_sub(baseline.asset_load_failures)
                    == 0
                && s.unrecovered_lod_queries == 0
                && s.unrecovered_lod_chunks == 0
        })
        && pending_terrain == 0
        && pending_specializations == Some(0)
        && pending_transfers == Some(0)
        && pending_batches == 0.0;
    let source = world
        .get_resource::<SceneGradeSource>()
        .cloned()
        .unwrap_or_default();
    let tone = world
        .get_resource::<SceneToneMapSource>()
        .cloned()
        .unwrap_or_default();
    let overrides = world
        .get_resource::<crate::color_pipeline::SceneGradeOverrides>()
        .copied()
        .unwrap_or_default();
    let resolved = tone
        .resolve_with_overrides(&source, mode, preset, overrides)
        .expect("validated scene grade source");
    let grade = resolved.grade;
    serde_json::json!({
        "ready": ready, "visual_profile": profile, "grade": mode.name(), "preset": preset.name(),
        "saturation": grade.saturation, "brightness": grade.brightness, "contrast": grade.contrast,
        "tint_amount": grade.tint_amount, "tint_rgb": grade.tint_rgb,
        "tone_map": tone.mode.name(),
        "grading_source": tone.evidence_with_overrides(&source, mode, preset, overrides),
        "environment_adapter": "bevy-interior-v1",
        "environment": world.get_resource::<crate::world::environment::interior::SceneEnvironment>(),
        "pending_terrain_uploads": pending_terrain,
        "pending_specializations": pending_specializations,
        "pending_render_transfers": pending_transfers,
        "pending_batches": pending_batches,
        "renderer_ready": renderer_ready,
        "loading_cells": streaming.map(|s| s.loading_cells),
        "pending_assets": streaming.map(|s| s.pending_asset_instances),
        "pending_surfaces": streaming.map(|s| s.pending_surface_instances),
        "pending_lod_queries": streaming.map(|s| s.pending_lod_queries),
        "pending_lod_chunks": streaming.map(|s| s.pending_lod_chunks),
        "unrecovered_lod_queries": streaming.map(|s| s.unrecovered_lod_queries),
        "unrecovered_lod_chunks": streaming.map(|s| s.unrecovered_lod_chunks),
        "failed_cells": streaming.map(|s| s.failed_cells),
        "asset_load_failures": streaming.map(|s| s.asset_load_failures),
        "failed_lod_queries": streaming.map(|s| s.failed_lod_queries),
        "failed_lod_chunks": streaming.map(|s| s.failed_lod_chunks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture_frame::CaptureGrade, shots::SettleCounts};

    fn ready_scene_world() -> World {
        let mut world = World::new();
        world.insert_resource(SceneGradeMode::Full);
        world.insert_resource(SceneVisualPreset::CoolGray);
        world.insert_resource(SceneGradeSource::default());
        world.insert_resource(SceneToneMapSource::default());
        world.insert_resource(StreamingMetrics::default());
        world.insert_resource(crate::terrain_upload::TerrainMeshUploadReadiness::default());
        world.insert_resource(crate::profiling::ProfilingState::default());
        world.insert_resource(crate::render::RendererMetrics {
            gpu_preprocessing_active: true,
            gpu_culling_active: true,
            indirect_drawing_active: true,
            occlusion_culling_views: 1,
            hzb_views: 1,
            indirect_phase_buffers: 1,
            indirect_batch_sets: 1,
            proof_frames: 1,
            ..default()
        });
        world.insert_resource(crate::capture_frame::RetryReadiness(std::sync::Arc::new(
            std::sync::Mutex::new(Some(0)),
        )));
        world.insert_resource(crate::capture_frame::TransferReadiness(
            std::sync::Arc::new(std::sync::Mutex::new(Some(0))),
        ));
        world
    }

    fn captured_frame() -> CaptureFrame {
        CaptureFrame {
            main_frame: 42,
            render_frame: 41,
            camera_creation: [21750.0, -44530.0, 140.0],
            rotation_runtime_xyzw: [0.0, 0.0, 0.0, 1.0],
            vertical_fov_radians: Some(std::f32::consts::FRAC_PI_4),
            render_origin: [5, -11],
            pending_specializations: Some(0),
            work: SettleCounts {
                renderer_ready: true,
                pending_specializations: Some(0),
                pending_render_transfers: Some(0),
                ..default()
            },
            terrain_coverage: default(),
            terrain_gauges: default(),
            visual_profile: Some(crate::visual_profile::VisualProfile::Mudcrab),
            grading: Some(CaptureGrade {
                color_grade: "snapshot-srgb".into(),
                visual_preset: "cool-gray".into(),
                preset_saturation: 1.25,
                resolved_grade: serde_json::json!({
                    "saturation": 1.66,
                    "brightness": 1.38,
                    "contrast": 1.5,
                    "tint_amount": 0.56,
                    "tint_rgb": [0.88, 0.81, 0.72],
                }),
                source: serde_json::json!({
                    "tone_map_mode": "tony",
                    "source_kind": "approved-artistic-override",
                    "approved_override": {"path": "original-approved.json"},
                }),
            }),
        }
    }

    #[test]
    fn live_receipt_uses_captured_grading_after_request_state_changes() {
        let ticket = crate::capture_frame::CaptureTicket::default();
        ticket.set_receipt(captured_frame());
        // The console can select another grade/source while GPU readback is pending.
        let requested_state = serde_json::json!({
            "ready": true,
            "grade": "record-srgb",
            "preset": "vivid",
            "tone_map": "retail-curve",
            "saturation": 1.6,
            "brightness": 1.15,
            "grading_source": {
                "tone_map_mode": "retail-curve",
                "source_kind": "authored-image-space-record",
                "loaded_record": {"form_id": 77703},
            },
        });
        let frame = ticket.receipt().unwrap();
        let receipt = build_live_capture_receipt(&requested_state, &frame, [1920, 1080]).unwrap();
        assert_eq!(receipt["grade"], "snapshot-srgb");
        assert_eq!(receipt["preset"], "cool-gray");
        assert_eq!(receipt["tone_map"], "tony");
        assert_eq!(receipt["saturation"], 1.66);
        assert_eq!(receipt["brightness"], 1.38);
        assert_eq!(
            receipt["grading_source"]["approved_override"]["path"],
            "original-approved.json"
        );
        assert_eq!(
            receipt["grading_source"]["loaded_record"],
            serde_json::Value::Null
        );
        assert_eq!(receipt["requested_state"], requested_state);
        assert_eq!(
            receipt["grading_source"],
            receipt["capture_frame"]["grading"]["source"]
        );
        assert_eq!(
            receipt["resolved_grade"],
            receipt["capture_frame"]["grading"]["resolved_grade"]
        );
        assert_eq!(receipt["work"], receipt["capture_frame"]["work"]);
        assert_eq!(receipt["dimensions"], serde_json::json!([1920, 1080]));
    }

    #[test]
    fn live_receipt_rejects_missing_frame_grading_or_unsettled_rendering() {
        let request = serde_json::json!({"ready": true});
        let mut frame = captured_frame();
        frame.grading = None;
        assert!(
            build_live_capture_receipt(&request, &frame, [1920, 1080])
                .unwrap_err()
                .contains("no validated grading")
        );
        let mut frame = captured_frame();
        frame.work.pending_asset_instances = 1;
        assert!(
            build_live_capture_receipt(&request, &frame, [1920, 1080])
                .unwrap_err()
                .contains("not settled")
        );
        frame.work.pending_asset_instances = 0;
        frame.work.pending_specializations = None;
        assert!(build_live_capture_receipt(&request, &frame, [1920, 1080]).is_err());
        frame.work.pending_specializations = Some(0);
        frame.work.renderer_ready = false;
        assert!(build_live_capture_receipt(&request, &frame, [1920, 1080]).is_err());
        let mut frame = captured_frame();
        frame.grading.as_mut().unwrap().source["tone_map_mode"] = serde_json::Value::Null;
        assert!(
            build_live_capture_receipt(&request, &frame, [1920, 1080])
                .unwrap_err()
                .contains("no tone-map mode")
        );
    }

    #[test]
    fn scene_shot_uses_request_baseline_and_keeps_cumulative_history_visible() {
        let mut world = ready_scene_world();
        *world.resource_mut::<StreamingMetrics>() = StreamingMetrics {
            failed_cells: 3,
            asset_load_failures: 2,
            material_validation_failures: 1,
            failed_lod_queries: 4,
            failed_lod_chunks: 5,
            ..default()
        };

        let (request_state, ticket) = prepare_scene_shot(&world).unwrap();
        let baseline = crate::shots::Failures::read(
            world.resource::<StreamingMetrics>(),
            world.resource::<crate::render::RendererMetrics>(),
        );

        assert_eq!(request_state["ready"], true);
        assert_eq!(request_state["failed_cells"], 3);
        assert_eq!(request_state["asset_load_failures"], 2);
        assert_eq!(request_state["failed_lod_queries"], 4);
        assert_eq!(request_state["failed_lod_chunks"], 5);
        assert_eq!(ticket.baseline(), baseline);

        let cumulative_state = scene_state(&world, crate::shots::Failures::default());
        assert_eq!(cumulative_state["ready"], false);
        assert_eq!(cumulative_state["failed_cells"], 3);
        assert_eq!(cumulative_state["asset_load_failures"], 2);
        assert_eq!(cumulative_state["failed_lod_queries"], 4);
        assert_eq!(cumulative_state["failed_lod_chunks"], 5);
    }

    #[test]
    fn scene_shot_stays_blocked_for_unrecovered_lod_and_missing_resources() {
        let mut world = ready_scene_world();
        world
            .resource_mut::<StreamingMetrics>()
            .unrecovered_lod_chunks = 1;
        let Err(error) = prepare_scene_shot(&world) else {
            panic!("unrecovered LOD work must block SceneShot");
        };
        assert!(error.contains("scene is still loading"));

        let mut world = ready_scene_world();
        world.remove_resource::<StreamingMetrics>();
        let Err(error) = prepare_scene_shot(&world) else {
            panic!("missing streaming metrics must keep SceneShot unready");
        };
        assert!(error.contains("scene is still loading"));

        let mut world = ready_scene_world();
        world.remove_resource::<crate::render::RendererMetrics>();
        let Err(error) = prepare_scene_shot(&world) else {
            panic!("missing renderer metrics must keep SceneShot unready");
        };
        assert!(error.contains("scene is still loading"));
    }

    #[test]
    fn failures_after_scene_shot_request_block_its_capture_receipt() {
        for failure in ["cell", "asset", "validation", "lod"] {
            let mut world = ready_scene_world();
            *world.resource_mut::<StreamingMetrics>() = StreamingMetrics {
                failed_cells: 1,
                asset_load_failures: 2,
                material_validation_failures: 3,
                failed_lod_queries: 4,
                failed_lod_chunks: 5,
                ..default()
            };
            let (request_state, ticket) = prepare_scene_shot(&world).unwrap();
            let baseline = ticket.baseline();

            {
                let mut current = world.resource_mut::<StreamingMetrics>();
                match failure {
                    "cell" => current.failed_cells += 1,
                    "asset" => current.asset_load_failures += 1,
                    "validation" => current.material_validation_failures += 1,
                    "lod" => current.failed_lod_chunks += 1,
                    _ => unreachable!(),
                }
            }

            assert_eq!(
                ticket.baseline(),
                baseline,
                "{failure} changed the snapshot"
            );
            let mut work = SettleCounts::read(
                world.resource::<StreamingMetrics>(),
                world.resource::<crate::render::RendererMetrics>(),
                baseline,
                true,
                0,
            );
            work.pending_specializations = Some(0);
            work.pending_render_transfers = Some(0);
            let mut frame = captured_frame();
            frame.work = work;
            ticket.set_receipt(frame);

            let frame = ticket.receipt().unwrap();
            let error = build_live_capture_receipt(&request_state, &frame, [1920, 1080])
                .expect_err("a new failure must reject this frame's receipt");
            assert!(error.contains("not settled"), "{failure}: {error}");
        }
    }

    #[test]
    fn stdin_commands_use_the_registered_console_and_bound_work_per_update() {
        let mut app = App::new();
        let (sender, receiver) = mpsc::sync_channel(32);
        app.insert_resource(IncomingCommands(Mutex::new(receiver)))
            .init_resource::<ConsoleState>()
            .insert_resource(SceneGradeMode::Full)
            .add_console_command(ConsoleCommand {
                name: "TestMode",
                short: None,
                usage: "TestMode",
                help: "test",
                handler: Box::new(|world, _| {
                    *world.resource_mut::<SceneGradeMode>() = SceneGradeMode::SnapshotLinear;
                    Ok("selected snapshot-linear".into())
                }),
            });
        for _ in 0..17 {
            sender.send("TestMode".into()).unwrap();
        }
        dispatch_stdin(app.world_mut());
        assert_eq!(
            *app.world().resource::<SceneGradeMode>(),
            SceneGradeMode::SnapshotLinear
        );
        assert_eq!(app.world().resource::<ConsoleState>().scrollback.len(), 32);
        dispatch_stdin(app.world_mut());
        assert_eq!(app.world().resource::<ConsoleState>().scrollback.len(), 34);
    }
}
