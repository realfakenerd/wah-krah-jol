use crate::{
    config::EngineConfig,
    fingerprint::{RunFingerprint, records_measurements},
    frame_budget::{FrameBudgetReport, FrameSides, ScoredFrame, attribute},
    pacing::{PacingReport, PacingTracker},
    profiling::{
        CpuReport, GpuPassReport, MemoryReport, MetricSummary, ProfilingState,
        RenderMeasurementProbe, SceneReport, SystemMetadata, cpu_asset_bytes, reconcile_scene,
        summarize,
    },
    render::RendererMetrics,
    render_timing::{PipelineActivity, RenderTimingPlugin, RenderTimings},
    route::{RouteReport, RouteRunner},
    streaming::{ActiveSpace, RenderOrigin, StreamingMetrics},
    streaming_gpu_metrics::StreamingGpuMetricsBridge,
    streaming_trace::{
        BenchmarkWindow, CameraMotion, CameraObservation, StreamingBenchmarkRouteSnapshot,
        StreamingFrameSample,
    },
    world::components::{CELL_SIZE, StreamingCamera},
};
use bevy::ecs::system::SystemParam;
use bevy::{
    diagnostic::{
        DiagnosticsStore, EntityCountDiagnosticsPlugin, SystemInfo,
        SystemInformationDiagnosticsPlugin,
    },
    prelude::*,
    render::{
        ExtractSchedule, RenderApp,
        diagnostic::{
            ErasedRenderAssetDiagnosticPlugin, MeshAllocatorDiagnosticPlugin,
            RenderAssetDiagnosticPlugin,
        },
        mesh::RenderMesh,
        texture::GpuImage,
    },
    world_serialization::WorldAsset,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct AcceptanceMetricsPlugin;

impl Plugin for AcceptanceMetricsPlugin {
    /// Install benchmark collection and enable frame tracing when profile output is configured.
    fn build(&self, app: &mut App) {
        app.init_resource::<BenchmarkSamples>()
            .add_plugins((
                EntityCountDiagnosticsPlugin::default(),
                SystemInformationDiagnosticsPlugin,
                RenderTimingPlugin,
            ))
            .add_systems(
                Last,
                (record_frame_ledger, collect_and_finish)
                    .chain()
                    .after(crate::render_timing::end_main_world),
            );
        if app
            .world()
            .get_resource::<EngineConfig>()
            .is_some_and(|config| config.profile_output_dir.is_some())
        {
            app.add_systems(Last, sample_streaming_trace.before(collect_and_finish));
        }
        let measures_pacing = app
            .world()
            .get_resource::<EngineConfig>()
            .map(EngineConfig::measures_pacing);
        install_schedule_timing(app, measures_pacing);
        let records_measurements = app
            .world()
            .get_resource::<EngineConfig>()
            .map(records_measurements);
        install_gpu_memory_diagnostics(app, records_measurements);
    }
}

/// Adds Bevy's GPU-memory diagnostics — the mesh allocator's counters and the live GPU-asset
/// counts — only when the run records measurements.
///
/// Each of these runs a system every frame, in the main world and in the render world, so an
/// ordinary play session installs none of them. Materials are counted through the erased
/// render-asset diagnostic: every material type prepares into the same `PreparedMaterial` map, so
/// the one count covers all of them (see `profiling::gpu_asset_diagnostics`).
///
/// The probe is what tells the report those counters are real. Bevy's plugins add a main-world
/// `PreUpdate` system that publishes their atomics from the first frame, and the atomics are zero
/// until the render world's `ExtractSchedule` writes them, so the stored value alone cannot say
/// whether the render world has measured anything. The probe is the same shape as the plugins' own
/// handoff: one `Arc<AtomicBool>` in both worlds, set by an extract system and read by the report.
fn install_gpu_memory_diagnostics(app: &mut App, records_measurements: Option<bool>) {
    if records_measurements != Some(true) {
        return;
    }
    app.add_plugins((
        MeshAllocatorDiagnosticPlugin,
        RenderAssetDiagnosticPlugin::<RenderMesh>::new(" meshes"),
        RenderAssetDiagnosticPlugin::<GpuImage>::new(" images"),
        ErasedRenderAssetDiagnosticPlugin::<MeshMaterial3d<StandardMaterial>>::new(" materials"),
    ));
    let probe = RenderMeasurementProbe::default();
    app.insert_resource(probe.clone());
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app
            .insert_resource(probe)
            .add_systems(ExtractSchedule, mark_render_world_measured);
    }
}

/// Marks the run's [`RenderMeasurementProbe`] from the render world, in the schedule whose systems
/// fill Bevy's GPU-memory counters.
fn mark_render_world_measured(probe: Res<RenderMeasurementProbe>) {
    probe.mark_measured();
}

/// Adds the schedule-timing plugin (which also counts asset arrivals) only when the run measures
/// pacing. An ordinary play session installs neither the eight marker schedules per frame nor the
/// four asset-arrival counts.
fn install_schedule_timing(app: &mut App, measures_pacing: Option<bool>) {
    if measures_pacing == Some(true) {
        app.add_plugins(crate::schedule_timing::ScheduleTimingPlugin);
    }
}

#[derive(Resource, Default)]
struct BenchmarkSamples {
    frames_seen: u32,
    frame_ms: Vec<f64>,
    measured_seconds: f64,
    peak_process_memory_gib: f64,
    first_process_memory_gib: Option<f64>,
    last_process_memory_gib: Option<f64>,
    measurement_complete: bool,
    screenshot_wait_started: Option<std::time::Instant>,
    finished: bool,
    /// The frame-time distribution against the budget, scored frame by frame.
    ledger: FrameLedger,
    /// The route reached its last waypoint: the measured window ends with this frame.
    route_finished: bool,
}

/// What the frame-budget section of the report is built from: every scored frame with its owner.
///
/// It keeps its own frame count, which advances in step with `BenchmarkSamples::frames_seen`
/// (both count the frames of a benchmark run from the first, and stop together), so the same frames
/// are warm-up for both.
#[derive(Default)]
struct FrameLedger {
    frames_seen: u32,
    /// The streaming commit counters as of the previous frame, to find what this frame added.
    last_commit: Option<(u64, u64)>,
    /// The commit time of the previous frame's cycle, which is the cycle the current frame time
    /// measures.
    previous_upload_ms: Option<f64>,
    scored: Vec<ScoredFrame>,
}

impl FrameLedger {
    /// The streaming commit time of the frame that just ran, in milliseconds: zero on a frame that
    /// committed nothing. The counters only grow, so the first frame, which has nothing to compare
    /// with, counts as zero.
    fn commit_ms(&mut self, metrics: &StreamingMetrics) -> f64 {
        let now = (metrics.commit_frames, metrics.total_frame_commit_micros);
        let added = match self.last_commit {
            Some((frames, micros)) if now.0 > frames => {
                now.1.saturating_sub(micros) as f64 / 1000.0
            }
            _ => 0.0,
        };
        self.last_commit = Some(now);
        added
    }
}

/// Classify the current frame before benchmark collection advances its counters.
fn benchmark_window(config: &EngineConfig, samples: &BenchmarkSamples) -> BenchmarkWindow {
    if config.benchmark_frames.is_none() && config.benchmark_duration_secs.is_none() {
        BenchmarkWindow::OutsideBenchmark
    } else if samples.measurement_complete {
        BenchmarkWindow::Settlement
    } else if samples.frames_seen < config.benchmark_warmup_frames {
        BenchmarkWindow::Warmup
    } else {
        BenchmarkWindow::Measured
    }
}

fn route_snapshot(
    route: &crate::app::StreamingBenchmarkRoute,
    elapsed_ms: f64,
) -> StreamingBenchmarkRouteSnapshot {
    use crate::app::StreamingBenchmarkPhase;
    let offset_ms = elapsed_ms - route.latest_real_seconds * 1000.0;
    let timestamp = |seconds: f64| offset_ms + seconds * 1000.0;
    StreamingBenchmarkRouteSnapshot {
        phase: match route.phase {
            StreamingBenchmarkPhase::Waiting => "waiting",
            StreamingBenchmarkPhase::Moving => "moving",
            StreamingBenchmarkPhase::Tail => "tail",
            StreamingBenchmarkPhase::Finished => "finished",
        },
        elapsed_ms,
        real_elapsed_ms: route.latest_real_seconds * 1000.0,
        cpu_settled_elapsed_ms: route.cpu_settled_real_seconds.map(timestamp),
        movement_started_elapsed_ms: route.movement_started_real_seconds.map(timestamp),
        movement_finished_elapsed_ms: route.movement_finished_real_seconds.map(timestamp),
        tail_finished_elapsed_ms: route.tail_finished_real_seconds.map(timestamp),
        route_distance_units: route.route_distance_units,
    }
}

fn route_window(route: &crate::app::StreamingBenchmarkRoute) -> BenchmarkWindow {
    use crate::app::StreamingBenchmarkPhase;
    match route.phase {
        StreamingBenchmarkPhase::Waiting => BenchmarkWindow::Warmup,
        StreamingBenchmarkPhase::Moving => BenchmarkWindow::Measured,
        StreamingBenchmarkPhase::Tail
            if route.movement_started_real_seconds == route.movement_finished_real_seconds =>
        {
            BenchmarkWindow::Measured
        }
        StreamingBenchmarkPhase::Tail | StreamingBenchmarkPhase::Finished => {
            BenchmarkWindow::Settlement
        }
    }
}

#[derive(Clone)]
struct PreviousCamera {
    entity: Entity,
    worldspace_id: u32,
    position: [f64; 3],
    rotation: Quat,
}

/// Record stable world coordinates and motion relative to the previous comparable camera.
/// Origin rebases and equivalent quaternion signs do not count as movement.
fn observe_camera(
    entity: Entity,
    transform: &GlobalTransform,
    worldspace_id: u32,
    origin: IVec2,
    previous: &mut Option<PreviousCamera>,
) -> CameraObservation {
    let local = transform.translation();
    let position = [
        f64::from(local.x) + f64::from(origin.x) * f64::from(CELL_SIZE),
        f64::from(local.y),
        f64::from(local.z) - f64::from(origin.y) * f64::from(CELL_SIZE),
    ];
    let rotation = transform.rotation();
    let motion = match previous {
        Some(last) if last.entity == entity && last.worldspace_id == worldspace_id => {
            let translated = position
                .iter()
                .zip(last.position)
                .any(|(current, prior)| (current - prior).abs() > 0.001);
            let rotated = rotation.dot(last.rotation).abs() < 1.0 - 1.0e-6;
            match (translated, rotated) {
                (false, false) => CameraMotion::Stationary,
                (true, false) => CameraMotion::Translated,
                (false, true) => CameraMotion::Rotated,
                (true, true) => CameraMotion::TranslatedAndRotated,
            }
        }
        _ => CameraMotion::Unavailable,
    };
    *previous = Some(PreviousCamera {
        entity,
        worldspace_id,
        position,
        rotation,
    });
    CameraObservation {
        worldspace_id,
        render_origin_grid: [origin.x, origin.y],
        world_position: position,
        motion,
    }
}

/// Runs in `Last` before `collect_and_finish`, and `profiling::begin_trace_frame` clears the
/// per-frame span maps in `First`. Spans recorded after this system in `Last`, or by render-world
/// callbacks that land after it, therefore never reach the per-frame trace. They still reach the
/// aggregate profile, so the trace's span totals can be slightly lower than the aggregate.
#[allow(clippy::too_many_arguments)]
fn sample_streaming_trace(
    config: Res<EngineConfig>,
    samples: Res<BenchmarkSamples>,
    diagnostics: Res<DiagnosticsStore>,
    streaming: Option<Res<StreamingMetrics>>,
    admission: Option<Res<crate::streaming::admission::SceneAdmission>>,
    bridge: Option<Res<StreamingGpuMetricsBridge>>,
    server: Option<Res<AssetServer>>,
    mut events: MessageReader<AssetEvent<WorldAsset>>,
    cameras: Query<(Entity, &GlobalTransform), With<StreamingCamera>>,
    origin: Option<Res<RenderOrigin>>,
    space: Option<Res<ActiveSpace>>,
    mut previous_camera: Local<Option<PreviousCamera>>,
    mut pending_ids: Local<Vec<bevy::asset::AssetId<WorldAsset>>>,
    mut profiler: ResMut<ProfilingState>,
    runtime: Option<Res<crate::streaming::runtime::StreamingRuntime>>,
    route: Option<Res<crate::app::StreamingBenchmarkRoute>>,
) {
    // Trace v1 has no interior identity. Do not label interior coordinates as exterior world
    // positions or compare motion across the transition; resume observations after leaving.
    let active = space.as_deref().copied().unwrap_or_default();
    let camera = match cameras.single().ok().filter(|_| active.interior.is_none()) {
        Some((entity, transform)) => Some(observe_camera(
            entity,
            transform,
            active.exterior_worldspace(config.worldspace_id),
            origin.as_ref().map_or(IVec2::ZERO, |origin| origin.0),
            &mut previous_camera,
        )),
        None => {
            *previous_camera = None;
            None
        }
    };
    let sample = StreamingFrameSample {
        main_frame: profiler.main_frame(),
        elapsed_ms: profiler.elapsed_ms(),
        delta_ms: profiler.trace_delta_ms,
        benchmark_window: route
            .as_deref()
            .map_or_else(|| benchmark_window(&config, &samples), route_window),
        streaming_benchmark_route: route
            .as_deref()
            .map(|route| route_snapshot(route, profiler.elapsed_ms())),
        camera,
        streaming: streaming.as_deref().map(Into::into),
        scene_admission: (config.prioritize_streaming || config.max_scene_loads != 0)
            .then(|| {
                admission
                    .as_ref()
                    .map(|admission| crate::streaming_trace::SceneAdmissionSnapshot {
                        configured_job_limit: config.max_scene_loads,
                        prioritization_enabled: config.prioritize_streaming,
                        jobs: admission.stats(),
                    })
            })
            .flatten(),
        scenes: Default::default(),
        cpu_spans_ms: profiler.frame_cpu_spans_ms.clone(),
        completion_latencies_ms: profiler.frame_completion_latencies_ms.clone(),
        process_memory_gib: diagnostic_value(
            &diagnostics,
            &SystemInformationDiagnosticsPlugin::PROCESS_MEM_USAGE,
        ),
        gpu: bridge.as_ref().and_then(|bridge| bridge.latest()),
        adaptive: runtime
            .as_ref()
            .and_then(|runtime| runtime.snapshot.clone()),
    };
    if let Some(trace) = profiler.trace.as_mut() {
        if let Some(server) = server {
            for event in events.read() {
                match event {
                    AssetEvent::Unused { id }
                    | AssetEvent::Added { id }
                    | AssetEvent::Modified { id }
                    | AssetEvent::LoadedWithDependencies { id } => {
                        reconcile_scene(trace, &server, *id);
                    }
                    AssetEvent::Removed { id } => {
                        trace.remove_scene(*id);
                    }
                }
            }
            pending_ids.clear();
            pending_ids.extend(trace.pending_scene_ids());
            for id in pending_ids.iter().copied() {
                reconcile_scene(trace, &server, id);
            }
        }
        trace.push_frame(sample);
    }
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    format_version: u32,
    generated_unix_ms: u128,
    scenario: String,
    frames: usize,
    warmup_frames: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    streaming_benchmark_route: Option<StreamingBenchmarkRouteSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    measurement_scope: Option<&'static str>,
    synthetic_instances: usize,
    elapsed_seconds: f64,
    average_fps: f64,
    frame_ms_mean: f64,
    frame_ms_p50: f64,
    frame_ms_p95: f64,
    frame_ms_p99: f64,
    frame_ms_worst: f64,
    peak_process_memory_gib: Option<f64>,
    process_memory_growth_gib: Option<f64>,
    entity_count: Option<u64>,
    system: Option<SystemSnapshot>,
    streaming: Option<StreamingMetrics>,
    /// Last control observation, including reservation and physical-pressure gates.
    adaptive: Option<crate::streaming::runtime::RuntimeSnapshot>,
    renderer: RendererMetrics,
    /// CPU time of the parts of a frame the frame-time and GPU numbers do not split out, in
    /// milliseconds: the main world, the wait for the render thread, extract, and the render
    /// thread with its phases (see `render_timing`). Empty when the renderer did not run.
    render_world: BTreeMap<String, MetricSummary>,
    /// Render pipelines queued and finished per render frame, set against render-thread time (see
    /// `render_timing::PipelineActivity`). Serialised as `null` when the renderer did not run.
    render_pipelines: Option<PipelineActivity>,
    /// Time to a fully loaded world, the jump, and loading lag at speed (see `pacing`). The fields
    /// sit at the top level of the report and are absent for a run with no streaming.
    #[serde(flatten)]
    pacing: Option<PacingReport>,
    /// The frame-time distribution against `--frame-budget-ms` (6.25 ms unless given), the frames
    /// over it, and who owned them (see `frame_budget`). Present in every benchmark report. The
    /// frames scored are the measured window's, or, for a route run, the route's.
    frame_budget: FrameBudgetReport,
    /// What the `--route` run was and how far it got (see `route`); `null` without a route.
    route: Option<RouteReport>,
    /// What the run ran on: machine, renderer, world data, and the settings it rendered with (see
    /// `fingerprint`). `null` only for a run that recorded measurements without installing the
    /// fingerprint.
    fingerprint: Option<RunFingerprint>,
    /// Per-pass GPU time over the measured frames (see [`GpuPassReport`]).
    gpu_passes: GpuPassReport,
    /// What the run's scene contained: entity and mesh counts, and the per-pass
    /// pipeline-statistics counters the backend can report (see [`SceneReport`]).
    scene: SceneReport,
    /// Where the run's CPU time went: Bevy's main schedules and the engine's own spans (see
    /// [`CpuReport`]).
    cpu: CpuReport,
    /// Where the run's memory was: Bevy's CPU-side asset data and the GPU-side counters.
    memory: MemoryReport,
    thresholds: Thresholds,
    passed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SystemSnapshot {
    os: String,
    kernel: String,
    cpu: String,
    core_count: String,
    memory: String,
}

#[derive(Debug, Serialize)]
struct Thresholds {
    minimum_average_fps: f64,
    maximum_p95_frame_ms: f64,
    maximum_memory_growth_gib: f64,
    no_streaming_failures: bool,
    commit_budget_respected: bool,
    streaming_lifecycle_validated: bool,
    renderer_path_active: bool,
    screenshot_captured: bool,
}

/// The run's fingerprint, the render-world probe, the loaded assets and the `--route` file's runner,
/// grouped so that `collect_and_finish` stays within Bevy's system-parameter limit.
#[derive(SystemParam)]
struct MeasurementInputs<'w> {
    fingerprint: Option<Res<'w, RunFingerprint>>,
    render_measurement_probe: Option<Res<'w, RenderMeasurementProbe>>,
    meshes: Res<'w, Assets<Mesh>>,
    images: Res<'w, Assets<Image>>,
    file_route: Option<Res<'w, RouteRunner>>,
}

#[allow(clippy::too_many_arguments)]
fn collect_and_finish(
    time: Res<Time>,
    real_time: Res<Time<Real>>,
    config: Res<EngineConfig>,
    diagnostics: Res<DiagnosticsStore>,
    system: Option<Res<SystemInfo>>,
    streaming: Option<Res<StreamingMetrics>>,
    runtime: Option<Res<crate::streaming::runtime::StreamingRuntime>>,
    renderer: Res<RendererMetrics>,
    render_timings: Res<RenderTimings>,
    pacing: Option<Res<PacingTracker>>,
    measurements: MeasurementInputs,
    mut samples: ResMut<BenchmarkSamples>,
    mut profiler: ResMut<ProfilingState>,
    mut exit: MessageWriter<AppExit>,
    route: Option<Res<crate::app::StreamingBenchmarkRoute>>,
) {
    if samples.finished || !config.is_benchmark_run() {
        return;
    }
    if !samples.measurement_complete {
        samples.frames_seen = samples.frames_seen.saturating_add(1);
        let process_memory = diagnostic_value(
            &diagnostics,
            &SystemInformationDiagnosticsPlugin::PROCESS_MEM_USAGE,
        );
        // The profiler samples every frame — its process-memory timeline is whole-run — but only
        // keeps the GPU diagnostics of the measured frames, and the GPU-memory counters only once
        // the render world has measured them (see `ProfilingState::sample_frame`). A route run's
        // measured window is the route's own, not the warm-up count.
        // A file route's frames are measured from its first full moving frame on (the frame the route
        // scores), so the waiting for the world and the activation frame are not in the summary, the
        // budget, or the duration limit. A streaming route and a run without one keep their gates.
        let measuring = match (route.as_deref(), measurements.file_route.as_deref()) {
            (Some(route), _) => route_window(route) == BenchmarkWindow::Measured,
            (None, Some(file_route)) => file_route.scoring(),
            (None, None) => samples.frames_seen > config.benchmark_warmup_frames,
        };
        if (route.is_some() || measurements.file_route.is_some()) && !measuring {
            render_timings.set_recording(false);
        }
        profiler.sample_frame(
            &diagnostics,
            process_memory,
            measuring,
            measurements
                .render_measurement_probe
                .as_deref()
                .is_some_and(RenderMeasurementProbe::measured),
        );
        if measuring {
            render_timings.set_recording(true);
            let milliseconds = if route.is_some() || measurements.file_route.is_some() {
                real_time.delta_secs_f64() * 1000.0
            } else {
                time.delta_secs_f64() * 1000.0
            };
            if milliseconds.is_finite() && milliseconds > 0.0 {
                samples.frame_ms.push(milliseconds);
                samples.measured_seconds += milliseconds / 1000.0;
            }
        }
        if let Some(memory) = process_memory {
            samples.peak_process_memory_gib = samples.peak_process_memory_gib.max(memory);
            if samples.frames_seen > config.benchmark_warmup_frames
                && samples.measured_seconds >= memory_settle_seconds(&config)
            {
                samples.first_process_memory_gib.get_or_insert(memory);
                samples.last_process_memory_gib = Some(memory);
            }
        }
        let frame_limit_reached = config.benchmark_frames.is_some_and(|limit| {
            samples.frames_seen >= limit.saturating_add(config.benchmark_warmup_frames)
        });
        let duration_reached = config
            .benchmark_duration_secs
            .is_some_and(|limit| samples.frame_ms.iter().sum::<f64>() / 1000.0 >= limit);
        // A route is bounded by real elapsed time from engine start, so a route that never settles
        // (or stops advancing) still reaches the bundle and report before the runner's own timeout.
        let route_deadline_reached = config
            .benchmark_duration_secs
            .is_some_and(|limit| real_time.elapsed_secs_f64() >= limit);
        // A streaming route ends the window at its last waypoint, so the report covers the route
        // and not the standing around after it. A `--route` file ends it the same way, and its real
        // deadline ends a route that never became ready or never reached its end.
        let complete = match route.as_deref() {
            Some(route) => {
                route.phase == crate::app::StreamingBenchmarkPhase::Finished
                    || route_deadline_reached
            }
            None => {
                frame_limit_reached
                    || duration_reached
                    || samples.route_finished
                    || (measurements.file_route.is_some() && route_deadline_reached)
            }
        };
        if !complete {
            return;
        }
        samples.measurement_complete = true;
        render_timings.set_recording(false);
    }
    let screenshot_captured = config
        .acceptance_screenshot
        .as_ref()
        .is_none_or(|path| path.is_file());
    if !screenshot_captured {
        let waiting_since = samples
            .screenshot_wait_started
            .get_or_insert_with(std::time::Instant::now);
        if waiting_since.elapsed() < std::time::Duration::from_secs(10) {
            return;
        }
    }
    let render_world: BTreeMap<String, MetricSummary> = render_timings
        .take_samples()
        .into_iter()
        .map(|(name, values)| {
            for &value in &values {
                profiler.record_ms(format!("render_world/{name}"), value);
            }
            (name.to_owned(), summarize(&values))
        })
        .collect();
    let render_pipelines = render_timings.take_pipeline_activity();
    let pacing_report = pacing.as_deref().map(|tracker| {
        tracker.report(
            &config,
            streaming
                .as_deref()
                .map_or(0, |value| value.peak_arming_queue_depth),
        )
    });
    let mut ordered = samples.frame_ms.clone();
    ordered.sort_by(f64::total_cmp);
    let total_ms = ordered.iter().sum::<f64>();
    let mean = total_ms / ordered.len().max(1) as f64;
    let p50 = percentile(&ordered, 0.50);
    let p95 = percentile(&ordered, 0.95);
    let p99 = percentile(&ordered, 0.99);
    let worst = ordered.last().copied().unwrap_or_default();
    let average_fps = if mean > 0.0 { 1000.0 / mean } else { 0.0 };
    let no_streaming_failures = no_runtime_failures(
        streaming.as_deref(),
        config.material_fixture,
        config.terrain_water_fixture,
        config.transform_bounds_fixture,
    ) && (!config.streaming_fixture
        || streaming
            .as_deref()
            .is_some_and(|value| value.streaming_fixture_validated))
        && physics_fixture_ready(config.physics_fixture, streaming.as_deref());
    let commit_budget_respected = streaming
        .as_deref()
        .is_none_or(|value| value.commit_budget_violations == 0);
    let streaming_lifecycle_validated = streaming.as_deref().is_none_or(|value| {
        value.streaming_invariant_failures == 0
            && value.duplicate_cell_roots == 0
            && value.orphaned_cell_roots == 0
            && value.missing_cell_roots == 0
            && value.out_of_range_cell_roots == 0
            && value.streaming_fixture_failures == 0
    });
    let renderer_path_active = renderer.final_path_active()
        && (!config.renderer_fixture || renderer.renderer_fixture_validated);
    let memory_growth_gib = samples
        .first_process_memory_gib
        .zip(samples.last_process_memory_gib)
        .map(|(first, last)| (last - first).max(0.0));
    let passed = average_fps >= config.accept_min_fps
        && p95 <= config.accept_p95_ms
        && memory_growth_gib.is_none_or(|growth| growth <= config.accept_max_memory_growth_gib)
        && no_streaming_failures
        && commit_budget_respected
        && streaming_lifecycle_validated
        && renderer_path_active
        && screenshot_captured
        && route
            .as_ref()
            .is_none_or(|route| route.phase == crate::app::StreamingBenchmarkPhase::Finished)
        // A `--route` run passes only once its route has finished: a window closed by its frame
        // or duration limit reports the partial route, and that is not a pass.
        && measurements.file_route.as_deref().is_none_or(RouteRunner::finished);
    // Whether the device can time GPU work is the device's own answer, recorded in the fingerprint
    // at startup. The GPU sections take the fingerprint whole, so a run with no renderer says so
    // rather than blaming a backend it never had.
    let renderer_fingerprint = measurements
        .fingerprint
        .as_deref()
        .and_then(|value| value.renderer.as_ref());
    let gpu_passes = profiler.gpu_passes(renderer_fingerprint);
    let memory = MemoryReport {
        cpu: cpu_asset_bytes(&measurements.meshes, &measurements.images),
        gpu: profiler.gpu_memory(renderer_fingerprint),
    };
    // Both sections report what the run measured, so a count that was never sampled stays `null`
    // with a reason rather than becoming a zero (see `profiling::SceneReport`).
    let entity_count = diagnostic_value(&diagnostics, &EntityCountDiagnosticsPlugin::ENTITY_COUNT)
        .map(|value| value as u64);
    let scene = profiler.scene_report(entity_count, renderer_fingerprint);
    let cpu = profiler.cpu_report();
    let system_snapshot = system.map(|value| SystemSnapshot {
        os: value.os.clone(),
        kernel: value.kernel.clone(),
        cpu: value.cpu.clone(),
        core_count: value.core_count.clone(),
        memory: value.memory.clone(),
    });
    let frame_budget = FrameBudgetReport::from_frames(
        config.frame_budget_ms,
        if route.is_some() || measurements.file_route.is_some() {
            "route window: the frames from the first full frame on the route to the end of the measured window"
        } else {
            "measured window: every frame after the warm-up"
        },
        &samples.ledger.scored,
    );
    let report = BenchmarkReport {
        format_version: 11,
        generated_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis()),
        scenario: if config.profile_scenario.is_empty() {
            if config.streaming_fixture {
                "streaming".to_owned()
            } else if config.terrain_water_fixture {
                "terrain-water".to_owned()
            } else if config.renderer_fixture {
                "renderer".to_owned()
            } else if config.transform_bounds_fixture {
                "transform-bounds".to_owned()
            } else if config.material_fixture {
                "materials".to_owned()
            } else if config.benchmark_only {
                "synthetic".to_owned()
            } else {
                "world".to_owned()
            }
        } else {
            config.profile_scenario.clone()
        },
        frames: ordered.len(),
        warmup_frames: config.benchmark_warmup_frames,
        streaming_benchmark_route: route
            .as_deref()
            .map(|route| route_snapshot(route, profiler.elapsed_ms())),
        measurement_scope: route.as_ref().map(|_| {
            if config.streaming_benchmark_route_speed == Some(0.0) {
                "stationary_tail"
            } else {
                "route_motion"
            }
        }),
        synthetic_instances: if config.benchmark_only {
            config.synthetic_instances
        } else {
            0
        },
        elapsed_seconds: total_ms / 1000.0,
        average_fps,
        frame_ms_mean: mean,
        frame_ms_p50: p50,
        frame_ms_p95: p95,
        frame_ms_p99: p99,
        frame_ms_worst: worst,
        peak_process_memory_gib: (samples.peak_process_memory_gib > 0.0)
            .then_some(samples.peak_process_memory_gib),
        process_memory_growth_gib: memory_growth_gib,
        entity_count,
        system: system_snapshot.clone(),
        streaming: streaming.as_ref().map(|value| (*value).clone()),
        adaptive: runtime
            .as_ref()
            .and_then(|runtime| runtime.snapshot.clone()),
        renderer: renderer.clone(),
        render_world,
        render_pipelines,
        pacing: pacing_report,
        frame_budget,
        route: measurements.file_route.as_deref().map(RouteRunner::report),
        fingerprint: measurements.fingerprint.as_deref().cloned(),
        gpu_passes,
        scene,
        cpu,
        memory,
        thresholds: Thresholds {
            minimum_average_fps: config.accept_min_fps,
            maximum_p95_frame_ms: config.accept_p95_ms,
            maximum_memory_growth_gib: config.accept_max_memory_growth_gib,
            no_streaming_failures,
            commit_budget_respected,
            streaming_lifecycle_validated,
            renderer_path_active,
            screenshot_captured,
        },
        passed,
    };
    if let Some(parent) = config
        .benchmark_output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = fs::create_dir_all(parent)
    {
        error!(%error, "failed to create benchmark report directory");
        exit.write(AppExit::error());
        samples.finished = true;
        return;
    }
    let frame_metrics = match serde_json::to_value(&report) {
        Ok(value) => value,
        Err(error) => {
            error!(%error, "failed to serialize benchmark report");
            exit.write(AppExit::error());
            samples.finished = true;
            return;
        }
    };
    let bundle_system = system_snapshot.map(|value| SystemMetadata {
        os: value.os,
        kernel: value.kernel,
        cpu: value.cpu,
        core_count: value.core_count,
        memory: value.memory,
    });
    if let Err(error) = profiler.write_bundle(
        &config,
        &frame_metrics,
        streaming.as_deref(),
        &renderer,
        bundle_system,
        measurements.fingerprint.as_deref(),
    ) {
        error!(%error, "failed to write profiling bundle");
        exit.write(AppExit::error());
        samples.finished = true;
        return;
    }
    if let Some(path) = &config.benchmark_frame_times
        && let Err(error) = write_frame_times(path, &samples.frame_ms)
    {
        error!(%error, path = %path.display(), "failed to write benchmark frame times");
        exit.write(AppExit::error());
        samples.finished = true;
        return;
    }
    match serde_json::to_vec_pretty(&report)
        .map_err(std::io::Error::other)
        .and_then(|json| fs::write(&config.benchmark_output, json))
    {
        Ok(()) => info!(
            path = %config.benchmark_output.display(),
            average_fps,
            p95_ms = p95,
            passed,
            "acceptance benchmark complete"
        ),
        Err(error) => {
            error!(%error, "failed to write benchmark report");
            exit.write(AppExit::error());
            samples.finished = true;
            return;
        }
    }
    samples.finished = true;
    exit.write(if passed {
        AppExit::Success
    } else {
        AppExit::error()
    });
}

/// Scores the frame that just ended: its real-clock duration and the side that owned it (see
/// `frame_budget`). Runs just before [`collect_and_finish`] and follows it frame for frame, so the
/// frames scored are the frames the report measures; for a route run only the frames on the route
/// count, and the route's arrival is passed on as the end of the window.
///
/// The duration is the time from the start of the previous main frame to the start of this one, so
/// the sides that explain it are the previous cycle's: its main schedule and commit time, and the
/// render and extract time that ran in it (`RenderTimings::take_previous_cycle`).
fn record_frame_ledger(
    config: Res<EngineConfig>,
    real_time: Res<Time<Real>>,
    render_timings: Res<RenderTimings>,
    streaming: Option<Res<StreamingMetrics>>,
    route: Option<Res<RouteRunner>>,
    mut samples: ResMut<BenchmarkSamples>,
) {
    if samples.finished || samples.measurement_complete || !config.is_benchmark_run() {
        return;
    }
    let samples = &mut *samples;
    if route.as_deref().is_some_and(RouteRunner::finished) {
        samples.route_finished = true;
    }
    let ledger = &mut samples.ledger;
    ledger.frames_seen = ledger.frames_seen.saturating_add(1);
    let commit_ms = streaming
        .as_deref()
        .map(|metrics| ledger.commit_ms(metrics));
    // Taken on every frame, warm-up included, so the records of frames nobody scores do not pile up.
    let cycle = render_timings.take_previous_cycle();
    let scored = frame_is_scored(
        ledger.frames_seen,
        config.benchmark_warmup_frames,
        route.as_deref().map(RouteRunner::scoring),
    );
    let milliseconds = real_time.delta_secs_f64() * 1000.0;
    if scored && milliseconds.is_finite() && milliseconds > 0.0 {
        let cycle = cycle.unwrap_or_default();
        let sides = FrameSides::from_samples(
            cycle.main_ms,
            cycle.extract_ms,
            cycle.render_ms,
            cycle.acquire_ms,
            ledger.previous_upload_ms,
        );
        ledger.scored.push(ScoredFrame {
            ms: milliseconds,
            owner: attribute(milliseconds, &sides),
            wait_ms: cycle.wait_ms,
        });
    }
    ledger.previous_upload_ms = commit_ms;
}

/// Whether a frame belongs to the frame budget's window. A run with no route scores the frames
/// after the warm-up, as the rest of the report does. A route run scores the route's own frames
/// and nothing else: the route's start (waiting for the world, or none at all) is what opens the
/// window, so the warm-up frames are not trimmed on top of it, which would drop the first frames
/// of a route that starts at once.
fn frame_is_scored(frames_seen: u32, warmup_frames: u32, on_route: Option<bool>) -> bool {
    on_route.unwrap_or(frames_seen > warmup_frames)
}

fn memory_settle_seconds(config: &EngineConfig) -> f64 {
    config
        .benchmark_duration_secs
        .map_or(0.0, |duration| (duration * 0.1).min(60.0))
}

fn diagnostic_value(
    store: &DiagnosticsStore,
    path: &bevy::diagnostic::DiagnosticPath,
) -> Option<f64> {
    store.get(path).and_then(|diagnostic| diagnostic.value())
}

fn physics_fixture_ready(selected: bool, metrics: Option<&StreamingMetrics>) -> bool {
    !selected
        || metrics.is_some_and(|value| {
            value.physics_fixture_validated && value.physics_fixture_failures == 0
        })
}

fn no_runtime_failures(
    streaming: Option<&StreamingMetrics>,
    require_material_fixture: bool,
    require_terrain_fixture: bool,
    require_transform_fixture: bool,
) -> bool {
    streaming.map_or(
        !require_material_fixture && !require_terrain_fixture && !require_transform_fixture,
        |value| {
            value.failed_cells == 0
                && value.asset_load_failures == 0
                && value.material_validation_failures == 0
                && value.diagnostic_fallbacks == 0
                && value.terrain_validation_failures == 0
                && value.water_validation_failures == 0
                && value.transform_bounds_validation_failures == 0
                && value.streaming_invariant_failures == 0
                && value.duplicate_cell_roots == 0
                && value.orphaned_cell_roots == 0
                && value.missing_cell_roots == 0
                && value.out_of_range_cell_roots == 0
                && value.streaming_fixture_failures == 0
                && (!require_material_fixture || value.canonical_fixture_validated)
                && (!require_terrain_fixture || value.terrain_water_fixture_validated)
                && (!require_transform_fixture || value.transform_bounds_fixture_validated)
        },
    )
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Writes the measured frame times as CSV (`frame,ms`), one row per frame after the warm-up, in
/// the order they were measured: the series behind the report's mean and percentiles.
fn write_frame_times(path: &std::path::Path, frame_ms: &[f64]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, frame_times_csv(frame_ms))
}

fn frame_times_csv(frame_ms: &[f64]) -> String {
    let mut csv = String::with_capacity(16 + frame_ms.len() * 12);
    csv.push_str(
        "frame,ms
",
    );
    for (frame, milliseconds) in frame_ms.iter().enumerate() {
        csv.push_str(&format!(
            "{frame},{milliseconds:.4}
"
        ));
    }
    csv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signal-timing app used by the gate test: minimal plugins, an asset store to add to,
    /// and a `ProfilingState` for the plugin (or the test) to record into.
    fn timing_app(measures_pacing: bool) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Mesh>()
            .init_resource::<ProfilingState>();
        install_schedule_timing(&mut app, Some(measures_pacing));
        app.finish();
        app.cleanup();
        app.update();
        app.world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Mesh::from(Cuboid::default()));
        app.update();
        app.update();
        app
    }

    /// A normal play session is not a benchmark, so it installs neither the marker schedules nor
    /// the asset-arrival counting; a benchmark run installs both.
    #[test]
    fn schedule_and_asset_instrumentation_is_benchmark_only() {
        let profiler = timing_app(false);
        let profiler = profiler.world().resource::<ProfilingState>();
        assert!(
            profiler.span_samples("main_schedule/Update").is_empty(),
            "a play session must not add the schedule markers"
        );
        assert!(
            profiler.span_samples("assets_added/mesh").is_empty(),
            "a play session must not count asset arrivals"
        );

        let profiler = timing_app(true);
        let profiler = profiler.world().resource::<ProfilingState>();
        assert!(
            !profiler.span_samples("main_schedule/Update").is_empty(),
            "a benchmark run times the schedules"
        );
        assert!(
            profiler
                .span_samples("assets_added/mesh")
                .iter()
                .sum::<f64>()
                >= 1.0,
            "a benchmark run counts asset arrivals"
        );
    }

    /// The plugin reads the run's `EngineConfig` while it builds, so the config must already be
    /// in the world; this drives that real path rather than the helper.
    #[test]
    fn the_plugin_installs_schedule_timing_from_the_inserted_config() {
        let installed = |config: Option<EngineConfig>| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AssetPlugin::default()));
            if let Some(config) = config {
                app.insert_resource(config);
            }
            app.add_plugins(AcceptanceMetricsPlugin);
            app.is_plugin_added::<crate::schedule_timing::ScheduleTimingPlugin>()
        };
        assert!(installed(Some(EngineConfig {
            benchmark_frames: Some(60),
            ..EngineConfig::default()
        })));
        assert!(!installed(Some(EngineConfig::default())));
        assert!(!installed(None), "no config means no measured run");
    }

    /// The GPU-memory diagnostics are measurement data: an ordinary play session installs none of
    /// them, and a benchmark or profile run installs all of them.
    #[test]
    fn gpu_memory_diagnostics_are_installed_only_for_measured_runs() {
        let installed = |config: EngineConfig| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AssetPlugin::default()));
            app.insert_resource(config);
            app.add_plugins(AcceptanceMetricsPlugin);
            (
                app.is_plugin_added::<MeshAllocatorDiagnosticPlugin>(),
                app.is_plugin_added::<RenderAssetDiagnosticPlugin<RenderMesh>>(),
                app.is_plugin_added::<RenderAssetDiagnosticPlugin<GpuImage>>(),
                app.is_plugin_added::<ErasedRenderAssetDiagnosticPlugin<
                    MeshMaterial3d<StandardMaterial>,
                >>(),
            )
        };
        assert_eq!(
            installed(EngineConfig::default()),
            (false, false, false, false),
            "a play session must install none of the GPU-memory diagnostics"
        );
        assert_eq!(
            installed(EngineConfig {
                benchmark_frames: Some(60),
                ..EngineConfig::default()
            }),
            (true, true, true, true)
        );
        assert_eq!(
            installed(EngineConfig {
                profile_output_dir: Some("profiles/run-1".into()),
                ..EngineConfig::default()
            }),
            (true, true, true, true)
        );
    }

    /// A run with no render world never measures the GPU-memory counters, and the report must not
    /// turn the zeros Bevy's plugins publish into counts.
    ///
    /// This drives the real install: Bevy's own main-world `PreUpdate` systems run every frame even
    /// with no render app and publish their atomics at their initial zero, which is why a stored
    /// `Some(0.0)` cannot be read as a measurement.
    #[test]
    fn gpu_memory_counters_are_not_measured_without_a_render_world() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        install_gpu_memory_diagnostics(&mut app, Some(true));
        app.update();

        let probe = app.world().resource::<RenderMeasurementProbe>();
        assert!(
            !probe.measured(),
            "with no render app there is no extraction to mark the probe"
        );
        let diagnostics = app.world().resource::<DiagnosticsStore>();
        assert_eq!(
            diagnostics
                .get(MeshAllocatorDiagnosticPlugin::slabs_diagnostic_path())
                .and_then(|diagnostic| diagnostic.value()),
            Some(0.0),
            "Bevy publishes the counter's initial zero whether or not anything measured it"
        );

        let mut profiler = ProfilingState::default();
        profiler.sample_frame(diagnostics, None, true, probe.measured());
        let gpu = profiler.gpu_memory(None);
        assert_eq!(
            gpu.mesh_allocator_slabs, None,
            "a zero the render world never produced is not a count"
        );
        assert_eq!(gpu.measured_frames, 0);
        assert_eq!(
            gpu.unavailable["mesh_allocator_slabs"],
            "no renderer in this run"
        );
    }

    #[test]
    fn route_report_clock_keeps_uncapped_real_frame_intervals() {
        use crate::{app::StreamingBenchmarkPhase, profiling::ProfilingPlugin};
        use bevy::time::TimeUpdateStrategy;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig {
                streaming_benchmark_route_speed: Some(370.0),
                ..default()
            })
            .insert_resource(crate::app::StreamingBenchmarkRoute {
                phase: StreamingBenchmarkPhase::Moving,
                movement_started_real_seconds: Some(0.0),
                ..default()
            })
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(600),
            ))
            .init_resource::<DiagnosticsStore>()
            .init_resource::<RendererMetrics>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .add_plugins((AcceptanceMetricsPlugin, ProfilingPlugin));
        app.update();
        app.update();
        let samples = app.world().resource::<BenchmarkSamples>();
        assert_eq!(samples.frame_ms, vec![600.0]);
        assert_eq!(samples.measured_seconds, 0.6);
        assert!(app.world().resource::<Time>().delta_secs_f64() < 0.6);
    }

    /// A `--route` run passes acceptance only when its route has finished. A window closed by its
    /// limit with the route still walking is reported as partial and fails; a finished route passes.
    #[test]
    fn a_file_route_passes_only_once_it_has_finished() {
        let waypoints = r#"{"worldspace": 60, "speed": 100, "waypoints": [[0, 0], [100, 0]]}"#;
        let route_at = |json: &str| {
            crate::route::RouteRunner::new(
                crate::route::RoutePlan::parse(json).unwrap(),
                crate::route::RouteGround::default(),
            )
        };
        let directory = tempfile::tempdir().unwrap();
        // The world is ready on the first frame, which stands still; the window closes at once.
        let mut partial = route_at(waypoints);
        partial.step(1.0, true, 0.0);
        let mut app = acceptance_report_app(directory.path(), None, std::time::Duration::ZERO);
        app.insert_resource(partial).update();
        let report = written_report(directory.path());
        assert_eq!(report["route"]["completed"], false);
        assert_eq!(report["passed"], false, "a partial route cannot pass");

        let directory = tempfile::tempdir().unwrap();
        let mut finished = route_at(waypoints);
        finished.step(0.0, true, 0.0);
        finished.step(2.0, true, 2000.0);
        let mut app = acceptance_report_app(directory.path(), None, std::time::Duration::ZERO);
        app.insert_resource(finished).update();
        let report = written_report(directory.path());
        assert_eq!(report["route"]["completed"], true);
        assert_eq!(
            report["passed"], true,
            "a finished route passes with the other gates"
        );
    }

    /// Whether the world counts as ready on each frame of the file-route fixtures below.
    #[derive(Resource, Default)]
    struct FixtureWorldReady(bool);

    /// Steps the file route the way `route::drive_route` does, from the same real clock. The camera
    /// is not needed for the route's own timing, so the fixture steps the runner directly.
    fn step_fixture_route(
        mut runner: ResMut<crate::route::RouteRunner>,
        real_time: Res<Time<Real>>,
        ready: Res<FixtureWorldReady>,
    ) {
        runner.step(
            real_time.delta_secs(),
            ready.0,
            real_time.elapsed_secs_f64() * 1000.0,
        );
    }

    /// A file-route app with the real measurement systems: the route steps in `PreUpdate`, and the
    /// report is taken in `Last` as in the engine. `directory` receives the report.
    fn file_route_app(directory: &std::path::Path, route: &str, duration_secs: Option<f64>) -> App {
        let plan = crate::route::RoutePlan::parse(route).unwrap();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig {
                benchmark_frames: Some(1000),
                benchmark_duration_secs: duration_secs,
                benchmark_output: directory.join("benchmark-report.json"),
                ..default()
            })
            .insert_resource(crate::route::RouteRunner::new(
                plan,
                crate::route::RouteGround::default(),
            ))
            .insert_resource(FixtureWorldReady(false))
            .init_resource::<DiagnosticsStore>()
            .init_resource::<RendererMetrics>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<ProfilingState>()
            .add_plugins(AcceptanceMetricsPlugin)
            .add_systems(PreUpdate, step_fixture_route);
        app
    }

    /// The route's frames are measured from its first full moving frame. A ten-second wait for the
    /// world and the activation frame that follows it are not frames of the route: the summary, the
    /// frame budget and the report's `frames` count the same four moving frames, each 100 ms.
    #[test]
    fn a_file_route_measures_its_moving_frames_not_the_wait_or_the_activation() {
        use bevy::time::TimeUpdateStrategy;

        let directory = tempfile::tempdir().unwrap();
        // 500 units at 1000 units per second: five moving frames of 100 ms reach the end.
        let mut app = file_route_app(
            directory.path(),
            r#"{"worldspace": 60, "speed": 1000, "waypoints": [[0, 0], [500, 0]]}"#,
            None,
        );
        let frames = [(10.0, false), (10.0, true)]
            .into_iter()
            .chain(std::iter::repeat_n((0.1, true), 8));
        for (seconds, ready) in frames {
            app.insert_resource(FixtureWorldReady(ready))
                .insert_resource(TimeUpdateStrategy::ManualDuration(
                    std::time::Duration::from_secs_f64(seconds),
                ));
            app.update();
            if app.world().resource::<BenchmarkSamples>().finished {
                break;
            }
        }
        let samples = app.world().resource::<BenchmarkSamples>();
        assert_eq!(samples.frame_ms.len(), 4, "{:?}", samples.frame_ms);
        for milliseconds in &samples.frame_ms {
            assert!((milliseconds - 100.0).abs() < 1e-6, "{milliseconds}");
        }
        let report = written_report(directory.path());
        assert_eq!(report["route"]["completed"], true);
        assert_eq!(report["frames"], 4);
        assert_eq!(report["frame_budget"]["frames"], 4);
    }

    /// A `--route` run with a duration limit ends at that real-time deadline even if the route never
    /// became ready, and reports the route as not started and not completed. Acceptance fails.
    #[test]
    fn a_file_route_that_never_becomes_ready_ends_at_its_deadline_and_fails() {
        use bevy::time::TimeUpdateStrategy;

        let directory = tempfile::tempdir().unwrap();
        let mut app = file_route_app(
            directory.path(),
            r#"{"worldspace": 60, "speed": 400, "waypoints": [[0, 0], [1000, 0]]}"#,
            Some(1.0),
        );
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(600),
        ));
        for _ in 0..4 {
            app.update();
            if app.world().resource::<BenchmarkSamples>().finished {
                break;
            }
        }
        let report = written_report(directory.path());
        assert_eq!(report["route"]["started"], false);
        assert_eq!(report["route"]["completed"], false);
        assert_eq!(report["passed"], false, "an unready route cannot pass");
    }

    /// A route that started but is still walking when its real-time deadline arrives is reported as
    /// partial and fails, however fast its frames were.
    #[test]
    fn a_file_route_still_walking_at_its_deadline_is_reported_as_partial() {
        use bevy::time::TimeUpdateStrategy;

        let directory = tempfile::tempdir().unwrap();
        let mut app = file_route_app(
            directory.path(),
            r#"{"worldspace": 60, "speed": 10, "waypoints": [[0, 0], [1000, 0]]}"#,
            Some(1.0),
        );
        app.insert_resource(FixtureWorldReady(true))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(600),
            ));
        for _ in 0..4 {
            app.update();
            if app.world().resource::<BenchmarkSamples>().finished {
                break;
            }
        }
        let report = written_report(directory.path());
        assert_eq!(report["route"]["started"], true);
        assert_eq!(report["route"]["completed"], false);
        assert_eq!(report["passed"], false);
    }

    /// A route whose CPU never settles still finalises at its real-time deadline: the profile
    /// bundle and the report are written, and acceptance fails because the route did not finish.
    #[test]
    fn unsettled_route_writes_bundle_and_fails_at_its_real_time_deadline() {
        use crate::{app::StreamingBenchmarkRoute, profiling::ProfilingPlugin};
        use bevy::time::TimeUpdateStrategy;

        let directory = tempfile::tempdir().unwrap();
        let report_path = directory.path().join("report.json");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig {
                streaming_benchmark_route_speed: Some(370.0),
                benchmark_duration_secs: Some(1.0),
                benchmark_output: report_path.clone(),
                profile_output_dir: Some(directory.path().join("profile")),
                ..default()
            })
            .insert_resource(StreamingBenchmarkRoute::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(600),
            ))
            .init_resource::<DiagnosticsStore>()
            .init_resource::<RendererMetrics>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .add_plugins((AcceptanceMetricsPlugin, ProfilingPlugin));
        // The deadline is 1 s of real time, so the second 600 ms update reaches it.
        for _ in 0..4 {
            app.update();
        }
        assert!(
            directory.path().join("profile/metadata.json").is_file(),
            "the profile bundle must be written at the deadline"
        );
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&report_path).expect("report written")).unwrap();
        assert_eq!(
            report["passed"], false,
            "an unfinished route cannot pass acceptance"
        );
        assert_eq!(report["streaming_benchmark_route"]["phase"], "waiting");
        assert!(app.world().resource::<BenchmarkSamples>().finished);
    }

    #[test]
    fn route_windows_exclude_wait_and_moving_tail_but_measure_stationary_tail() {
        use crate::app::{StreamingBenchmarkPhase, StreamingBenchmarkRoute};

        let mut route = StreamingBenchmarkRoute::default();
        assert_eq!(route_window(&route), BenchmarkWindow::Warmup);
        route.phase = StreamingBenchmarkPhase::Moving;
        route.movement_started_real_seconds = Some(5.0);
        assert_eq!(route_window(&route), BenchmarkWindow::Measured);
        route.phase = StreamingBenchmarkPhase::Tail;
        route.movement_finished_real_seconds = Some(50.0);
        assert_eq!(route_window(&route), BenchmarkWindow::Settlement);
        route.movement_finished_real_seconds = Some(5.0);
        assert_eq!(route_window(&route), BenchmarkWindow::Measured);
        route.phase = StreamingBenchmarkPhase::Finished;
        assert_eq!(route_window(&route), BenchmarkWindow::Settlement);
        route.latest_real_seconds = 8.0;
        route.cpu_settled_real_seconds = Some(2.0);
        let snapshot = route_snapshot(&route, 10_000.0);
        assert_eq!(snapshot.cpu_settled_elapsed_ms, Some(4000.0));
        assert_eq!(snapshot.movement_started_elapsed_ms, Some(7000.0));
    }

    /// Verify trace frame ordering across all benchmark windows with GPU inventory on and off.
    #[test]
    fn trace_keeps_warmup_and_capture_settlement_on_one_frame_clock() {
        for gpu_inventory_enabled in [false, true] {
            check_trace_windows(gpu_inventory_enabled);
        }
    }

    /// Run a short benchmark and verify frame windows, span attribution and exported frame IDs.
    fn check_trace_windows(gpu_inventory_enabled: bool) {
        use crate::profiling::ProfilingPlugin;
        use bevy::time::TimeUpdateStrategy;

        let directory = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            profile_output_dir: Some(directory.path().join("profile")),
            profile_gpu_inventory: gpu_inventory_enabled,
            benchmark_output: directory.path().join("report.json"),
            benchmark_frames: Some(1),
            benchmark_warmup_frames: 2,
            acceptance_screenshot: Some(directory.path().join("pending.png")),
            ..default()
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(config)
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(300),
            ))
            .init_resource::<DiagnosticsStore>()
            .init_resource::<RendererMetrics>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .add_plugins((AcceptanceMetricsPlugin, ProfilingPlugin))
            .add_systems(Update, |mut profiler: ResMut<ProfilingState>| {
                profiler.record_ms("test/work", 1.0);
                profiler.record_ms("test/work", 2.0);
                profiler.record_completed_latency_micros("test/ready", 10_000);
                profiler.event("test", "work", None);
            });
        for _ in 0..5 {
            app.update();
        }
        let profiler = app.world().resource::<ProfilingState>();
        let report = profiler.trace.as_ref().unwrap().report();
        assert_eq!(report.gpu_inventory_enabled, gpu_inventory_enabled);
        assert_eq!(report.samples.len(), 5);
        let windows: Vec<_> = report
            .samples
            .iter()
            .map(|sample| sample.benchmark_window)
            .collect();
        assert_eq!(
            windows,
            vec![
                BenchmarkWindow::Warmup,
                BenchmarkWindow::Warmup,
                BenchmarkWindow::Measured,
                BenchmarkWindow::Settlement,
                BenchmarkWindow::Settlement
            ]
        );
        for (index, sample) in report.samples.iter().enumerate() {
            assert_eq!(sample.main_frame, index as u64 + 1);
            assert_eq!(sample.cpu_spans_ms["test/work"], 3.0);
            assert!(!sample.cpu_spans_ms.contains_key("test/ready"));
            assert_eq!(sample.completion_latencies_ms["test/ready"], 10.0);
            if gpu_inventory_enabled {
                assert!(!sample.gpu.as_ref().unwrap().render_available);
            } else {
                assert!(sample.gpu.is_none());
            }
        }
        // The real interval is preserved even when virtual time caps a long frame.
        assert!((report.samples[4].delta_ms - 300.0).abs() < 0.001);
        let benchmark = app.world().resource::<BenchmarkSamples>();
        assert_eq!(benchmark.frames_seen, 3);
        assert_eq!(benchmark.frame_ms.len(), 1);
        assert!(benchmark.measurement_complete);
        assert!(!benchmark.finished);

        std::fs::write(directory.path().join("pending.png"), []).unwrap();
        app.update();
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("profile/streaming-frames.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["samples"].as_array().unwrap().len(), 6);
        let timeline: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("profile/streaming.json")).unwrap(),
        )
        .unwrap();
        for (index, event) in timeline["timeline"].as_array().unwrap().iter().enumerate() {
            assert_eq!(event["frame"], index as u64 + 1);
        }
    }

    /// Verify camera motion uses stable coordinates and resets across worldspace changes.
    #[test]
    fn camera_motion_ignores_rebases_and_quaternion_sign() {
        let entity = Entity::from_raw_u32(1).unwrap();
        let mut previous = None;
        let first = observe_camera(
            entity,
            &GlobalTransform::from_xyz(10.0, 20.0, 30.0),
            1,
            IVec2::ZERO,
            &mut previous,
        );
        assert_eq!(first.motion, CameraMotion::Unavailable);
        let rebased = GlobalTransform::from(
            Transform::from_xyz(10.0 - CELL_SIZE, 20.0, 30.0 + CELL_SIZE)
                .with_rotation(-Quat::IDENTITY),
        );
        let next = observe_camera(entity, &rebased, 1, IVec2::ONE, &mut previous);
        assert_eq!(next.world_position, first.world_position);
        assert_eq!(next.motion, CameraMotion::Stationary);
        let moved = observe_camera(
            entity,
            &GlobalTransform::from_xyz(11.0, 20.0, 30.0),
            1,
            IVec2::ZERO,
            &mut previous,
        );
        assert_eq!(moved.motion, CameraMotion::Translated);
        let changed_space = observe_camera(
            entity,
            &GlobalTransform::IDENTITY,
            2,
            IVec2::ZERO,
            &mut previous,
        );
        assert_eq!(changed_space.motion, CameraMotion::Unavailable);
    }

    #[test]
    fn trace_uses_the_active_worldspace_and_resets_camera_history_across_interiors() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig {
                profile_output_dir: Some(directory.path().join("profile")),
                ..default()
            })
            .insert_resource(RenderOrigin(IVec2::new(4, -3)))
            .insert_resource(ActiveSpace {
                worldspace_id: Some(42),
                interior: None,
            })
            .init_resource::<DiagnosticsStore>()
            .init_resource::<RendererMetrics>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .add_plugins((AcceptanceMetricsPlugin, crate::profiling::ProfilingPlugin));
        app.world_mut()
            .spawn((StreamingCamera, GlobalTransform::from_xyz(10.0, 20.0, 30.0)));
        app.update();
        app.update();
        app.world_mut().resource_mut::<ActiveSpace>().interior = Some(77);
        app.update();
        app.world_mut().resource_mut::<ActiveSpace>().interior = None;
        app.update();
        let profiler = app.world().resource::<ProfilingState>();
        let report = profiler.trace.as_ref().unwrap().report();
        let samples = &report.samples;
        assert_eq!(samples[0].camera.as_ref().unwrap().worldspace_id, 42);
        assert_eq!(
            samples[1].camera.as_ref().unwrap().motion,
            CameraMotion::Stationary
        );
        assert!(
            samples[2].camera.is_none(),
            "interior coordinates have no exterior identity"
        );
        assert_eq!(
            samples[3].camera.as_ref().unwrap().motion,
            CameraMotion::Unavailable
        );
    }

    #[test]
    fn frame_times_are_written_in_measured_order() {
        assert_eq!(
            frame_times_csv(&[4.0, 3.25, 16.6667]),
            "frame,ms
0,4.0000
1,3.2500
2,16.6667
"
        );
        assert_eq!(
            frame_times_csv(&[]),
            "frame,ms
"
        );
    }

    #[test]
    fn parses_the_frame_times_path() {
        let config = EngineConfig::run_from_args(
            ["--benchmark-frame-times", "out/frames.csv"]
                .into_iter()
                .map(str::to_owned),
        );
        assert_eq!(
            config.benchmark_frame_times.as_deref(),
            Some(std::path::Path::new("out/frames.csv"))
        );
    }

    #[test]
    fn physics_fixture_benchmark_requires_validation_without_failures() {
        let mut metrics = StreamingMetrics::default();
        assert!(physics_fixture_ready(false, None));
        assert!(!physics_fixture_ready(true, None));
        assert!(!physics_fixture_ready(true, Some(&metrics)));
        metrics.physics_fixture_validated = true;
        assert!(physics_fixture_ready(true, Some(&metrics)));
        metrics.physics_fixture_failures = 1;
        assert!(!physics_fixture_ready(true, Some(&metrics)));
    }

    #[test]
    fn calculates_percentiles_at_the_shared_rank() {
        let samples: Vec<_> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&samples, 0.50), 51.0);
        assert_eq!(percentile(&samples, 0.95), 96.0);
        assert_eq!(percentile(&samples, 0.99), 100.0);
    }

    /// A report without streaming carries no pacing fields at all: `#[serde(flatten)]` on the
    /// `None` must not fail or leave a `null` behind.
    #[test]
    fn a_report_without_pacing_serialises_no_pacing_fields() {
        #[derive(Serialize)]
        struct Flattened {
            frames: usize,
            #[serde(flatten)]
            pacing: Option<PacingReport>,
        }
        let value = serde_json::to_value(Flattened {
            frames: 3,
            pacing: None,
        })
        .expect("a report with no pacing block must serialise");
        assert_eq!(value, serde_json::json!({ "frames": 3 }));
        let value = serde_json::to_value(Flattened {
            frames: 3,
            pacing: Some(PacingReport {
                world_ready_reached: true,
                ..Default::default()
            }),
        })
        .unwrap();
        assert_eq!(value["world_ready_reached"], serde_json::json!(true));
        assert_eq!(value["frames"], serde_json::json!(3));
    }

    /// A route run scores from the route's start, with no warm-up trimmed: a route that starts on
    /// the first frame loses none of its frames. Without a route the warm-up is dropped.
    #[test]
    fn route_runs_score_from_the_route_start_without_a_warmup_trim() {
        // No route: frames 1..=60 are warm-up, 61 is the first scored.
        assert!(!frame_is_scored(60, 60, None));
        assert!(frame_is_scored(61, 60, None));
        // A route that has not started, or is on its first frame, scores nothing, whatever the
        // frame number.
        assert!(!frame_is_scored(500, 60, Some(false)));
        // A route scoring on frame 3 (it started on frame 2) is scored inside the warm-up.
        assert!(frame_is_scored(3, 60, Some(true)));
        assert!(frame_is_scored(500, 60, Some(true)));
    }

    #[test]
    fn settles_memory_after_ten_percent_capped_at_one_minute() {
        let mut config = EngineConfig {
            benchmark_duration_secs: Some(120.0),
            ..default()
        };
        assert_eq!(memory_settle_seconds(&config), 12.0);
        config.benchmark_duration_secs = Some(600.0);
        assert_eq!(memory_settle_seconds(&config), 60.0);
        config.benchmark_duration_secs = Some(1800.0);
        assert_eq!(memory_settle_seconds(&config), 60.0);
        config.benchmark_duration_secs = None;
        assert_eq!(memory_settle_seconds(&config), 0.0);
    }

    #[test]
    fn rejects_streaming_or_asset_load_failures() {
        assert!(no_runtime_failures(None, false, false, false));
        assert!(no_runtime_failures(
            Some(&StreamingMetrics::default()),
            false,
            false,
            false
        ));

        let streaming_failure = StreamingMetrics {
            failed_cells: 1,
            ..default()
        };
        assert!(!no_runtime_failures(
            Some(&streaming_failure),
            false,
            false,
            false
        ));

        let asset_failure = StreamingMetrics {
            asset_load_failures: 1,
            ..default()
        };
        assert!(!no_runtime_failures(
            Some(&asset_failure),
            false,
            false,
            false
        ));

        let validation_failure = StreamingMetrics {
            material_validation_failures: 1,
            ..default()
        };
        assert!(!no_runtime_failures(
            Some(&validation_failure),
            false,
            false,
            false
        ));

        let fallback = StreamingMetrics {
            diagnostic_fallbacks: 1,
            ..default()
        };
        assert!(!no_runtime_failures(Some(&fallback), false, false, false));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics {
                terrain_validation_failures: 1,
                ..default()
            }),
            false,
            false,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics {
                water_validation_failures: 1,
                ..default()
            }),
            false,
            false,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics {
                transform_bounds_validation_failures: 1,
                ..default()
            }),
            false,
            false,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics::default()),
            true,
            false,
            false
        ));
        assert!(no_runtime_failures(
            Some(&StreamingMetrics {
                canonical_fixture_validated: true,
                ..default()
            }),
            true,
            false,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics::default()),
            false,
            true,
            false
        ));
        assert!(no_runtime_failures(
            Some(&StreamingMetrics {
                terrain_water_fixture_validated: true,
                ..default()
            }),
            false,
            true,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics::default()),
            false,
            false,
            true
        ));
        assert!(no_runtime_failures(
            Some(&StreamingMetrics {
                transform_bounds_fixture_validated: true,
                ..default()
            }),
            false,
            false,
            true
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics {
                orphaned_cell_roots: 1,
                ..default()
            }),
            false,
            false,
            false
        ));
        assert!(!no_runtime_failures(
            Some(&StreamingMetrics {
                out_of_range_cell_roots: 1,
                ..default()
            }),
            false,
            false,
            false
        ));
    }

    /// The `collect_and_finish` app: a measurement that has already completed, a renderer on its
    /// final path, and the screenshot at `screenshot`, with the screenshot wait seeded `waited` ago
    /// rather than slept through. Every threshold but the screenshot's passes, so `passed` says
    /// what the screenshot gate alone decided. The report goes to `directory`.
    fn acceptance_report_app(
        directory: &std::path::Path,
        screenshot: Option<&std::path::Path>,
        waited: std::time::Duration,
    ) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig {
                benchmark_frames: Some(1),
                benchmark_output: directory.join("benchmark-report.json"),
                acceptance_screenshot: screenshot.map(std::path::Path::to_path_buf),
                accept_min_fps: 0.0,
                ..default()
            })
            .insert_resource(BenchmarkSamples {
                measurement_complete: true,
                screenshot_wait_started: Some(std::time::Instant::now() - waited),
                ..default()
            })
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
            .init_resource::<RenderTimings>()
            .init_resource::<DiagnosticsStore>()
            // The report counts the CPU-side mesh and image data, so `collect_and_finish` needs
            // both asset stores even when nothing was loaded.
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<Image>>()
            .insert_resource(ProfilingState::default())
            .add_systems(Update, collect_and_finish);
        app
    }

    fn written_report(directory: &std::path::Path) -> serde_json::Value {
        let report = fs::read_to_string(directory.join("benchmark-report.json")).unwrap();
        serde_json::from_str(&report).unwrap()
    }

    /// A missing acceptance screenshot fails the run once the 10-second wait is past, instead of
    /// passing on it or hanging; the same run with the PNG written passes. The wait is seeded past
    /// rather than slept through.
    #[test]
    fn the_screenshot_wait_fails_a_missing_capture_instead_of_passing_it() {
        use bevy::app::AppExit;
        use bevy::ecs::message::Messages;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("acceptance.png");

        // The wait has only just started: the run waits for the screenshot instead of finishing.
        let mut waiting = acceptance_report_app(directory.path(), Some(&missing), Duration::ZERO);
        waiting.update();
        assert!(!waiting.world().resource::<BenchmarkSamples>().finished);
        assert!(!directory.path().join("benchmark-report.json").exists());

        // Past the wait: the run finishes with the screenshot gate failed, and says so.
        let mut expired =
            acceptance_report_app(directory.path(), Some(&missing), Duration::from_secs(11));
        expired.update();
        assert!(expired.world().resource::<BenchmarkSamples>().finished);
        let exits: Vec<AppExit> = expired
            .world()
            .resource::<Messages<AppExit>>()
            .iter_current_update_messages()
            .cloned()
            .collect();
        assert_eq!(
            exits,
            vec![AppExit::error()],
            "a missing screenshot fails the run"
        );
        let report = written_report(directory.path());
        assert_eq!(report["thresholds"]["screenshot_captured"], false);
        assert_eq!(report["passed"], false);

        // The screenshot the capture system queues is what that gate reads: write it and the same
        // run passes without waiting at all.
        fs::write(&missing, b"png").unwrap();
        let mut captured = acceptance_report_app(directory.path(), Some(&missing), Duration::ZERO);
        captured.update();
        let report = written_report(directory.path());
        assert_eq!(report["thresholds"]["screenshot_captured"], true);
        assert_eq!(report["passed"], true);
    }

    /// A report that cannot be written fails the run even when every measured threshold passed.
    /// Otherwise a full disk, a read-only directory or a path that is a directory exits as a pass
    /// with no report for acceptance to read.
    #[test]
    fn an_unwritable_report_fails_the_run() {
        use bevy::app::AppExit;
        use bevy::ecs::message::Messages;

        let directory = tempfile::tempdir().unwrap();
        let mut app = acceptance_report_app(directory.path(), None, std::time::Duration::ZERO);
        // The report path is the temp directory itself: `create_dir_all` on its parent succeeds
        // and `fs::write` on the directory always fails, as it does on a disk that cannot take
        // the file.
        app.world_mut()
            .resource_mut::<EngineConfig>()
            .benchmark_output = directory.path().to_path_buf();
        app.update();
        assert!(app.world().resource::<BenchmarkSamples>().finished);
        let exits: Vec<AppExit> = app
            .world()
            .resource::<Messages<AppExit>>()
            .iter_current_update_messages()
            .cloned()
            .collect();
        assert_eq!(
            exits,
            vec![AppExit::error()],
            "a report that was not written fails the run"
        );
        assert!(
            fs::read_dir(directory.path()).unwrap().next().is_none(),
            "the failed run left a report behind"
        );
    }
}
