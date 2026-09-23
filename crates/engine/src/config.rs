use bevy::prelude::Resource;
use std::path::PathBuf;

#[derive(Debug, Clone, Resource)]
pub struct EngineConfig {
    pub assets_dir: PathBuf,
    pub worldspace_id: u32,
    pub start_grid: (i32, i32),
    pub stream_radius: i32,
    pub unload_radius: i32,
    pub max_cell_commits_per_frame: usize,
    pub max_commit_micros_per_frame: u64,
    pub headless: bool,
    pub benchmark_only: bool,
    pub benchmark_frames: Option<u32>,
    pub benchmark_duration_secs: Option<f64>,
    pub benchmark_warmup_frames: u32,
    pub benchmark_output: PathBuf,
    pub accept_min_fps: f64,
    pub accept_p95_ms: f64,
    pub accept_max_memory_growth_gib: f64,
    pub auto_fly_speed: f32,
    pub allow_incomplete_assets: bool,
    pub synthetic_instances: usize,
    pub profile_output_dir: Option<PathBuf>,
    pub profile_scenario: String,
    pub profile_run_id: String,
    pub profile_commit: String,
    pub profile_dirty_worktree: bool,
    pub profile_hardware: String,
    pub acceptance_screenshot: Option<PathBuf>,
    pub screenshot_camera_offset: Option<(f32, f32, f32)>,
    pub diagnostic_asset_fallbacks: bool,
    pub material_fixture: bool,
    pub terrain_water_fixture: bool,
    pub transform_bounds_fixture: bool,
    pub renderer_fixture: bool,
    pub streaming_fixture: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            assets_dir: PathBuf::from("modern_assets"),
            worldspace_id: 0x3c,
            start_grid: (0, 0),
            stream_radius: 2,
            unload_radius: 3,
            max_cell_commits_per_frame: 1,
            max_commit_micros_per_frame: 16_670,
            headless: false,
            benchmark_only: false,
            benchmark_frames: None,
            benchmark_duration_secs: None,
            benchmark_warmup_frames: 60,
            benchmark_output: PathBuf::from("benchmark-report.json"),
            accept_min_fps: 60.0,
            accept_p95_ms: 16.67,
            accept_max_memory_growth_gib: 0.5,
            auto_fly_speed: 0.0,
            allow_incomplete_assets: false,
            synthetic_instances: 250_000,
            profile_output_dir: None,
            profile_scenario: "adhoc".into(),
            profile_run_id: "run-1".into(),
            profile_commit: "unknown".into(),
            profile_dirty_worktree: false,
            profile_hardware: "unspecified".into(),
            acceptance_screenshot: None,
            screenshot_camera_offset: None,
            diagnostic_asset_fallbacks: false,
            material_fixture: false,
            terrain_water_fixture: false,
            transform_bounds_fixture: false,
            renderer_fixture: false,
            streaming_fixture: false,
        }
    }
}

impl EngineConfig {
    pub fn from_env() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut config = Self::default();
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--assets" => {
                    if let Some(value) = args.next() {
                        config.assets_dir = value.into();
                    }
                }
                "--worldspace" => {
                    if let Some(value) = args.next().and_then(|value| parse_u32(&value)) {
                        config.worldspace_id = value;
                    }
                }
                "--grid-x" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.start_grid.0 = value;
                    }
                }
                "--grid-y" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.start_grid.1 = value;
                    }
                }
                "--stream-radius" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.stream_radius = value;
                        config.unload_radius = value + 1;
                    }
                }
                "--headless" => config.headless = true,
                "--max-commit-ms" => {
                    if let Some(value) = args.next().and_then(|value| value.parse::<f64>().ok())
                        && value.is_finite()
                        && value > 0.0
                    {
                        config.max_commit_micros_per_frame =
                            (value * 1_000.0).round().clamp(1.0, u64::MAX as f64) as u64;
                    }
                }
                "--benchmark-only" => config.benchmark_only = true,
                "--benchmark-frames" => {
                    config.benchmark_frames = args.next().and_then(|value| value.parse().ok());
                }
                "--benchmark-duration" => {
                    config.benchmark_duration_secs =
                        args.next().and_then(|value| value.parse().ok());
                }
                "--benchmark-warmup-frames" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.benchmark_warmup_frames = value;
                    }
                }
                "--benchmark-output" => {
                    if let Some(value) = args.next() {
                        config.benchmark_output = value.into();
                    }
                }
                "--accept-min-fps" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_min_fps = value;
                    }
                }
                "--accept-p95-ms" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_p95_ms = value;
                    }
                }
                "--accept-max-memory-growth-gib" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_max_memory_growth_gib = value;
                    }
                }
                "--auto-fly-speed" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.auto_fly_speed = value;
                    }
                }
                "--allow-incomplete-assets" => config.allow_incomplete_assets = true,
                "--synthetic-instances" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.synthetic_instances = value;
                    }
                }
                "--profile-output" => {
                    config.profile_output_dir = args.next().map(PathBuf::from);
                }
                "--profile-scenario" => {
                    if let Some(value) = args.next() {
                        config.profile_scenario = value;
                    }
                }
                "--profile-run-id" => {
                    if let Some(value) = args.next() {
                        config.profile_run_id = value;
                    }
                }
                "--profile-commit" => {
                    if let Some(value) = args.next() {
                        config.profile_commit = value;
                    }
                }
                "--profile-dirty-worktree" => config.profile_dirty_worktree = true,
                "--profile-hardware" => {
                    if let Some(value) = args.next() {
                        config.profile_hardware = value;
                    }
                }
                "--acceptance-screenshot" => {
                    config.acceptance_screenshot = args.next().map(PathBuf::from);
                }
                "--screenshot-camera-offset" => {
                    if let Some(value) = args.next().and_then(|value| parse_offset(&value)) {
                        config.screenshot_camera_offset = Some(value);
                    }
                }
                "--diagnostic-asset-fallbacks" => config.diagnostic_asset_fallbacks = true,
                "--material-fixture" => config.material_fixture = true,
                "--terrain-water-fixture" => config.terrain_water_fixture = true,
                "--transform-bounds-fixture" => config.transform_bounds_fixture = true,
                "--renderer-fixture" => config.renderer_fixture = true,
                "--streaming-fixture" => config.streaming_fixture = true,
                _ => {}
            }
        }
        config
    }
}

fn parse_offset(value: &str) -> Option<(f32, f32, f32)> {
    let mut parts = value.split(',');
    let x: f32 = parts.next()?.trim().parse().ok()?;
    let y: f32 = parts.next()?.trim().parse().ok()?;
    let z: f32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((x, y, z))
}

fn parse_u32(value: &str) -> Option<u32> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse().ok(),
            |hex| u32::from_str_radix(hex, 16).ok(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_one_cell_commit_within_a_sixty_fps_frame() {
        let config = EngineConfig::default();
        assert_eq!(config.max_cell_commits_per_frame, 1);
        assert_eq!(config.max_commit_micros_per_frame, 16_670);
    }

    #[test]
    fn parses_screenshot_camera_offset() {
        let config = EngineConfig::from_args(
            ["--screenshot-camera-offset", "0,6000,12000"].map(str::to_owned),
        );
        assert_eq!(
            config.screenshot_camera_offset,
            Some((0.0, 6000.0, 12000.0))
        );
        let config =
            EngineConfig::from_args(["--screenshot-camera-offset", "0,6000"].map(str::to_owned));
        assert_eq!(config.screenshot_camera_offset, None);
        assert_eq!(EngineConfig::default().screenshot_camera_offset, None);
    }

    #[test]
    fn parses_runtime_options() {
        let config = EngineConfig::from_args(
            [
                "--assets",
                "converted",
                "--worldspace",
                "0x3c",
                "--grid-x",
                "4",
                "--stream-radius",
                "5",
                "--headless",
                "--max-commit-ms",
                "8.5",
                "--profile-output",
                "profiles/run-1",
                "--profile-scenario",
                "stress",
                "--profile-run-id",
                "run-3",
                "--profile-commit",
                "abc123",
                "--profile-dirty-worktree",
                "--profile-hardware",
                "test-machine",
                "--acceptance-screenshot",
                "evidence/rural.png",
                "--diagnostic-asset-fallbacks",
                "--material-fixture",
                "--terrain-water-fixture",
                "--transform-bounds-fixture",
                "--renderer-fixture",
                "--streaming-fixture",
            ]
            .map(str::to_owned),
        );
        assert_eq!(config.assets_dir, PathBuf::from("converted"));
        assert_eq!(config.worldspace_id, 0x3c);
        assert_eq!(config.start_grid, (4, 0));
        assert_eq!((config.stream_radius, config.unload_radius), (5, 6));
        assert!(config.headless);
        assert_eq!(config.max_commit_micros_per_frame, 8_500);
        assert_eq!(
            config.profile_output_dir,
            Some(PathBuf::from("profiles/run-1"))
        );
        assert_eq!(config.profile_scenario, "stress");
        assert_eq!(config.profile_run_id, "run-3");
        assert_eq!(config.profile_commit, "abc123");
        assert!(config.profile_dirty_worktree);
        assert_eq!(config.profile_hardware, "test-machine");
        assert_eq!(
            config.acceptance_screenshot,
            Some(PathBuf::from("evidence/rural.png"))
        );
        assert!(config.diagnostic_asset_fallbacks);
        assert!(config.material_fixture);
        assert!(config.terrain_water_fixture);
        assert!(config.transform_bounds_fixture);
        assert!(config.renderer_fixture);
        assert!(config.streaming_fixture);
    }
}
