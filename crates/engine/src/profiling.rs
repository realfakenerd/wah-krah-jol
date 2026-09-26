use crate::{config::EngineConfig, render::RendererMetrics, streaming::StreamingMetrics};
use bevy::{diagnostic::DiagnosticsStore, prelude::*};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SAMPLES_PER_METRIC: usize = 200_000;
const MAX_TIMELINE_EVENTS: usize = 100_000;

pub struct ProfilingPlugin;

impl Plugin for ProfilingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ProfilingState>()
            .add_systems(Update, sample_scene_inventory);
    }
}

#[derive(Resource)]
pub struct ProfilingState {
    started: Instant,
    frame: u64,
    cpu_spans_ms: BTreeMap<String, Vec<f64>>,
    render_metrics: BTreeMap<String, Vec<f64>>,
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
    timeline: Vec<TimelineEvent>,
    memory: Vec<MemorySample>,
}

impl Default for ProfilingState {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            frame: 0,
            cpu_spans_ms: BTreeMap::new(),
            render_metrics: BTreeMap::new(),
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            timeline: Vec::new(),
            memory: Vec::new(),
        }
    }
}

impl ProfilingState {
    pub fn record_elapsed(&mut self, name: impl Into<String>, started: Instant) {
        self.record_ms(name, started.elapsed().as_secs_f64() * 1000.0);
    }

    pub fn record_micros(&mut self, name: impl Into<String>, micros: u64) {
        self.record_ms(name, micros as f64 / 1000.0);
    }

    pub fn record_ms(&mut self, name: impl Into<String>, value: f64) {
        push_bounded(self.cpu_spans_ms.entry(name.into()).or_default(), value);
    }

    pub fn increment(&mut self, name: impl Into<String>, amount: u64) {
        let counter = self.counters.entry(name.into()).or_default();
        *counter = counter.saturating_add(amount);
    }

    pub fn set_gauge(&mut self, name: impl Into<String>, value: f64) {
        self.gauges.insert(name.into(), value);
    }

    pub fn event(
        &mut self,
        subject: impl Into<String>,
        stage: impl Into<String>,
        value_ms: Option<f64>,
    ) {
        if self.timeline.len() >= MAX_TIMELINE_EVENTS {
            return;
        }
        self.timeline.push(TimelineEvent {
            elapsed_ms: self.started.elapsed().as_secs_f64() * 1000.0,
            frame: self.frame,
            subject: subject.into(),
            stage: stage.into(),
            value_ms,
        });
    }

    pub fn sample_frame(
        &mut self,
        diagnostics: &DiagnosticsStore,
        process_memory_gib: Option<f64>,
    ) {
        self.frame = self.frame.saturating_add(1);
        for diagnostic in diagnostics.iter() {
            let path = diagnostic.path().as_str();
            if path.starts_with("render/")
                && let Some(value) = diagnostic.value()
            {
                push_bounded(
                    self.render_metrics.entry(path.to_owned()).or_default(),
                    value,
                );
            }
        }
        if self.frame.is_multiple_of(60)
            && let Some(process_memory_gib) = process_memory_gib
        {
            self.memory.push(MemorySample {
                elapsed_seconds: self.started.elapsed().as_secs_f64(),
                process_gib: process_memory_gib,
            });
        }
    }

    pub fn write_bundle(
        &self,
        config: &EngineConfig,
        frame_metrics: &serde_json::Value,
        streaming: Option<&StreamingMetrics>,
        renderer: &RendererMetrics,
        system: Option<SystemMetadata>,
    ) -> std::io::Result<()> {
        let Some(root) = &config.profile_output_dir else {
            return Ok(());
        };
        fs::create_dir_all(root)?;
        let cpu: BTreeMap<_, _> = self
            .cpu_spans_ms
            .iter()
            .map(|(name, samples)| (name.clone(), summarize(samples)))
            .collect();
        let render: BTreeMap<_, _> = self
            .render_metrics
            .iter()
            .map(|(name, samples)| (name.clone(), summarize(samples)))
            .collect();
        let gpu_supported = render.keys().any(|path| path.ends_with("/elapsed_gpu"));
        let pipeline_statistics_supported = render.keys().any(|path| {
            path.ends_with("/vertex_shader_invocations")
                || path.ends_with("/compute_shader_invocations")
        });
        write_json(
            &root.join("metadata.json"),
            &Metadata {
                format_version: 1,
                engine_version: env!("CARGO_PKG_VERSION"),
                bevy_version: "0.19",
                generated_unix_ms: unix_ms(),
                scenario: config.profile_scenario.clone(),
                run_id: config.profile_run_id.clone(),
                commit: config.profile_commit.clone(),
                dirty_worktree: config.profile_dirty_worktree,
                hardware_label: config.profile_hardware.clone(),
                build_profile: if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                resolution: [1600, 900],
                worldspace_id: config.worldspace_id,
                start_grid: config.start_grid,
                stream_radius: config.stream_radius,
                synthetic_instances: config.synthetic_instances,
                system,
            },
        )?;
        write_json(&root.join("frame-metrics.json"), frame_metrics)?;
        write_json(
            &root.join("cpu-spans.json"),
            &CpuProfile {
                spans: cpu.clone(),
                counters: self.counters.clone(),
                gauges: self.gauges.clone(),
            },
        )?;
        write_json(
            &root.join("gpu-passes.json"),
            &GpuProfile {
                timestamp_queries_supported: gpu_supported,
                pipeline_statistics_supported,
                metrics: render.clone(),
                unavailable: unavailable_render_metrics(&render),
            },
        )?;
        write_json(
            &root.join("streaming.json"),
            &StreamingProfile {
                aggregate: streaming.cloned(),
                timeline: self.timeline.clone(),
            },
        )?;
        write_json(&root.join("renderer.json"), renderer)?;
        write_json(
            &root.join("memory.json"),
            &MemoryProfile {
                samples: self.memory.clone(),
                slope_gib_per_minute: memory_slope(&self.memory),
            },
        )?;
        fs::write(
            root.join("summary.md"),
            summary_markdown(config, frame_metrics, &cpu, &render, streaming, renderer),
        )?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_scene_inventory(
    mut profiler: ResMut<ProfilingState>,
    mut frames: Local<u64>,
    meshes: Query<(), With<Mesh3d>>,
    terrain: Query<&ViewVisibility, With<crate::world::components::TerrainPatch>>,
    water: Query<(), With<crate::world::components::WaterSurface>>,
    bounds: Query<(), With<crate::world::components::InstanceBounds>>,
    mesh_assets: Res<Assets<Mesh>>,
    images: Res<Assets<Image>>,
) {
    *frames = (*frames).saturating_add(1);
    if !(*frames).is_multiple_of(60) {
        return;
    }
    let started = Instant::now();
    profiler.set_gauge("scene/mesh_entities", meshes.iter().count() as f64);
    profiler.set_gauge("scene/terrain_patches", terrain.iter().count() as f64);
    profiler.set_gauge(
        "scene/visible_terrain_patches",
        terrain.iter().filter(|visible| visible.get()).count() as f64,
    );
    profiler.set_gauge("scene/water_surfaces", water.iter().count() as f64);
    profiler.set_gauge("scene/bounded_instances", bounds.iter().count() as f64);
    profiler.set_gauge("assets/meshes", mesh_assets.len() as f64);
    profiler.set_gauge("assets/images", images.len() as f64);
    profiler.record_elapsed("profiling/scene_inventory", started);
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetadata {
    pub os: String,
    pub kernel: String,
    pub cpu: String,
    pub core_count: String,
    pub memory: String,
}

#[derive(Debug, Clone, Serialize)]
struct Metadata<'a> {
    format_version: u32,
    engine_version: &'a str,
    bevy_version: &'a str,
    generated_unix_ms: u128,
    scenario: String,
    run_id: String,
    commit: String,
    dirty_worktree: bool,
    hardware_label: String,
    build_profile: &'a str,
    resolution: [u32; 2],
    worldspace_id: u32,
    start_grid: (i32, i32),
    stream_radius: i32,
    synthetic_instances: usize,
    system: Option<SystemMetadata>,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineEvent {
    elapsed_ms: f64,
    frame: u64,
    subject: String,
    stage: String,
    value_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct MemorySample {
    elapsed_seconds: f64,
    process_gib: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricSummary {
    pub count: usize,
    pub total: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub worst: f64,
}

#[derive(Serialize)]
struct CpuProfile {
    spans: BTreeMap<String, MetricSummary>,
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct GpuProfile {
    timestamp_queries_supported: bool,
    pipeline_statistics_supported: bool,
    metrics: BTreeMap<String, MetricSummary>,
    unavailable: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct StreamingProfile {
    aggregate: Option<StreamingMetrics>,
    timeline: Vec<TimelineEvent>,
}

#[derive(Serialize)]
struct MemoryProfile {
    samples: Vec<MemorySample>,
    slope_gib_per_minute: Option<f64>,
}

fn summarize(samples: &[f64]) -> MetricSummary {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let total = sorted.iter().sum::<f64>();
    MetricSummary {
        count: sorted.len(),
        total,
        mean: total / sorted.len().max(1) as f64,
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        p99: percentile(&sorted, 0.99),
        worst: sorted.last().copied().unwrap_or_default(),
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn memory_slope(samples: &[MemorySample]) -> Option<f64> {
    let first = samples.first()?;
    let last = samples.last()?;
    let minutes = (last.elapsed_seconds - first.elapsed_seconds) / 60.0;
    (minutes > 0.0).then_some((last.process_gib - first.process_gib) / minutes)
}

fn unavailable_render_metrics(
    render: &BTreeMap<String, MetricSummary>,
) -> BTreeMap<String, String> {
    [
        ("draw_calls", "Bevy's built-in render diagnostics do not expose draw-call count for every render phase"),
        ("indirect_draw_count", "the native GPU preprocessing path does not currently publish this counter"),
        ("occlusion_rejected_instances", "OcclusionCulling does not export the rejected instance count to the main world"),
    ]
    .into_iter()
    .filter(|(name, _)| !render.keys().any(|path| path.ends_with(name)))
    .map(|(name, reason)| (name.to_owned(), reason.to_owned()))
    .collect()
}

fn summary_markdown(
    config: &EngineConfig,
    frame: &serde_json::Value,
    cpu: &BTreeMap<String, MetricSummary>,
    render: &BTreeMap<String, MetricSummary>,
    streaming: Option<&StreamingMetrics>,
    renderer: &RendererMetrics,
) -> String {
    let mut top_cpu: Vec<_> = cpu.iter().collect();
    top_cpu.sort_by(|left, right| right.1.total.total_cmp(&left.1.total));
    let mut top_gpu: Vec<_> = render
        .iter()
        .filter(|(name, _)| name.ends_with("/elapsed_gpu"))
        .collect();
    top_gpu.sort_by(|left, right| right.1.mean.total_cmp(&left.1.mean));
    let mut output = format!(
        "# Profiling summary — {}\n\n- Average FPS: {:.2}\n- Frame P95: {:.2} ms\n- Passed: {}\n- GPU preprocessing: {}\n- GPU culling: {}\n- Indirect drawing: {}\n- HZB views: {}\n",
        config.profile_scenario,
        frame["average_fps"].as_f64().unwrap_or_default(),
        frame["frame_ms_p95"].as_f64().unwrap_or_default(),
        frame["passed"].as_bool().unwrap_or(false),
        renderer.gpu_preprocessing_active,
        renderer.gpu_culling_active,
        renderer.indirect_drawing_active,
        renderer.hzb_views,
    );
    output.push_str(
        "\n## Top CPU spans\n\n| Span | Mean ms | P95 ms | Total ms |\n|---|---:|---:|---:|\n",
    );
    for (name, value) in top_cpu.into_iter().take(10) {
        output.push_str(&format!(
            "| {name} | {:.3} | {:.3} | {:.3} |\n",
            value.mean, value.p95, value.total
        ));
    }
    output.push_str("\n## Top GPU passes\n\n| Pass | Mean ms | P95 ms |\n|---|---:|---:|\n");
    for (name, value) in top_gpu.into_iter().take(10) {
        output.push_str(&format!(
            "| {name} | {:.3} | {:.3} |\n",
            value.mean, value.p95
        ));
    }
    if let Some(streaming) = streaming {
        output.push_str(&format!(
            "\n## Streaming and assets\n\n- Requests: {}\n- Active requests: {} (peak {})\n- Resident roots: {}\n- Failed cells: {}\n- Stale responses: {}\n- Unloaded cells: {}\n- Origin rebases: {}\n- Lifecycle invariant failures: {}\n- Duplicate/orphaned/missing/out-of-range roots: {}/{}/{}/{}\n- Streaming fixture validated: {}\n- Assets ready: {}\n- Empty model references: {}\n- Model assets pending: {}\n- Surface assets pending: {}\n- Meshes validated: {}\n- Materials validated: {}\n- Images validated: {}\n- Asset failures: {}\n- Material validation failures: {}\n- Diagnostic fallbacks: {}\n- Canonical fixture validated: {}\n- Terrain patches validated: {}\n- Terrain seams validated: {}\n- Terrain seam points welded: {}\n- Terrain edges left as authored: {}\n- Terrain failures: {}\n- Water surfaces validated: {}\n- Water failures: {}\n- Terrain/water fixture validated: {}\n- Max query: {:.3} ms\n- Max cell commit: {:.3} ms\n- Max frame commit: {:.3} ms (budget {:.3} ms, violations {})\n",
            streaming.requests_submitted,
            streaming.active_requests,
            streaming.peak_active_requests,
            streaming.resident_roots,
            streaming.failed_cells,
            streaming.stale_responses,
            streaming.unloaded_cells,
            streaming.origin_rebases,
            streaming.streaming_invariant_failures,
            streaming.duplicate_cell_roots,
            streaming.orphaned_cell_roots,
            streaming.missing_cell_roots,
            streaming.out_of_range_cell_roots,
            streaming.streaming_fixture_validated,
            streaming.assets_ready,
            streaming.empty_model_references,
            streaming.pending_asset_instances,
            streaming.pending_surface_instances,
            streaming.meshes_validated,
            streaming.materials_validated,
            streaming.images_validated,
            streaming.asset_load_failures,
            streaming.material_validation_failures,
            streaming.diagnostic_fallbacks,
            streaming.canonical_fixture_validated,
            streaming.terrain_patches_validated,
            streaming.terrain_seams_validated,
            streaming.terrain_seam_points_welded,
            streaming.terrain_edges_left_as_authored,
            streaming.terrain_validation_failures,
            streaming.water_surfaces_validated,
            streaming.water_validation_failures,
            streaming.terrain_water_fixture_validated,
            streaming.max_query_micros as f64 / 1000.0,
            streaming.max_commit_micros as f64 / 1000.0,
            streaming.max_frame_commit_micros as f64 / 1000.0,
            streaming.commit_budget_micros as f64 / 1000.0,
            streaming.commit_budget_violations,
        ));
    }
    output
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?,
    )
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn push_bounded(values: &mut Vec<f64>, value: f64) {
    if values.len() < MAX_SAMPLES_PER_METRIC && value.is_finite() {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_percentiles_and_worst_value() {
        let values: Vec<_> = (1..=100).map(f64::from).collect();
        let summary = summarize(&values);
        assert_eq!(summary.count, 100);
        assert_eq!(summary.p95, 96.0);
        assert_eq!(summary.worst, 100.0);
    }

    #[test]
    fn computes_memory_growth_slope() {
        let samples = vec![
            MemorySample {
                elapsed_seconds: 0.0,
                process_gib: 1.0,
            },
            MemorySample {
                elapsed_seconds: 120.0,
                process_gib: 1.2,
            },
        ];
        assert!((memory_slope(&samples).unwrap() - 0.1).abs() < 0.0001);
    }

    #[test]
    fn writes_complete_profile_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            profile_output_dir: Some(directory.path().to_owned()),
            profile_scenario: "synthetic".into(),
            ..Default::default()
        };
        let mut profiler = ProfilingState::default();
        profiler.record_ms("streaming/test", 2.0);
        profiler.increment("test/count", 1);
        profiler.event("cell", "requested", None);
        let frame = serde_json::json!({
            "average_fps": 90.0,
            "frame_ms_p95": 14.0,
            "passed": true
        });
        profiler
            .write_bundle(
                &config,
                &frame,
                Some(&StreamingMetrics::default()),
                &RendererMetrics::default(),
                None,
            )
            .unwrap();
        for name in [
            "metadata.json",
            "frame-metrics.json",
            "cpu-spans.json",
            "gpu-passes.json",
            "streaming.json",
            "renderer.json",
            "memory.json",
            "summary.md",
        ] {
            assert!(directory.path().join(name).is_file(), "missing {name}");
        }
        let summary = fs::read_to_string(directory.path().join("summary.md")).unwrap();
        assert!(
            summary.contains("Terrain seam points welded: 0"),
            "the summary must report the welded terrain seam points"
        );
    }
}
