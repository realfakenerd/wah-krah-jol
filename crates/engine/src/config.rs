use crate::{
    color_pipeline::{SCENE_GRADE_VALUES, SceneGradeMode, SceneVisualPreset},
    skyrim_ini::SkyrimIni,
    tone_mapping::SceneToneMapMode,
    visual_profile::{VISUAL_PROFILE_VALUES, VisualProfile},
    world::components::CELL_SIZE,
    world::imagespace::{ImageSpaceSelection, ImageSpaceSlot},
};
use bevy::prelude::{Msaa, Resource};
use shared::lod::LodTier;
use std::{fmt, path::PathBuf};

/// Most IO threads `--io-threads` accepts; `0` still asks for the automatic size. Each thread
/// reserves 128 MiB of address space (`app::IO_TASK_STACK_BYTES`), so an unbounded count could
/// exhaust it and make Bevy panic spawning threads. 64 is well past the 4 the automatic size
/// uses and the 8 that were measured.
pub(crate) const MAX_IO_THREADS: usize = 64;

/// What `--io-threads` takes, with [`MAX_IO_THREADS`] spelled out; a test holds the two equal.
const IO_THREADS_EXPECTED: &str = "a thread count from 0 to 64";

/// How far each terrain LOD tier draws, in Skyrim's `[TerrainManager]` terms:
/// a tier reaches its block distance times `fSplitDistanceMult`, in Creation
/// units from the camera. `--ini` reads these from a `SkyrimPrefs.ini`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainLodDistances {
    /// `fBlockLevel0Distance`: level-4 tier.
    pub block_level0_distance: f32,
    /// `fBlockLevel1Distance`: level-8 tier.
    pub block_level1_distance: f32,
    /// `fBlockMaximumDistance`: level-16 tier.
    pub block_maximum_distance: f32,
    /// `fSplitDistanceMult`: terrain multiplier on the three block distances.
    pub split_distance_mult: f32,
}

impl Default for TerrainLodDistances {
    /// Skyrim Special Edition's `SkyrimPrefs.ini` defaults, so an install without
    /// an INI draws as far as the game does: about 12, 25 and 91 cells for
    /// tiers 4, 8 and 16.
    fn default() -> Self {
        Self {
            block_level0_distance: 35_000.0,
            block_level1_distance: 70_000.0,
            block_maximum_distance: 250_000.0,
            split_distance_mult: 1.5,
        }
    }
}

impl TerrainLodDistances {
    /// The Chebyshev cell distance from the camera's cell out to which `tier` draws.
    pub fn reach_cells(&self, tier: LodTier) -> i32 {
        let block = match tier {
            LodTier::Tier4 => self.block_level0_distance,
            LodTier::Tier8 => self.block_level1_distance,
            LodTier::Tier16 => self.block_maximum_distance,
        };
        // `as` saturates, so an enormous configured distance means "everything".
        (f64::from(block) * f64::from(self.split_distance_mult) / f64::from(CELL_SIZE)).floor()
            as i32
    }

    /// The farthest reach of any tier.
    pub fn max_reach_cells(&self) -> i32 {
        LodTier::ALL
            .map(|tier| self.reach_cells(tier))
            .into_iter()
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Resource)]
pub struct EngineConfig {
    /// Explicit launch options. Saved interactive settings defer to these and to `--ini`.
    pub settings_overrides: Vec<String>,
    pub assets_dir: PathBuf,
    /// Explicit local report destination. `None` uses the per-user application-data directory.
    pub bug_report_dir: Option<PathBuf>,
    pub worldspace_id: u32,
    pub start_grid: (i32, i32),
    pub stream_radius: i32,
    pub unload_radius: i32,
    /// Optional upper bound for sun-shadow cascades from `[Display] fShadowDistance`.
    pub sun_shadow_distance_limit: Option<f32>,
    /// Optional sun bearing clockwise from north and elevation above the horizon, in degrees.
    pub sun_position: Option<(f32, f32)>,
    pub terrain_lod: TerrainLodDistances,
    pub max_cell_commits_per_frame: usize,
    pub max_commit_micros_per_frame: u64,
    /// Cells outside the unload radius a frame may despawn. `0` despawns every one at once, which
    /// is the unbudgeted behaviour unloading used to have.
    pub max_cell_unloads_per_frame: usize,
    /// Converted models a frame may hand to Bevy's scene spawner. `0` arms every model whose asset
    /// is loaded, which is the unbudgeted behaviour a single spawn batch used to have.
    pub max_model_spawns_per_frame: usize,
    /// Camera and bounds based queue ordering. Off preserves fixed FIFO activation.
    pub prioritize_streaming: bool,
    /// Outstanding unique Scene(0) jobs, including recursive CPU dependencies.
    /// Zero retains unlimited admission; this count does not bound memory bytes.
    pub max_scene_loads: usize,
    /// Opt-in startup, walking and recovery budgets. Also enables spatial priority.
    pub adaptive_streaming: bool,
    /// Downstream queue high watermark; zero disables it outside adaptive mode.
    pub max_streaming_backlog: usize,
    /// Estimated streaming allocation allowance. Zero disables memory admission
    /// outside adaptive mode, which otherwise uses a finite automatic allowance.
    pub streaming_memory_mib: usize,
    /// Override for converter-generated streaming cost metadata.
    pub streaming_costs: Option<PathBuf>,
    /// Free system memory reserved for the rest of the process and other apps.
    pub streaming_headroom_mib: usize,
    /// Controller frame target. Benchmark acceptance keeps its separate thresholds.
    pub streaming_frame_ms: f64,
    /// Threads in the asset IO pool. `0` sizes it to the machine: a quarter of the hardware
    /// threads, at least one and at most four. A nonzero count must be at most
    /// `MAX_IO_THREADS`; a larger one is refused like a malformed number.
    pub io_threads: usize,
    /// MiB of newly loaded render assets (meshes, textures) the renderer may
    /// prepare per frame. `0` prepares every asset the frame extracted.
    pub max_upload_mib_per_frame: usize,
    pub headless: bool,
    /// Keep a renderable window hidden for automated screenshot capture.
    pub hidden_window: bool,
    /// Whole color pipeline; fixed for the run because stages never mix across profiles.
    pub visual_profile: VisualProfile,
    /// Select the cinematic A/B experiment without changing scene lighting.
    pub color_grade: SceneGradeMode,
    /// Named sky, fog and grading look; Vivid preserves the approved configuration.
    pub visual_preset: SceneVisualPreset,
    /// Optional approved-look JSON override; authored records are retained separately.
    pub grade_preset: Option<PathBuf>,
    /// Display transform selection; native-curve experiments require explicit parameters.
    pub tone_map: SceneToneMapMode,
    pub tone_map_params: Option<PathBuf>,
    pub image_space: Option<u32>,
    pub image_space_weather: Option<u32>,
    pub image_space_slot: Option<ImageSpaceSlot>,
    /// Seed a live world camera from a validated one-pose shots file.
    pub start_shot: Option<PathBuf>,
    /// Accept developer console commands on this process's stdin.
    pub stdin_console: bool,
    /// Disable Bevy's GPU timing and pipeline-statistics query collection.
    pub no_render_diagnostics: bool,
    pub benchmark_only: bool,
    pub benchmark_frames: Option<u32>,
    pub benchmark_duration_secs: Option<f64>,
    pub benchmark_warmup_frames: u32,
    /// Keep the interactive app's presentation mode for an opt-in benchmark.
    pub benchmark_vsync: bool,
    /// Dedicated post-settlement camera route; Some(0) measures its stationary tail.
    pub streaming_benchmark_route_speed: Option<f64>,
    pub streaming_benchmark_route_secs: f64,
    pub streaming_benchmark_tail_secs: f64,
    pub streaming_benchmark_settle_secs: f64,
    pub streaming_benchmark_expected_models: Option<u64>,
    pub streaming_benchmark_expected_lod_chunks: Option<usize>,
    pub streaming_benchmark_expected_terrain: Option<u64>,
    pub streaming_benchmark_expected_water: Option<u64>,
    /// `--benchmark-jump <grid-x>,<grid-y>`: once the world around the camera is first fully
    /// loaded, move the camera to the centre of that cell of the same worldspace and measure how
    /// long the world takes to become fully loaded again (`time_to_world_ready_after_jump_ms`).
    /// Coordinates must be within [`JUMP_GRID_LIMIT`]; anything further out is ignored with a
    /// warning, since the position maths would not survive it.
    pub benchmark_jump: Option<(i32, i32)>,
    pub benchmark_output: PathBuf,
    /// Where to write every measured frame time, in order, as CSV (`--benchmark-frame-times`).
    /// Off by default: the report's summary is what acceptance reads; the series is for choosing
    /// run lengths and spotting drift within a run.
    pub benchmark_frame_times: Option<PathBuf>,
    /// `--run-label <text>`: names an automated run in its window title, e.g. a benchmark's
    /// variant and round ([`EngineConfig::window_title`]).
    pub run_label: Option<String>,
    pub accept_min_fps: f64,
    pub accept_p95_ms: f64,
    pub accept_max_memory_growth_gib: f64,
    pub auto_fly_speed: f32,
    /// `--route <file>`: the camera follows a route file's waypoints at its speed (see
    /// [`crate::route`]). It owns the camera, like `--auto-fly-speed`.
    pub route: Option<PathBuf>,
    /// `--frame-budget-ms <ms>`: the frame time the report counts frames over (see
    /// [`crate::frame_budget`]). The default is 6.25, 160 frames per second.
    pub frame_budget_ms: f64,
    pub allow_incomplete_assets: bool,
    pub synthetic_instances: usize,
    pub profile_output_dir: Option<PathBuf>,
    pub profile_gpu_inventory: bool,
    pub profile_scenario: String,
    pub profile_run_id: String,
    pub profile_commit: String,
    pub profile_dirty_worktree: bool,
    pub profile_hardware: String,
    pub acceptance_screenshot: Option<PathBuf>,
    pub screenshot_camera_offset: Option<(f32, f32, f32)>,
    /// `--shots <file>`: render the camera poses in a shots file, one PNG each, and exit instead of
    /// running interactively. See [`crate::shots`].
    pub shots: Option<PathBuf>,
    /// Exact, fixed-step profiling traversal. Mutually exclusive with other camera drivers.
    pub matched_route: Option<PathBuf>,
    /// `--shots-out <dir>`: where the PNGs and `shots.log` go. `None` is
    /// [`crate::shots::default_output_dir`], a `<file stem>-shots/` folder beside the shots file.
    pub shots_out: Option<PathBuf>,
    /// Maximum settling time per shot, in seconds.
    pub shots_settle_timeout: f32,
    pub diagnostic_asset_fallbacks: bool,
    pub material_fixture: bool,
    pub terrain_water_fixture: bool,
    /// The sample count the terrain/water fixture camera draws with
    /// (`--terrain-water-fixture-msaa`). Off by default, so the fixture renders as it always has.
    pub terrain_water_fixture_msaa: Msaa,
    pub transform_bounds_fixture: bool,
    pub renderer_fixture: bool,
    pub streaming_fixture: bool,
    /// Whether a streamed `LIGH` reference places a point light (`--lights`). Off by default, so
    /// every run that does not ask for lights renders exactly as it did before.
    pub lights: bool,
    pub physics_fixture: bool,
}

impl Default for EngineConfig {
    /// Start with the standard engine settings and optional profiling diagnostics disabled.
    fn default() -> Self {
        Self {
            settings_overrides: Vec::new(),
            assets_dir: PathBuf::from("modern_assets"),
            bug_report_dir: None,
            worldspace_id: 0x3c,
            start_grid: (0, 0),
            // Skyrim's default `uGridsToLoad=5`.
            stream_radius: 2,
            unload_radius: 3,
            sun_shadow_distance_limit: None,
            sun_position: None,
            terrain_lod: TerrainLodDistances::default(),
            max_cell_commits_per_frame: 1,
            max_commit_micros_per_frame: 16_670,
            max_cell_unloads_per_frame: 2,
            // After a jump, the models of several cells are already loaded and reach the scene
            // spawner together: on the stress scenario the backlog runs to about 2,000 models. At 4
            // a frame that backlog took ~500 frames to clear; at 32 the world is ready 2.5x sooner
            // with the same worst frame, because a frame hands over more of the queue without
            // instantiating so many models at once that the frame blows past 60 fps. The limit
            // still spreads a burst over frames; `0` arms the whole batch at once, the behaviour
            // before this budget existed (docs/roadmap/02-profiling.md, "Load speed defaults").
            max_model_spawns_per_frame: 32,
            prioritize_streaming: false,
            max_scene_loads: 0,
            adaptive_streaming: false,
            max_streaming_backlog: 0,
            streaming_memory_mib: 0,
            streaming_costs: None,
            streaming_headroom_mib: 2_048,
            streaming_frame_ms: 16.67,
            // A quarter of the hardware threads, chosen at startup (see `app::io_pool_threads`).
            io_threads: 0,
            // Three 2K BC7/UASTC textures with a full mip chain (~5.3 MiB each):
            // a cell's new textures spread over a few frames instead of landing
            // in one 13 ms upload burst, and at 60 fps the budget still admits
            // far more new assets than the streaming radius can produce.
            max_upload_mib_per_frame: 16,
            headless: false,
            hidden_window: false,
            visual_profile: VisualProfile::Mudcrab,
            color_grade: SceneGradeMode::SnapshotSrgb,
            visual_preset: SceneVisualPreset::CoolGray,
            grade_preset: None,
            tone_map: SceneToneMapMode::Tony,
            tone_map_params: None,
            image_space: None,
            image_space_weather: None,
            image_space_slot: None,
            start_shot: None,
            stdin_console: false,
            no_render_diagnostics: false,
            benchmark_only: false,
            benchmark_frames: None,
            benchmark_duration_secs: None,
            benchmark_warmup_frames: 60,
            benchmark_vsync: false,
            streaming_benchmark_route_speed: None,
            streaming_benchmark_route_secs: 45.0,
            streaming_benchmark_tail_secs: 10.0,
            streaming_benchmark_settle_secs: 5.0,
            streaming_benchmark_expected_models: None,
            streaming_benchmark_expected_lod_chunks: None,
            streaming_benchmark_expected_terrain: None,
            streaming_benchmark_expected_water: None,
            benchmark_jump: None,
            benchmark_output: PathBuf::from("benchmark-report.json"),
            benchmark_frame_times: None,
            run_label: None,
            accept_min_fps: 60.0,
            accept_p95_ms: 16.67,
            accept_max_memory_growth_gib: 0.5,
            auto_fly_speed: 0.0,
            route: None,
            frame_budget_ms: crate::frame_budget::DEFAULT_FRAME_BUDGET_MS,
            allow_incomplete_assets: false,
            synthetic_instances: 250_000,
            profile_output_dir: None,
            profile_gpu_inventory: false,
            profile_scenario: "adhoc".into(),
            profile_run_id: "run-1".into(),
            profile_commit: "unknown".into(),
            profile_dirty_worktree: false,
            profile_hardware: "unspecified".into(),
            acceptance_screenshot: None,
            screenshot_camera_offset: None,
            shots: None,
            matched_route: None,
            shots_out: None,
            shots_settle_timeout: crate::shots::SETTLE_TIMEOUT_SECONDS,
            diagnostic_asset_fallbacks: false,
            material_fixture: false,
            terrain_water_fixture: false,
            terrain_water_fixture_msaa: Msaa::Off,
            transform_bounds_fixture: false,
            renderer_fixture: false,
            streaming_fixture: false,
            lights: false,
            physics_fixture: false,
        }
    }
}

/// The text `--help` prints, and the list of options the engine accepts.
///
/// This and the `match` in [`EngineConfig::from_args`] are the two places an
/// option is named; the tests read the match arms back out of this file and
/// check the two agree, so neither can drift from the other.
pub const HELP_TEXT: &str = "\
Mudcrab engine

Usage: engine [options]

Streams converted cells from a world database. With no options it reads
\"modern_assets\" and starts in worldspace 0x3c at cell (0, 0).

Assets and start position:
  --assets <dir>                        converted asset directory to stream (default: modern_assets)
  --worldspace <id>                     worldspace form id, decimal or 0x-hex (default: 0x3c)
  --grid-x <cell>                       starting cell x (default: 0)
  --grid-y <cell>                       starting cell y (default: 0)
  --start-shot <file>                  start the live camera at one exterior shot pose
  --stdin-console                     accept developer console commands on stdin
  --no-render-diagnostics             disable GPU timing and pipeline-statistics queries
  --allow-incomplete-assets             run despite a failed manifest or integration-report gate
  --headless                            run without opening a window
  --hidden-window                       render screenshots without showing a window
  --lights                              place a point light for each streamed LIGH reference

Streaming:
  --ini <file>                        layer Skyrim INI settings; CLI options override files
  --stream-radius <cells>               cells streamed around the camera (default: 2)
  --max-commit-ms <ms>                  cell commit time allowed per frame (default: 16.67)
  --max-unloads-per-frame <count>       cells despawned per frame; 0 despawns all at once (default: 2)
  --max-model-spawns-per-frame <count>  models spawned per frame; 0 spawns all at once (default: 32)
  --prioritize-streaming               prioritize nearby collision and view-facing bounds
  --max-scene-loads <count>             outstanding unique scene jobs; 0 is unlimited (default: 0)
  --adaptive-streaming                 adapt startup/walking budgets; enables spatial priority
  --max-streaming-backlog <count>      downstream queue limit; 0 disables it, adaptive uses 256
  --streaming-memory-mib <mib>         estimated allowance; 0 disables it unless adaptive
                                     adaptive: half detected RAM, at most 16 GiB (fallback: 8 GiB)
  --streaming-costs <file>             override streaming cost metadata (default: asset pack metadata)
  --streaming-headroom-mib <mib>       system memory kept free (default: 2048)
  --streaming-frame-ms <ms>            controller frame target (default: 16.67)
  --max-upload-mib-per-frame <mib>      render-asset upload budget per frame; 0 is unlimited (default: 16)
  --io-threads <count>                  asset IO threads, 0 to 64; 0 sizes the pool automatically (default: 0)
  --auto-fly-speed <units/s>            fly the camera forward at this speed; 0 holds it still
  --route <file>                        follow a route file's waypoints at its speed (a JSON file; see scripts/routes)

Benchmark and profiling:
  --benchmark-only                      run the synthetic benchmark; opens no world database
  --benchmark-frames <count>            stop after this many measured frames
  --benchmark-duration <seconds>        stop after this many measured seconds (a route run: real seconds since start)
  --benchmark-warmup-frames <count>     frames discarded before measuring (default: 60)
  --benchmark-vsync                     use the interactive app's vsync during benchmarks
  --streaming-benchmark-route-speed <units/s>  start a real-time route after CPU settlement; 0 is stationary
  --streaming-benchmark-route-secs <seconds>   moving route duration (default: 45)
  --streaming-benchmark-tail-secs <seconds>    stationary tail duration (default: 10)
  --streaming-benchmark-settle-secs <seconds>  continuous CPU-settled wait before starting (default: 5)
  --streaming-benchmark-expected-models <count>       optional initial validated model count
  --streaming-benchmark-expected-lod-chunks <count>   optional initial resident LOD chunk count
  --streaming-benchmark-expected-terrain <count>      optional initial validated terrain patch count
  --streaming-benchmark-expected-water <count>        optional initial validated water surface count
  --benchmark-jump <x,y>                after the world first loads, jump to this grid cell and time the reload
  --benchmark-output <file>             benchmark report path (default: benchmark-report.json)
  --frame-budget-ms <ms>                frame time the report counts frames over (default: 6.25, 160 fps)
  --benchmark-frame-times [<file>]      write every measured frame time to this CSV file
  --run-label [<text>]                  name the run in the window title
  --synthetic-instances <count>         instances in the synthetic benchmark (default: 250000)
  --accept-min-fps <fps>                fail the run below this average frame rate (default: 60)
  --accept-p95-ms <ms>                  fail the run above this p95 frame time (default: 16.67)
  --accept-max-memory-growth-gib <gib>  fail the run above this memory growth (default: 0.5)
  --acceptance-screenshot <file>        write a screenshot when the run ends
  --screenshot-camera-offset <x,y,z>    camera offset for the acceptance screenshot
  --matched-route <file>                execute exact route steps with transition and settled checkpoints
  --visual-profile <mudcrab|skyrim>    color pipeline (default: mudcrab); changing it requires a restart
  --color-grade <mode>                 off|saturation|full|snapshot-srgb|snapshot-linear|record-srgb (default: snapshot-srgb)
  --grade-preset <json>                approved-look JSON override (snapshot-srgb and Cool Gray sky)
  --tone-map <tony|retail-curve>       display transform (default: tony); retail curve is experimental
  --tone-map-params <json>             explicit curve/scales and fixed luminance samples; requires selected IMGS
  --image-space <FormID>               load an authored IMGS record for record-srgb experiments
  --image-space-weather <FormID>       select an authored WTHR image-space slot, not live weather
  --image-space-slot <slot>            sunrise|day|sunset|night; requires --image-space-weather
  --visual-preset <vivid|cool-gray>      select sky and grading look (default: cool-gray)
  --sun-position <azimuth,elevation>     sun bearing clockwise from north and elevation above horizon, degrees
  --shots <file>                        render the camera poses in a shots file, one PNG each, then exit
  --shots-settle-timeout <seconds>      maximum settling time per shot (default: 30, maximum: 600)
  --shots-out <dir>                     where the shots' PNGs and shots.log go (default: beside the shots file)
  --profile-output <dir>                profile bundle directory (default: no bundle)
  --profile-gpu-inventory               expensive GPU inventory diagnostic; requires --profile-output
  --profile-scenario <name>             scenario name recorded in the profile (default: adhoc)
  --profile-run-id <id>                 run id recorded in the profile (default: run-1)
  --profile-commit <hash>               commit recorded in the profile (default: unknown)
  --profile-dirty-worktree              record uncommitted worktree changes in the profile
  --profile-hardware <text>             hardware recorded in the profile (default: unspecified)

Diagnostics:
  --bug-report-dir <dir>                local bug-report directory (default: per-user application data)
  --diagnostic-asset-fallbacks          log every fallback asset substitution
  --material-fixture                    run the material fixture (no world database)
  --terrain-water-fixture               run the terrain-water fixture (no world database)
  --terrain-water-fixture-msaa <n>      terrain-water fixture MSAA samples: 1, 2, 4 or 8 (default: 1)
  --transform-bounds-fixture            run the transform-bounds fixture (no world database)
  --renderer-fixture                    run the renderer fixture (no world database)
  --physics-fixture                     run the physics fixture (no world database)
  --streaming-fixture                   stream the fixture world instead of the full asset set

Help:
  -h, --help                            print this message and exit

Exit codes:
  0  success
  1  the run failed
  2  the command line could not be used
  3  the GPU device was lost (retry the run; repeated losses can still be an engine fault)

Example (the dummy-content fixture's worldspace is 1):
  engine --assets modern_assets --worldspace 1
";

/// What a parsed command line asks the process to do.
#[derive(Debug, Clone)]
pub enum ConfigAction {
    /// Run the engine. The configuration is boxed to keep this enum small.
    Run(Box<EngineConfig>),
    /// Print [`HELP_TEXT`] and exit successfully.
    Help,
}

/// A command line the engine cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// An option the engine does not accept, with the known option it is
    /// closest to when one is within two edits.
    UnknownOption {
        argument: String,
        suggestion: Option<&'static str>,
    },
    /// A bare argument where the engine expects an option.
    UnexpectedArgument { argument: String },
    /// A value-taking option whose value is missing, is another option, or
    /// cannot be read.
    InvalidValue {
        option: &'static str,
        /// The value that was given, if the command line had one to give.
        value: Option<String>,
        /// What the option needs, phrased for the message.
        expected: &'static str,
    },
    /// An option that only works together with another option that was not given.
    MissingRequirement {
        option: &'static str,
        requires: &'static str,
    },
    /// An option that steers a pipeline the selected visual profile does not run.
    ForeignToProfile {
        option: &'static str,
        profile: &'static str,
        reason: &'static str,
    },
}

impl ConfigError {
    /// Classifies an argument the parser did not match, naming the nearest
    /// option when the argument looks like a mistyped one.
    fn unrecognized(argument: &str) -> Self {
        if argument.starts_with('-') {
            Self::UnknownOption {
                argument: argument.to_owned(),
                suggestion: nearest_option(argument),
            }
        } else {
            Self::UnexpectedArgument {
                argument: argument.to_owned(),
            }
        }
    }

    fn invalid_value(option: &'static str, value: Option<String>, expected: &'static str) -> Self {
        Self::InvalidValue {
            option,
            value,
            expected,
        }
    }
}

impl fmt::Display for ConfigError {
    /// Format a configuration error with the relevant option and usage guidance.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption {
                argument,
                suggestion: Some(suggestion),
            } => write!(
                formatter,
                "unknown option '{argument}'; did you mean '{suggestion}'? \
                 Run with --help to list every option."
            ),
            Self::UnknownOption {
                argument,
                suggestion: None,
            } => write!(
                formatter,
                "unknown option '{argument}'. Run with --help to list every option."
            ),
            Self::UnexpectedArgument { argument } => write!(
                formatter,
                "unexpected argument '{argument}'. Run with --help to list every option."
            ),
            Self::ForeignToProfile {
                option,
                profile,
                reason,
            } => write!(
                formatter,
                "option '{option}' does not apply under --visual-profile {profile}: {reason}. \
                 Remove it, or select --visual-profile mudcrab."
            ),
            Self::MissingRequirement { option, requires } => write!(
                formatter,
                "option '{option}' requires '{requires}'. Run with --help to list every option."
            ),
            Self::InvalidValue {
                option,
                value: None,
                expected,
            } => write!(
                formatter,
                "option '{option}' needs a value: expected {expected}. \
                 Run with --help to list every option."
            ),
            // A value shaped like an option is almost always the next option
            // with the value of this one forgotten.
            Self::InvalidValue {
                option,
                value: Some(value),
                expected,
            } if value.starts_with("--") => write!(
                formatter,
                "option '{option}' needs a value: expected {expected}, but '{value}' is another \
                 option. Run with --help to list every option."
            ),
            Self::InvalidValue {
                option,
                value: Some(value),
                expected,
            } => write!(
                formatter,
                "option '{option}' does not accept '{value}': expected {expected}. \
                 Run with --help to list every option."
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl EngineConfig {
    pub fn streaming_controls_enabled(&self) -> bool {
        self.adaptive_streaming || self.max_streaming_backlog != 0 || self.streaming_memory_mib != 0
    }

    pub fn streaming_backlog_limit(&self) -> usize {
        if self.max_streaming_backlog == 0 && self.adaptive_streaming {
            256
        } else {
            self.max_streaming_backlog
        }
    }

    /// A limit on estimated streaming allocations, not process RSS. Without an
    /// explicit adaptive limit this returns the 8 GiB fallback; the runtime
    /// selects half detected RAM within the 16 GiB and system-headroom limits.
    /// Available system memory is checked separately before reservations.
    pub fn memory_budget_bytes(&self) -> Option<u64> {
        let mib = if self.streaming_memory_mib != 0 {
            self.streaming_memory_mib
        } else if self.adaptive_streaming {
            8_192
        } else {
            return None;
        };
        Some(
            mib.checked_mul(1024 * 1024)
                .expect("streaming memory size must fit bytes") as u64,
        )
    }

    pub fn streaming_headroom_bytes(&self) -> u64 {
        self.streaming_headroom_mib
            .checked_mul(1024 * 1024)
            .expect("streaming headroom size must fit bytes") as u64
    }

    /// Select only explicitly authored data; this does not simulate effective weather.
    pub fn image_space_selection(&self) -> color_eyre::Result<Option<ImageSpaceSelection>> {
        use color_eyre::eyre::ensure;
        ensure!(
            self.image_space.is_none()
                || (self.image_space_weather.is_none() && self.image_space_slot.is_none()),
            "--image-space cannot be combined with --image-space-weather or --image-space-slot"
        );
        ensure!(
            self.image_space_weather.is_some() == self.image_space_slot.is_some(),
            "--image-space-weather and --image-space-slot must be supplied together"
        );
        let selection = self
            .image_space
            .map(ImageSpaceSelection::Direct)
            .or_else(|| {
                self.image_space_weather
                    .zip(self.image_space_slot)
                    .map(|(form_id, slot)| ImageSpaceSelection::Weather { form_id, slot })
            });
        ensure!(
            self.color_grade != SceneGradeMode::RecordSrgb || selection.is_some(),
            "record-srgb requires an explicit --image-space or --image-space-weather with --image-space-slot"
        );
        ensure!(
            self.tone_map != SceneToneMapMode::RetailCurve || selection.is_some(),
            "retail-curve requires an explicit --image-space or --image-space-weather with --image-space-slot"
        );
        Ok(selection)
    }

    /// Refuses launch options that steer a pipeline the selected profile does not run. Saved
    /// settings may select the profile too, so this runs again once they are applied.
    pub fn check_visual_profile(&self) -> Result<(), ConfigError> {
        match self.visual_profile.foreign_option(&self.settings_overrides) {
            Some((option, reason)) => Err(ConfigError::ForeignToProfile {
                option,
                profile: self.visual_profile.name(),
                reason,
            }),
            None => Ok(()),
        }
    }

    /// The per-frame render-asset upload budget in bytes, or `None` when
    /// uploads are unlimited (`--max-upload-mib-per-frame 0`).
    pub fn max_upload_bytes_per_frame(&self) -> Option<usize> {
        (self.max_upload_mib_per_frame != 0)
            .then(|| self.max_upload_mib_per_frame.saturating_mul(1024 * 1024))
    }

    /// Interactive exterior play owns the mouse/controller; automated camera paths keep their
    /// existing movement and framing. A `--benchmark-jump` run is one of those even without a frame
    /// limit: the jump moves only the camera, and in WALK the camera follow would put it straight
    /// back at the player's body.
    pub fn interactive_world_physics(&self) -> bool {
        #[cfg(feature = "collision-measure")]
        if std::env::var_os("MUDCRAB_COLLISION_MEASURE").is_some() {
            return true;
        }
        !self.headless
            && !self.hidden_window
            && self.benchmark_jump.is_none()
            && !self.benchmark_only
            && self.benchmark_frames.is_none()
            && self.benchmark_duration_secs.is_none()
            && self.acceptance_screenshot.is_none()
            && self.shots.is_none()
            && self.matched_route.is_none()
            && self.auto_fly_speed <= 0.0
            && self.route.is_none()
            && self.streaming_benchmark_route_speed.is_none()
            && !self.material_fixture
            && !self.terrain_water_fixture
            && !self.transform_bounds_fixture
            && !self.renderer_fixture
            && !self.streaming_fixture
            && !self.physics_fixture
    }

    pub fn from_env() -> Result<ConfigAction, ConfigError> {
        Self::from_args(std::env::args().skip(1))
    }

    /// Whether this is a benchmark run: `--benchmark-frames` or `--benchmark-duration` was given.
    /// The same test the report uses to decide whether to write itself (`metrics::collect_and_finish`),
    /// so anything that only makes sense for a benchmark can key off it.
    pub fn is_benchmark_run(&self) -> bool {
        self.benchmark_frames.is_some()
            || self.benchmark_duration_secs.is_some()
            || self.streaming_benchmark_route_speed.is_some()
    }

    /// Whether the pacing instrumentation should run at all. It is measurement only, and only a
    /// benchmark report or the profiling campaign (itself a benchmark run) reads it, so an ordinary
    /// play session installs none of it. A jump without a frame limit or duration still installs it:
    /// the jump is documented to run without a report and warn that it has none.
    pub fn measures_pacing(&self) -> bool {
        self.is_benchmark_run() || self.benchmark_jump.is_some()
    }

    /// Whether the run moves the camera on its own: `--auto-fly-speed` or `--route`.
    pub fn flies(&self) -> bool {
        self.auto_fly_speed > 0.0 || self.route.is_some()
    }

    /// Whether the readiness scan records how far behind the flying camera each model finishes
    /// loading. Only the pacing report reads it, so a plain `--auto-fly-speed` run records nothing.
    pub fn records_fly_lag(&self) -> bool {
        self.flies() && self.measures_pacing()
    }

    /// The window's title: what kind of automated run this is and its `--run-label`, so a run on
    /// the taskbar says what it is. An interactive run is plain "Mudcrab".
    pub fn window_title(&self) -> String {
        let kind = if self.is_benchmark_run() {
            Some("benchmark")
        } else if self.streaming_fixture {
            Some("streaming fixture")
        } else if self.matched_route.is_some() {
            Some("matched-route")
        } else if self.shots.is_some() {
            Some("shots")
        } else {
            None
        };
        match (kind, self.run_label.as_deref()) {
            (Some(kind), Some(label)) => format!("Mudcrab - {kind}: {label}"),
            (Some(kind), None) => format!("Mudcrab - {kind}"),
            (None, Some(label)) => format!("Mudcrab - {label}"),
            (None, None) => "Mudcrab".to_owned(),
        }
    }

    /// Parses a command line into a [`ConfigAction`], or names the argument it
    /// does not recognise.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<ConfigAction, ConfigError> {
        let args: Vec<String> = args.into_iter().collect();
        // Help wins wherever it appears, so `--typo --help` prints the options
        // rather than failing on the earlier argument.
        if args
            .iter()
            .any(|argument| argument == "-h" || argument == "--help")
        {
            return Ok(ConfigAction::Help);
        }
        let mut config = Self::default();
        let mut ini = SkyrimIni::default();
        for pair in args.windows(2) {
            if pair[0] == "--ini" {
                let path = PathBuf::from(take_raw(
                    "--ini",
                    "a Skyrim INI file path",
                    Some(pair[1].clone()),
                )?);
                if let Err(error) = ini.merge_file(&path) {
                    eprintln!("warning: ignoring --ini {}: {error}", path.display());
                }
            }
        }
        ini.apply(&mut config);
        config.settings_overrides.extend(
            args.iter()
                .filter(|argument| argument.starts_with("--"))
                .cloned(),
        );
        let mut args = args.into_iter().peekable();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                // The pre-scan above answers both spellings; these arms keep the
                // option list the help-drift tests read complete.
                "-h" => return Ok(ConfigAction::Help),
                "--help" => return Ok(ConfigAction::Help),
                // A bare `--` is the conventional end-of-options marker. The
                // engine takes no positional arguments, so it has nothing to
                // end, and main ignored it: keep ignoring it.
                "--" => {}
                "--ini" => {
                    take_raw("--ini", "a Skyrim INI file path", args.next())?;
                }
                "--assets" => {
                    config.assets_dir = take_value(
                        "--assets",
                        "a directory holding converted assets",
                        args.next(),
                    )?;
                }
                "--bug-report-dir" => {
                    let path = take_raw(
                        "--bug-report-dir",
                        "a nonempty local report directory path",
                        args.next(),
                    )?;
                    if path.trim().is_empty() {
                        return Err(ConfigError::invalid_value(
                            "--bug-report-dir",
                            Some(path),
                            "a nonempty local report directory path",
                        ));
                    }
                    config.bug_report_dir = Some(PathBuf::from(path));
                }
                "--worldspace" => {
                    let value = take_raw(
                        "--worldspace",
                        "a worldspace form id, decimal or 0x-hex",
                        args.next(),
                    )?;
                    config.worldspace_id = parse_u32(&value).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--worldspace",
                            Some(value),
                            "a worldspace form id, decimal or 0x-hex",
                        )
                    })?;
                }
                "--grid-x" => {
                    config.start_grid.0 = take_value("--grid-x", "a cell coordinate", args.next())?;
                }
                "--grid-y" => {
                    config.start_grid.1 = take_value("--grid-y", "a cell coordinate", args.next())?;
                }
                "--stream-radius" => {
                    let raw = take_raw("--stream-radius", STREAM_RADIUS_EXPECTED, args.next())?;
                    let value = raw
                        .parse::<i32>()
                        .ok()
                        .filter(|value| (0..=MAX_STREAM_RADIUS).contains(value))
                        .ok_or_else(|| {
                            ConfigError::invalid_value(
                                "--stream-radius",
                                Some(raw),
                                STREAM_RADIUS_EXPECTED,
                            )
                        })?;
                    config.stream_radius = value;
                    config.unload_radius = value + 1;
                }
                "--headless" => config.headless = true,
                "--hidden-window" => config.hidden_window = true,
                "--max-unloads-per-frame" => {
                    config.max_cell_unloads_per_frame = take_value(
                        "--max-unloads-per-frame",
                        "a cell count, 0 for unlimited",
                        args.next(),
                    )?;
                }
                "--max-model-spawns-per-frame" => {
                    config.max_model_spawns_per_frame = take_value(
                        "--max-model-spawns-per-frame",
                        "a model count, 0 for unlimited",
                        args.next(),
                    )?;
                }
                "--prioritize-streaming" => config.prioritize_streaming = true,
                "--max-scene-loads" => {
                    config.max_scene_loads = take_value(
                        "--max-scene-loads",
                        "a scene job count, 0 for unlimited",
                        args.next(),
                    )?;
                }
                "--adaptive-streaming" => {
                    config.adaptive_streaming = true;
                    config.prioritize_streaming = true;
                }
                "--max-streaming-backlog" => {
                    config.max_streaming_backlog = take_value(
                        "--max-streaming-backlog",
                        "a downstream queue count, 0 to disable",
                        args.next(),
                    )?;
                }
                "--streaming-memory-mib" => {
                    config.streaming_memory_mib = take_mib(
                        "--streaming-memory-mib",
                        "a MiB size that fits bytes, 0 to disable",
                        args.next(),
                    )?;
                }
                "--streaming-costs" => {
                    config.streaming_costs = Some(take_value(
                        "--streaming-costs",
                        "a file path for streaming cost metadata",
                        args.next(),
                    )?);
                }
                "--streaming-headroom-mib" => {
                    config.streaming_headroom_mib = take_mib(
                        "--streaming-headroom-mib",
                        "a MiB size that fits bytes, 0 to reserve no headroom",
                        args.next(),
                    )?;
                }
                "--streaming-frame-ms" => {
                    config.streaming_frame_ms = take_number(
                        "--streaming-frame-ms",
                        "a positive number of milliseconds",
                        Bound::Positive,
                        args.next(),
                    )?;
                }
                // 0 keeps the automatic size; a count past the cap is refused like a value that
                // fails to parse.
                "--io-threads" => {
                    let raw = take_raw("--io-threads", IO_THREADS_EXPECTED, args.next())?;
                    config.io_threads = raw
                        .parse::<usize>()
                        .ok()
                        .filter(|value| *value <= MAX_IO_THREADS)
                        .ok_or_else(|| {
                            ConfigError::invalid_value(
                                "--io-threads",
                                Some(raw),
                                IO_THREADS_EXPECTED,
                            )
                        })?;
                }
                "--max-upload-mib-per-frame" => {
                    config.max_upload_mib_per_frame = take_value(
                        "--max-upload-mib-per-frame",
                        "a size in MiB, 0 for unlimited",
                        args.next(),
                    )?;
                }
                "--max-commit-ms" => {
                    let millis: f64 = take_number(
                        "--max-commit-ms",
                        "a positive number of milliseconds",
                        Bound::Positive,
                        args.next(),
                    )?;
                    config.max_commit_micros_per_frame =
                        (millis * 1_000.0).round().clamp(1.0, u64::MAX as f64) as u64;
                }
                "--benchmark-only" => config.benchmark_only = true,
                "--benchmark-frames" => {
                    config.benchmark_frames = Some(take_value(
                        "--benchmark-frames",
                        "a frame count",
                        args.next(),
                    )?);
                }
                "--benchmark-duration" => {
                    config.benchmark_duration_secs = Some(take_number(
                        "--benchmark-duration",
                        "a positive number of seconds",
                        Bound::Positive,
                        args.next(),
                    )?);
                }
                "--benchmark-warmup-frames" => {
                    config.benchmark_warmup_frames =
                        take_value("--benchmark-warmup-frames", "a frame count", args.next())?;
                }
                "--benchmark-vsync" => config.benchmark_vsync = true,
                "--streaming-benchmark-route-speed" => {
                    config.streaming_benchmark_route_speed = Some(take_number(
                        "--streaming-benchmark-route-speed",
                        "a finite speed in units per second, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?);
                }
                "--streaming-benchmark-route-secs" => {
                    config.streaming_benchmark_route_secs = take_number(
                        "--streaming-benchmark-route-secs",
                        "a positive finite number of seconds",
                        Bound::Positive,
                        args.next(),
                    )?;
                }
                "--streaming-benchmark-tail-secs" => {
                    config.streaming_benchmark_tail_secs = take_number(
                        "--streaming-benchmark-tail-secs",
                        "a finite number of seconds, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--streaming-benchmark-settle-secs" => {
                    config.streaming_benchmark_settle_secs = take_number(
                        "--streaming-benchmark-settle-secs",
                        "a finite number of seconds, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--streaming-benchmark-expected-models" => {
                    config.streaming_benchmark_expected_models = Some(take_value(
                        "--streaming-benchmark-expected-models",
                        "a model count",
                        args.next(),
                    )?);
                }
                "--streaming-benchmark-expected-lod-chunks" => {
                    config.streaming_benchmark_expected_lod_chunks = Some(take_value(
                        "--streaming-benchmark-expected-lod-chunks",
                        "a LOD chunk count",
                        args.next(),
                    )?);
                }
                "--streaming-benchmark-expected-terrain" => {
                    config.streaming_benchmark_expected_terrain = Some(take_value(
                        "--streaming-benchmark-expected-terrain",
                        "a terrain patch count",
                        args.next(),
                    )?);
                }
                "--streaming-benchmark-expected-water" => {
                    config.streaming_benchmark_expected_water = Some(take_value(
                        "--streaming-benchmark-expected-water",
                        "a water surface count",
                        args.next(),
                    )?);
                }
                // Unlike its neighbours, a bad jump only warns: the jump is an add-on to a
                // benchmark run, which still measures and writes its report without it.
                "--benchmark-jump" => match args.next_if(|value| !value.starts_with("--")) {
                    Some(raw) => match parse_grid(&raw) {
                        Some(value) if in_jump_grid_range(value) => {
                            config.benchmark_jump = Some(value)
                        }
                        Some((x, y)) => eprintln!(
                            "warning: ignoring --benchmark-jump {x},{y}: grid coordinates must be within {}..={JUMP_GRID_LIMIT}",
                            -JUMP_GRID_LIMIT
                        ),
                        None => eprintln!(
                            "warning: ignoring malformed --benchmark-jump {raw:?}; expected \"<grid-x>,<grid-y>\" integers"
                        ),
                    },
                    None => eprintln!(
                        "warning: missing value for --benchmark-jump; expected \"<grid-x>,<grid-y>\" integers"
                    ),
                },
                "--benchmark-output" => {
                    config.benchmark_output = take_value(
                        "--benchmark-output",
                        "a file path for the benchmark report",
                        args.next(),
                    )?;
                }
                // The value of these two may be left out, and a value left out must not swallow
                // the next option.
                "--run-label" => {
                    config.run_label = args.next_if(|value| !value.starts_with("--"));
                }
                "--benchmark-frame-times" => {
                    if let Some(value) = args.next_if(|value| !value.starts_with("--")) {
                        config.benchmark_frame_times = Some(value.into());
                    }
                }
                "--accept-min-fps" => {
                    config.accept_min_fps = take_number(
                        "--accept-min-fps",
                        "a frame rate, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--accept-p95-ms" => {
                    config.accept_p95_ms = take_number(
                        "--accept-p95-ms",
                        "a frame time in milliseconds, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--accept-max-memory-growth-gib" => {
                    config.accept_max_memory_growth_gib = take_number(
                        "--accept-max-memory-growth-gib",
                        "a memory size in GiB, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--auto-fly-speed" => {
                    config.auto_fly_speed = take_number(
                        "--auto-fly-speed",
                        "a speed in units per second, 0 or more",
                        Bound::NonNegative,
                        args.next(),
                    )?;
                }
                "--route" => {
                    config.route = Some(take_value("--route", "a route file path", args.next())?);
                }
                "--frame-budget-ms" => {
                    config.frame_budget_ms = take_number(
                        "--frame-budget-ms",
                        "a positive number of milliseconds",
                        Bound::Positive,
                        args.next(),
                    )?;
                }
                "--allow-incomplete-assets" => config.allow_incomplete_assets = true,
                "--synthetic-instances" => {
                    config.synthetic_instances =
                        take_value("--synthetic-instances", "an instance count", args.next())?;
                }
                "--profile-output" => {
                    config.profile_output_dir = Some(take_value(
                        "--profile-output",
                        "a directory path for the profile bundle",
                        args.next(),
                    )?);
                }
                "--profile-scenario" => {
                    config.profile_scenario =
                        take_value("--profile-scenario", "a scenario name", args.next())?;
                }
                "--profile-run-id" => {
                    config.profile_run_id =
                        take_value("--profile-run-id", "a run id", args.next())?;
                }
                "--profile-commit" => {
                    config.profile_commit =
                        take_value("--profile-commit", "a commit hash", args.next())?;
                }
                "--profile-dirty-worktree" => config.profile_dirty_worktree = true,
                "--profile-gpu-inventory" => config.profile_gpu_inventory = true,
                "--profile-hardware" => {
                    config.profile_hardware = take_value(
                        "--profile-hardware",
                        "a description of the hardware",
                        args.next(),
                    )?;
                }
                "--acceptance-screenshot" => {
                    config.acceptance_screenshot = Some(take_value(
                        "--acceptance-screenshot",
                        "a file path for the screenshot",
                        args.next(),
                    )?);
                }
                "--screenshot-camera-offset" => {
                    const EXPECTED: &str = "three finite numbers \"x,y,z\"";
                    let raw = take_raw("--screenshot-camera-offset", EXPECTED, args.next())?;
                    config.screenshot_camera_offset =
                        Some(parse_offset(&raw).ok_or_else(|| {
                            ConfigError::invalid_value(
                                "--screenshot-camera-offset",
                                Some(raw),
                                EXPECTED,
                            )
                        })?);
                }
                // A path left out is an error, and must not swallow the next option: a `--shots`
                // with no path is no mode at all, and continuing would silently launch an ordinary
                // interactive run.
                "--matched-route" => {
                    config.matched_route = Some(take_value(
                        "--matched-route",
                        "a route fixture",
                        args.next(),
                    )?);
                }
                "--color-grade" => {
                    let raw = take_raw("--color-grade", SCENE_GRADE_VALUES, args.next())?;
                    config.color_grade = SceneGradeMode::parse(&raw).ok_or_else(|| {
                        ConfigError::invalid_value("--color-grade", Some(raw), SCENE_GRADE_VALUES)
                    })?;
                }
                "--grade-preset" => {
                    config.grade_preset =
                        Some(take_value("--grade-preset", "a JSON file", args.next())?);
                }
                "--tone-map" => {
                    let raw = take_raw("--tone-map", "tony or retail-curve", args.next())?;
                    config.tone_map = SceneToneMapMode::parse(&raw).ok_or_else(|| {
                        ConfigError::invalid_value("--tone-map", Some(raw), "tony or retail-curve")
                    })?;
                }
                "--tone-map-params" => {
                    config.tone_map_params =
                        Some(take_value("--tone-map-params", "a JSON file", args.next())?);
                }
                "--image-space" => {
                    let raw =
                        take_raw("--image-space", "a FormID, decimal or 0x-hex", args.next())?;
                    config.image_space = Some(parse_u32(&raw).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--image-space",
                            Some(raw),
                            "a FormID, decimal or 0x-hex",
                        )
                    })?);
                }
                "--image-space-weather" => {
                    let raw = take_raw(
                        "--image-space-weather",
                        "a FormID, decimal or 0x-hex",
                        args.next(),
                    )?;
                    config.image_space_weather = Some(parse_u32(&raw).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--image-space-weather",
                            Some(raw),
                            "a FormID, decimal or 0x-hex",
                        )
                    })?);
                }
                "--image-space-slot" => {
                    let raw = take_raw(
                        "--image-space-slot",
                        "sunrise, day, sunset or night",
                        args.next(),
                    )?;
                    config.image_space_slot =
                        Some(ImageSpaceSlot::parse(&raw).ok_or_else(|| {
                            ConfigError::invalid_value(
                                "--image-space-slot",
                                Some(raw),
                                "sunrise, day, sunset or night",
                            )
                        })?);
                }
                "--visual-profile" => {
                    let raw = take_raw("--visual-profile", VISUAL_PROFILE_VALUES, args.next())?;
                    config.visual_profile = VisualProfile::parse(&raw).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--visual-profile",
                            Some(raw),
                            VISUAL_PROFILE_VALUES,
                        )
                    })?;
                }
                "--visual-preset" => {
                    let raw = take_raw("--visual-preset", "vivid or cool-gray", args.next())?;
                    config.visual_preset = SceneVisualPreset::parse(&raw).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--visual-preset",
                            Some(raw),
                            "vivid or cool-gray",
                        )
                    })?;
                }
                "--sun-position" => {
                    let raw = take_raw("--sun-position", SUN_POSITION_EXPECTED, args.next())?;
                    config.sun_position = Some(parse_sun_position(&raw).ok_or_else(|| {
                        ConfigError::invalid_value(
                            "--sun-position",
                            Some(raw),
                            SUN_POSITION_EXPECTED,
                        )
                    })?);
                }
                "--shots" => {
                    config.shots = Some(take_value("--shots", "a shots file path", args.next())?);
                }
                "--start-shot" => {
                    config.start_shot = Some(take_value(
                        "--start-shot",
                        "a one-pose shots file",
                        args.next(),
                    )?);
                }
                "--stdin-console" => config.stdin_console = true,
                "--no-render-diagnostics" => config.no_render_diagnostics = true,
                "--shots-settle-timeout" => {
                    const EXPECTED: &str = "a positive number of seconds up to 600";
                    let raw = take_raw("--shots-settle-timeout", EXPECTED, args.next())?;
                    config.shots_settle_timeout = raw
                        .parse::<f32>()
                        .ok()
                        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 600.0)
                        .ok_or_else(|| {
                            ConfigError::invalid_value(
                                "--shots-settle-timeout",
                                Some(raw),
                                EXPECTED,
                            )
                        })?;
                }
                "--shots-out" => {
                    config.shots_out = Some(take_value(
                        "--shots-out",
                        "a directory for the shots' images and log",
                        args.next(),
                    )?);
                }
                "--diagnostic-asset-fallbacks" => config.diagnostic_asset_fallbacks = true,
                "--material-fixture" => config.material_fixture = true,
                "--terrain-water-fixture" => config.terrain_water_fixture = true,
                "--terrain-water-fixture-msaa" => {
                    config.terrain_water_fixture_msaa =
                        parse_msaa("--terrain-water-fixture-msaa", args.next())?
                }
                "--transform-bounds-fixture" => config.transform_bounds_fixture = true,
                "--renderer-fixture" => config.renderer_fixture = true,
                "--streaming-fixture" => config.streaming_fixture = true,
                "--lights" => config.lights = true,
                "--physics-fixture" => config.physics_fixture = true,
                unknown => return Err(ConfigError::unrecognized(unknown)),
            }
        }
        if config.terrain_water_fixture_msaa != Msaa::Off && !config.terrain_water_fixture {
            return Err(ConfigError::MissingRequirement {
                option: "--terrain-water-fixture-msaa",
                requires: "--terrain-water-fixture",
            });
        }
        if config.streaming_benchmark_route_speed.is_some()
            && (config.benchmark_only
                || config.acceptance_screenshot.is_some()
                || config.shots.is_some()
                || config.matched_route.is_some()
                || config.route.is_some()
                || config.auto_fly_speed > 0.0
                || config.material_fixture
                || config.terrain_water_fixture
                || config.transform_bounds_fixture
                || config.renderer_fixture
                || config.streaming_fixture
                || config.physics_fixture)
        {
            return Err(ConfigError::invalid_value(
                "--streaming-benchmark-route-speed",
                config
                    .streaming_benchmark_route_speed
                    .map(|speed| speed.to_string()),
                "a world camera route without screenshots, shots, a route file, synthetic mode or fixtures",
            ));
        }
        // A route waits for CPU settlement with no other bound, so it needs the real-time deadline.
        if config.streaming_benchmark_route_speed.is_some()
            && config.benchmark_duration_secs.is_none()
        {
            return Err(ConfigError::MissingRequirement {
                option: "--streaming-benchmark-route-speed",
                requires: "--benchmark-duration",
            });
        }
        if let Some(speed) = config.streaming_benchmark_route_speed
            && (!(speed * config.streaming_benchmark_route_secs).is_finite()
                || !(config.streaming_benchmark_route_secs + config.streaming_benchmark_tail_secs)
                    .is_finite())
        {
            return Err(ConfigError::invalid_value(
                "--streaming-benchmark-route-secs",
                Some(config.streaming_benchmark_route_secs.to_string()),
                "a duration whose route distance and total duration remain finite",
            ));
        }
        // The inventory is written into the profile bundle; without one it would do nothing.
        if config.profile_gpu_inventory && config.profile_output_dir.is_none() {
            return Err(ConfigError::MissingRequirement {
                option: "--profile-gpu-inventory",
                requires: "--profile-output",
            });
        }
        config.check_visual_profile()?;
        config.drop_jump_the_run_would_overwrite();
        Ok(ConfigAction::Run(Box::new(config)))
    }

    /// The reason text for a fixture run that builds no streaming runtime (see `app.rs`), or
    /// `None` when none of those fixtures is selected.
    fn non_streaming_fixture(&self) -> Option<&'static str> {
        if self.material_fixture {
            Some("--material-fixture creates no streaming runtime to jump in")
        } else if self.terrain_water_fixture {
            Some("--terrain-water-fixture creates no streaming runtime to jump in")
        } else if self.transform_bounds_fixture {
            Some("--transform-bounds-fixture creates no streaming runtime to jump in")
        } else if self.renderer_fixture {
            Some("--renderer-fixture creates no streaming runtime to jump in")
        } else if self.physics_fixture {
            Some("--physics-fixture creates no streaming runtime to jump in")
        } else {
            None
        }
    }

    /// `--benchmark-jump` moves the camera once, so a run that keeps driving it would undo the
    /// jump: auto-fly keeps moving it, the streaming fixture moves it every frame, a shots run poses
    /// it for each shot, and a screenshot run anchors streaming on the start cell. A synthetic
    /// `--benchmark-only` run loads no world, so nothing would carry the jump out.
    fn drop_jump_the_run_would_overwrite(&mut self) {
        let reason = if self.benchmark_only {
            Some("--benchmark-only loads no world to jump in")
        } else if let Some(fixture) = self.non_streaming_fixture() {
            Some(fixture)
        } else if self.auto_fly_speed > 0.0 {
            Some("--auto-fly-speed keeps driving the camera")
        } else if self.route.is_some() {
            Some("--route keeps driving the camera")
        } else if self.acceptance_screenshot.is_some() {
            Some("--acceptance-screenshot anchors streaming on the start cell")
        } else if self.streaming_fixture {
            Some("--streaming-fixture moves the camera every frame")
        } else if self.shots.is_some() {
            Some("--shots sets the camera pose for each shot")
        } else if self.streaming_benchmark_route_speed.is_some() {
            Some("--streaming-benchmark-route-speed owns the camera")
        } else if self.matched_route.is_some() {
            Some("--matched-route sets the camera pose for each route step")
        } else {
            None
        };
        let Some(reason) = reason else {
            // The jump still runs, but with no frame limit or duration no benchmark report (which
            // carries the pacing fields) is written, so there is nothing to read the jump out of.
            if let Some((x, y)) = self.benchmark_jump
                && !self.is_benchmark_run()
            {
                eprintln!(
                    "warning: --benchmark-jump {x},{y} has no report to appear in without --benchmark-frames or --benchmark-duration"
                );
            }
            return;
        };
        if let Some((x, y)) = self.benchmark_jump.take() {
            eprintln!("warning: ignoring --benchmark-jump {x},{y}: {reason}");
        }
    }
}

/// The widest `--stream-radius` the engine takes: the unload ring is one cell wider, and the
/// streamer measures that ring as `2 * unload_radius + 1` cells across in an `i32`, which this
/// keeps from overflowing.
pub const MAX_STREAM_RADIUS: i32 = (i32::MAX - 3) / 2;

/// What `--stream-radius` takes, with [`MAX_STREAM_RADIUS`] spelled out; a test holds the two
/// equal.
const STREAM_RADIUS_EXPECTED: &str = "a cell count from 0 to 1073741822";

/// What `--sun-position` takes: azimuth clockwise from north, then elevation above the horizon.
const SUN_POSITION_EXPECTED: &str =
    "two finite angles in degrees: azimuth,elevation (elevation 0 to 90)";

/// Test shorthand for a command line that must ask for a run.
#[cfg(test)]
impl EngineConfig {
    pub(crate) fn run_from_args(args: impl IntoIterator<Item = String>) -> Self {
        match Self::from_args(args) {
            Ok(ConfigAction::Run(config)) => *config,
            other => panic!("expected a run configuration, got {other:?}"),
        }
    }
}

fn parse_offset(value: &str) -> Option<(f32, f32, f32)> {
    let mut parts = value.split(',');
    let x: f32 = parts.next()?.trim().parse().ok()?;
    let y: f32 = parts.next()?.trim().parse().ok()?;
    let z: f32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() || !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return None;
    }
    Some((x, y, z))
}

/// The furthest grid coordinate `--benchmark-jump` accepts. The jump position maths subtracts the
/// render origin and scales by the cell size, so an extreme `i32` would overflow; half the distance
/// to `i32::MAX` in cells is far past any converted world.
const JUMP_GRID_LIMIT: i32 = 512;

fn in_jump_grid_range((x, y): (i32, i32)) -> bool {
    (-JUMP_GRID_LIMIT..=JUMP_GRID_LIMIT).contains(&x)
        && (-JUMP_GRID_LIMIT..=JUMP_GRID_LIMIT).contains(&y)
}

fn parse_grid(value: &str) -> Option<(i32, i32)> {
    let mut parts = value.split(',');
    let x: i32 = parts.next()?.trim().parse().ok()?;
    let y: i32 = parts.next()?.trim().parse().ok()?;
    parts.next().is_none().then_some((x, y))
}

fn parse_sun_position(value: &str) -> Option<(f32, f32)> {
    let mut parts = value.split(',');
    let azimuth: f32 = parts.next()?.trim().parse().ok()?;
    let elevation: f32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some()
        || !azimuth.is_finite()
        || !elevation.is_finite()
        || !(0.0..=90.0).contains(&elevation)
    {
        return None;
    }
    Some((azimuth.rem_euclid(360.0), elevation))
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

/// The value of a value-taking option, as written.
///
/// A missing value, or one that is another option, is an error rather than
/// something to ignore: `--profile-hardware --worldspace` would otherwise
/// swallow the second option, leaving the run in a state neither spelling
/// asked for.
fn take_raw(
    option: &'static str,
    expected: &'static str,
    value: Option<String>,
) -> Result<String, ConfigError> {
    match value {
        Some(value) if !value.starts_with("--") => Ok(value),
        // Only `--` marks an option: a path or a negative number may start
        // with a single dash.
        other => Err(ConfigError::invalid_value(option, other, expected)),
    }
}

/// As [`take_raw`], parsed into the type the option feeds.
fn take_value<T: std::str::FromStr>(
    option: &'static str,
    expected: &'static str,
    value: Option<String>,
) -> Result<T, ConfigError> {
    let value = take_raw(option, expected, value)?;
    value
        .parse::<T>()
        .map_err(|_| ConfigError::invalid_value(option, Some(value), expected))
}

fn parse_msaa(option: &'static str, value: Option<String>) -> Result<Msaa, ConfigError> {
    const EXPECTED: &str = "1, 2, 4 or 8";
    let raw = take_raw(option, EXPECTED, value)?;
    match raw.as_str() {
        "1" | "off" => Ok(Msaa::Off),
        "2" => Ok(Msaa::Sample2),
        "4" => Ok(Msaa::Sample4),
        "8" => Ok(Msaa::Sample8),
        _ => Err(ConfigError::invalid_value(option, Some(raw), EXPECTED)),
    }
}

fn take_mib(
    option: &'static str,
    expected: &'static str,
    value: Option<String>,
) -> Result<usize, ConfigError> {
    let raw = take_raw(option, expected, value)?;
    raw.parse::<usize>()
        .ok()
        .filter(|mib| mib.checked_mul(1024 * 1024).is_some())
        .ok_or_else(|| ConfigError::invalid_value(option, Some(raw), expected))
}

/// The range a numeric option's value must fall in. Neither admits NaN or an infinity.
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// Greater than zero.
    Positive,
    /// Zero or greater.
    NonNegative,
}

/// As [`take_value`] for a floating-point option: the value must also be finite and within
/// `bound`, or it is refused as written.
fn take_number<T>(
    option: &'static str,
    expected: &'static str,
    bound: Bound,
    value: Option<String>,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr + Copy + Into<f64>,
{
    let raw = take_raw(option, expected, value)?;
    let accepted = raw.parse::<T>().ok().filter(|value| {
        let value: f64 = (*value).into();
        value.is_finite()
            && match bound {
                Bound::Positive => value > 0.0,
                Bound::NonNegative => value >= 0.0,
            }
    });
    accepted.ok_or_else(|| ConfigError::invalid_value(option, Some(raw), expected))
}

/// The option `argument` most likely meant, if it is within two edits of one.
/// Ties go to the first option the help text lists.
///
/// The candidates are [`HELP_TEXT`]'s option tokens, which the tests hold equal
/// to the parser's match arms.
fn nearest_option(argument: &str) -> Option<&'static str> {
    let mut nearest: Option<(usize, &'static str)> = None;
    for option in option_tokens(HELP_TEXT) {
        let distance = edit_distance(argument, option);
        if distance <= 2 && nearest.is_none_or(|(best, _)| distance < best) {
            nearest = Some((distance, option));
        }
    }
    nearest.map(|(_, option)| option)
}

/// Every `--option` token in `text`, so the help text and the parser arms can be
/// compared without a third list of option names to keep in step.
///
/// A token counts only where it starts a word (see [`starts_a_word`]), so
/// `word--word` and `0--10` inside a string are not read as options.
fn option_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("--") {
        let after = &rest[start + 2..];
        let length = if after.starts_with(|character: char| character.is_ascii_alphanumeric()) {
            after
                .find(|character: char| !is_option_character(character))
                .unwrap_or(after.len())
        } else {
            // A bare `--` separator, or a rule of dashes, is not an option.
            0
        };
        if length > 0 && starts_a_word(rest, start) {
            tokens.push(&rest[start..start + 2 + length]);
        }
        // Advance past the token, or past one dash of a separator so a rule of
        // dashes cannot loop. A `--` at the very end of the text ends the scan.
        rest = after.get(length.max(1)..).unwrap_or_default();
    }
    tokens
}

/// True when `index` in `text` starts a word: the text's start, a whitespace
/// boundary, the far side of a quote (how the PowerShell scripts spell an
/// option), or shell punctuation such as `(`, `=` or `;`.
fn starts_a_word(text: &str, index: usize) -> bool {
    text[..index].chars().next_back().is_none_or(|previous| {
        previous.is_whitespace()
            || matches!(previous, '"' | '\'' | '(' | '=' | ';' | '|' | '&' | ',')
    })
}

/// Options are spelled with ASCII lower case letters, digits and inner dashes.
fn is_option_character(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
}

/// Levenshtein distance, used only to propose a correction for a mistyped
/// option.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (index, left_character) in left.chars().enumerate() {
        current[0] = index + 1;
        for (offset, right_character) in right.iter().enumerate() {
            let substitution = previous[offset] + usize::from(left_character != *right_character);
            current[offset + 1] = substitution
                .min(previous[offset + 1] + 1)
                .min(current[offset] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This file, so the tests can read the parser's own match arms back out of
    /// it instead of keeping a second list of option names in step by hand.
    const CONFIG_SOURCE: &str = include_str!("config.rs");

    /// Flags owned by one release script: (file name, flag). Scoped like the
    /// observation tool's `--verify`, so any other script passing one of them
    /// to the engine still fails the audit.
    const SCRIPT_SCOPED_FLAGS: &[(&str, &str)] = &[
        ("install.sh", "--progress-bar"), // curl
        ("stage-release.sh", "--dest"),
        ("stage-release.sh", "--target"),
    ];

    /// Flags consumed by Cargo, Git, `world-inspect`, the dynamic loader,
    /// audit tools and launch wrappers. Wrappers also contain engine invocations,
    /// so every other scanned `--flag` must be accepted by the engine parser.
    const NON_ENGINE_FLAGS: &[&str] = &[
        "--all",                 // cargo fmt
        "--all-targets",         // cargo test, cargo clippy
        "--bin",                 // cargo test
        "--bins",                // cargo build
        "--check",               // cargo fmt
        "--release",             // cargo build
        "--workspace",           // cargo build, cargo test, cargo clippy
        "--ignore-submodules",   // git diff
        "--quiet",               // git diff
        "--short",               // git rev-parse
        "--output", // world-inspect, profiling, startup runner, repeat-terrain-startup.py
        "--pairs-output", // correlate_drawables.py
        "--radius", // world-inspect
        "--library-path", // ld-linux
        "--meshes", // audit-collision.py
        "--min-x",  // audit-collision.py
        "--max-x",  // audit-collision.py
        "--min-y",  // audit-collision.py
        "--max-y",  // audit-collision.py
        "--out",    // audit-collision.py
        "--expect-solid", // audit-collision.py
        "--expect-passable", // audit-collision.py
        "--original", // audit_asset_sizes.py
        "--json",   // audit_asset_sizes.py
        "--candidate-inventory", // audit-riverwood-reuse.py
        "--reference-inventory", // audit-riverwood-reuse.py
        "--manifest", // audit-riverwood-reuse.py
        "--include", // audit-z-fighting.py
        "--materials-only", // audit-z-fighting.py
        "--no-placements", // audit-z-fighting.py
        "--plane-tolerance", // audit-z-fighting.py
        "--min-overlap-area", // audit-z-fighting.py
        "--max-triangles", // audit-z-fighting.py
        "--max-comparisons", // audit-z-fighting.py
        "--max-examples", // audit-z-fighting.py
        "--progress", // audit-z-fighting.py
        "--jobs",   // audit-z-fighting.py
        "--locked", // cargo run
        "--manifest-path", // cargo run
        "--cpu-jobs", // converter
        "--io-jobs", // converter
        "--binary", // git diff
        "--porcelain", // git status
        "--engine", // repeat-terrain-startup.py
        "--repeats", // repeat-terrain-startup.py
        "--upload-budgets", // repeat-terrain-startup.py
        "--warmup", // repeat-terrain-startup.py
        "--duration", // repeat-terrain-startup.py
        "--timeout", // repeat-terrain-startup.py
        "--launch-prefix", // repeat-terrain-startup.py
        "--commit", // repeat-terrain-startup.py
        "--costs",  // compare-streaming-modes.py
        "--thermal-probe", // compare-streaming-modes.py
        "--ablation-repeats", // compare-streaming-modes.py
        "--resume", // compare-streaming-modes.py
        "--seed",   // streaming comparison scripts
        "--smoke",  // compare-streaming-modes.py
        "--experiment", // analyze-streaming-comparison.py
        "--output-dir", // analyze-streaming-comparison.py
        "--bootstrap-samples", // analyze-streaming-comparison.py
        "--comparison", // analyze-streaming-comparison.py
        "--output", // world inspection, profiling, startup runner
        "--pairs-output", // correlate_drawables.py
        "--native-preset", // prepare_skyrim_comparison.py
        "--calibration", // prepare_skyrim_comparison.py
        "--timeout", // run_skyrim_comparison.py
        "--user",   // systemctl
        "--property", // systemctl
        "--value",  // systemctl
        "--onlyvisible", // xdotool
        "--name",   // xdotool
        "--sync",   // xdotool
        "--psm",    // tesseract
        "--radius", // world-inspect
        "--library-path", // ld-linux
        "--meshes", // audit-collision.py
        "--min-x",  // audit-collision.py
        "--max-x",  // audit-collision.py
        "--min-y",  // audit-collision.py
        "--max-y",  // audit-collision.py
        "--out",    // audit-collision.py
        "--expect-solid", // audit-collision.py
        "--expect-passable", // audit-collision.py
        "--original", // audit_asset_sizes.py
        "--json",   // audit_asset_sizes.py
        "--candidate-inventory", // audit-riverwood-reuse.py
        "--reference-inventory", // audit-riverwood-reuse.py
        "--manifest", // audit-riverwood-reuse.py
        "--include", // audit-z-fighting.py
        "--materials-only", // audit-z-fighting.py
        "--no-placements", // audit-z-fighting.py
        "--plane-tolerance", // audit-z-fighting.py
        "--min-overlap-area", // audit-z-fighting.py
        "--max-triangles", // audit-z-fighting.py
        "--max-comparisons", // audit-z-fighting.py
        "--max-examples", // audit-z-fighting.py
        "--progress", // audit-z-fighting.py
        "--jobs",   // audit-z-fighting.py
        "--locked", // cargo run
        "--manifest-path", // cargo run
        "--cpu-jobs", // converter
        "--io-jobs", // converter
        "--binary", // git diff
        "--porcelain", // git status
        "--engine", // repeat-terrain-startup.py
        "--repeats", // repeat-terrain-startup.py
        "--upload-budgets", // repeat-terrain-startup.py
        "--warmup", // repeat-terrain-startup.py
        "--duration", // repeat-terrain-startup.py
        "--timeout", // repeat-terrain-startup.py
        "--launch-prefix", // repeat-terrain-startup.py
        "--commit", // repeat-terrain-startup.py
        "--config", // capture_tonemap_tuning.py
        "--validate-only", // capture_tonemap_tuning.py
        "--clearmodifiers", // xdotool
        "--delay",  // xdotool
        "--console-open", // capture_skyrim_tonemap_state.py
        "--event",  // inspect_skyrim_gpu_capture.py, sample_skyrim_tonemap_gpu.py
        "--max-actions", // inspect_skyrim_gpu_capture.py
        "--max-resource-mib", // inspect_skyrim_gpu_capture.py
        "--max-resources", // inspect_skyrim_gpu_capture.py
        "--max-total-mib", // inspect_skyrim_gpu_capture.py
        "--previews", // inspect_skyrim_gpu_capture.py
        "--tail",   // inspect_skyrim_gpu_capture.py
        "--exe",    // inspect_skyrim_tonemap_memory.py
        "--pid",    // inspect_skyrim_tonemap_memory.py
        "--atol",   // replay_skyrim_tonemap.py
        "--dot-mode", // replay_skyrim_tonemap.py
        "--input",  // replay_skyrim_tonemap.py
        "--mad-mode", // replay_skyrim_tonemap.py
        "--rtol",   // replay_skyrim_tonemap.py
        "--capture-sha256", // sample_skyrim_tonemap_gpu.py
        "--output-id", // sample_skyrim_tonemap_gpu.py
        "--pixel",  // sample_skyrim_tonemap_gpu.py
        "--shader-id", // sample_skyrim_tonemap_gpu.py
        "--source-shader", // sample_skyrim_tonemap_gpu.py
        "--stage3-relative", // compare_stage_receipts.py
        "--stage4-bytes", // compare_stage_receipts.py
    ];

    fn run_config(arguments: &[&str]) -> EngineConfig {
        EngineConfig::run_from_args(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    fn parse_error(arguments: &[&str]) -> ConfigError {
        match EngineConfig::from_args(arguments.iter().map(|argument| (*argument).to_owned())) {
            Err(error) => error,
            Ok(action) => panic!("expected an error, got {action:?}"),
        }
    }

    /// The option literals the parser's `match` arms name, in source order.
    fn parser_options() -> Vec<&'static str> {
        CONFIG_SOURCE
            .lines()
            .filter_map(|line| {
                let (option, rest) = leading_quoted(line)?;
                // The bare `--` arm is a no-op marker, not an option to document.
                (rest.trim_start().starts_with("=>") && option != "--").then_some(option)
            })
            .collect()
    }

    /// The quoted literal `line` begins with, and the rest of the line.
    fn leading_quoted(line: &str) -> Option<(&str, &str)> {
        let (option, rest) = line.trim().strip_prefix('"')?.split_once('"')?;
        Some((option, rest))
    }

    #[test]
    fn defaults_to_one_cell_commit_within_a_sixty_fps_frame() {
        let config = EngineConfig::default();
        assert_eq!(config.max_cell_commits_per_frame, 1);
        assert_eq!(config.max_commit_micros_per_frame, 16_670);
        assert_eq!(config.max_cell_unloads_per_frame, 2);
        assert_eq!(config.max_model_spawns_per_frame, 32);
        assert_eq!(config.io_threads, 0);
    }

    /// Verify GPU inventory is opt-in and requires a profile output directory.
    #[test]
    fn gpu_inventory_is_separate_from_normal_profile_output() {
        assert!(!EngineConfig::default().profile_gpu_inventory);
        assert!(!run_config(&["--profile-output", "profiles/test"]).profile_gpu_inventory);
        let diagnostic = run_config(&[
            "--profile-gpu-inventory",
            "--profile-output",
            "profiles/test",
        ]);
        assert!(diagnostic.profile_gpu_inventory);
        assert_eq!(
            diagnostic.profile_output_dir,
            Some(PathBuf::from("profiles/test"))
        );
        // The switch alone must not silently do nothing or implicitly enable profile collection.
        let error = parse_error(&["--profile-gpu-inventory"]);
        assert!(matches!(
            error,
            ConfigError::MissingRequirement {
                option: "--profile-gpu-inventory",
                requires: "--profile-output",
            }
        ));
        assert_eq!(
            error.to_string(),
            "option '--profile-gpu-inventory' requires '--profile-output'. \
             Run with --help to list every option."
        );
    }

    #[test]
    fn demand_scheduling_is_opt_in_and_keeps_zero_unlimited() {
        let defaults = EngineConfig::default();
        assert!(!defaults.prioritize_streaming);
        assert_eq!(defaults.max_scene_loads, 0);
        let candidate = run_config(&["--prioritize-streaming", "--max-scene-loads", "32"]);
        assert!(candidate.prioritize_streaming);
        assert_eq!(candidate.max_scene_loads, 32);
        assert_eq!(run_config(&["--max-scene-loads", "0"]).max_scene_loads, 0);
        assert!(matches!(
            parse_error(&["--max-scene-loads", "-1"]),
            ConfigError::InvalidValue { .. }
        ));
    }

    #[test]
    fn pipeline_controls_default_off_and_preserve_fixed_limits() {
        let config = run_config(&[]);
        assert!(!config.adaptive_streaming);
        assert!(!config.streaming_controls_enabled());
        assert_eq!(config.streaming_backlog_limit(), 0);
        assert_eq!(config.memory_budget_bytes(), None);
        assert_eq!(config.streaming_headroom_bytes(), 2_048 * 1024 * 1024);
        assert_eq!(config.streaming_frame_ms, 16.67);
        assert!(config.streaming_costs.is_none());
        assert_eq!(config.max_scene_loads, 0);
        assert_eq!(config.max_model_spawns_per_frame, 32);
        assert_eq!(config.max_upload_bytes_per_frame(), Some(16 * 1024 * 1024));
    }

    #[test]
    fn dedicated_streaming_benchmark_route_is_opt_in_and_accepts_stationary_speed() {
        let defaults = run_config(&[]);
        assert!(!defaults.benchmark_vsync);
        assert!(defaults.streaming_benchmark_route_speed.is_none());
        assert_eq!(defaults.streaming_benchmark_settle_secs, 5.0);
        let config = run_config(&[
            "--streaming-benchmark-route-speed",
            "0",
            "--streaming-benchmark-route-secs",
            "45",
            "--streaming-benchmark-tail-secs",
            "10",
            "--streaming-benchmark-settle-secs",
            "5",
            "--streaming-benchmark-expected-models",
            "2029",
            "--streaming-benchmark-expected-lod-chunks",
            "176",
            "--streaming-benchmark-expected-terrain",
            "100",
            "--streaming-benchmark-expected-water",
            "25",
            "--benchmark-vsync",
            "--benchmark-duration",
            "210",
        ]);
        assert_eq!(config.streaming_benchmark_route_speed, Some(0.0));
        assert_eq!(config.streaming_benchmark_route_secs, 45.0);
        assert_eq!(config.streaming_benchmark_tail_secs, 10.0);
        assert_eq!(config.streaming_benchmark_expected_models, Some(2029));
        assert_eq!(config.streaming_benchmark_expected_lod_chunks, Some(176));
        assert_eq!(config.streaming_benchmark_expected_terrain, Some(100));
        assert_eq!(config.streaming_benchmark_expected_water, Some(25));
        assert!(config.benchmark_vsync);
        assert!(!config.interactive_world_physics());
        assert!(!config.streaming_controls_enabled());
    }

    #[test]
    fn benchmark_route_requires_a_real_time_deadline() {
        let error = parse_error(&["--streaming-benchmark-route-speed", "370"]);
        assert!(matches!(
            error,
            ConfigError::MissingRequirement {
                option: "--streaming-benchmark-route-speed",
                requires: "--benchmark-duration",
            }
        ));
        let config = run_config(&[
            "--streaming-benchmark-route-speed",
            "370",
            "--benchmark-duration",
            "210",
        ]);
        assert_eq!(config.benchmark_duration_secs, Some(210.0));
    }

    #[test]
    fn benchmark_route_rejects_invalid_numbers_and_anchored_camera_modes() {
        for option in [
            "--streaming-benchmark-route-speed",
            "--streaming-benchmark-tail-secs",
            "--streaming-benchmark-settle-secs",
        ] {
            for value in ["-1", "NaN", "inf"] {
                assert!(matches!(
                    parse_error(&[option, value]),
                    ConfigError::InvalidValue { .. }
                ));
            }
        }
        for value in ["0", "-1", "NaN", "inf"] {
            assert!(matches!(
                parse_error(&["--streaming-benchmark-route-secs", value]),
                ConfigError::InvalidValue { .. }
            ));
        }
        for args in [
            vec![
                "--streaming-benchmark-route-speed",
                "370",
                "--acceptance-screenshot",
                "out.png",
            ],
            vec![
                "--streaming-benchmark-route-speed",
                "370",
                "--shots",
                "poses.json",
            ],
            vec![
                "--streaming-benchmark-route-speed",
                "370",
                "--benchmark-only",
            ],
            vec![
                "--streaming-benchmark-route-speed",
                "370",
                "--physics-fixture",
            ],
            vec![
                "--streaming-benchmark-route-speed",
                "1e308",
                "--streaming-benchmark-route-secs",
                "1e308",
            ],
        ] {
            let mut args = args;
            args.extend(["--benchmark-duration", "60"]);
            assert!(matches!(
                parse_error(&args),
                ConfigError::InvalidValue { .. }
            ));
        }
    }

    #[test]
    fn adaptive_mode_enables_priority_and_finite_automatic_pipeline_limits() {
        let config = run_config(&["--adaptive-streaming"]);
        assert!(config.adaptive_streaming);
        assert!(config.prioritize_streaming);
        assert!(config.streaming_controls_enabled());
        assert_eq!(config.streaming_backlog_limit(), 256);
        assert_eq!(config.memory_budget_bytes(), Some(8_192 * 1024 * 1024));
        // Legacy fields keep their meaning; the runtime translates unlimited
        // values into finite controller ceilings only for adaptive mode.
        assert_eq!(config.max_scene_loads, 0);
        assert_eq!(config.max_model_spawns_per_frame, 32);
    }

    #[test]
    fn explicit_pipeline_limits_override_adaptive_fallbacks() {
        let config = run_config(&[
            "--adaptive-streaming",
            "--max-streaming-backlog",
            "48",
            "--streaming-memory-mib",
            "4096",
            "--streaming-headroom-mib",
            "1024",
            "--streaming-frame-ms",
            "33.33",
            "--streaming-costs",
            "metadata/custom-costs.json",
        ]);
        assert_eq!(config.streaming_backlog_limit(), 48);
        assert_eq!(config.memory_budget_bytes(), Some(4_096 * 1024 * 1024));
        assert_eq!(config.streaming_headroom_bytes(), 1_024 * 1024 * 1024);
        assert_eq!(config.streaming_frame_ms, 33.33);
        assert_eq!(
            config.streaming_costs,
            Some(PathBuf::from("metadata/custom-costs.json"))
        );
    }

    #[test]
    fn backlog_and_memory_admission_can_be_enabled_without_adaptation() {
        for args in [
            vec!["--max-streaming-backlog", "64"],
            vec!["--streaming-memory-mib", "1024"],
        ] {
            let config = run_config(&args);
            assert!(config.streaming_controls_enabled());
            assert!(!config.adaptive_streaming);
            assert!(!config.prioritize_streaming);
        }
        for args in [
            vec!["--max-streaming-backlog", "0"],
            vec!["--streaming-memory-mib", "0"],
            vec!["--streaming-costs", "costs.json"],
            vec!["--streaming-frame-ms", "8.33"],
            vec!["--streaming-headroom-mib", "0"],
        ] {
            assert!(!run_config(&args).streaming_controls_enabled());
        }
    }

    #[test]
    fn zero_pipeline_limits_select_automatic_limits_only_in_adaptive_mode() {
        let config = run_config(&[
            "--adaptive-streaming",
            "--streaming-memory-mib",
            "0",
            "--max-streaming-backlog",
            "0",
            "--streaming-headroom-mib",
            "0",
        ]);
        assert_eq!(config.memory_budget_bytes(), Some(8_192 * 1024 * 1024));
        assert_eq!(config.streaming_backlog_limit(), 256);
        assert_eq!(config.streaming_headroom_bytes(), 0);
    }

    #[test]
    fn pipeline_mib_values_must_fit_bytes_and_negative_values_are_rejected() {
        let overflow = (usize::MAX / (1024 * 1024) + 1).to_string();
        let maximum = (usize::MAX / (1024 * 1024)).to_string();
        for option in ["--streaming-memory-mib", "--streaming-headroom-mib"] {
            for invalid in ["-1", "NaN", "inf", overflow.as_str()] {
                assert!(matches!(
                    parse_error(&[option, invalid]),
                    ConfigError::InvalidValue { .. }
                ));
            }
            let config = run_config(&[option, &maximum]);
            if option == "--streaming-memory-mib" {
                assert_eq!(
                    config.memory_budget_bytes(),
                    Some((usize::MAX / (1024 * 1024) * (1024 * 1024)) as u64)
                );
            } else {
                assert_eq!(
                    config.streaming_headroom_bytes(),
                    (usize::MAX / (1024 * 1024) * (1024 * 1024)) as u64
                );
            }
        }
        assert!(matches!(
            parse_error(&["--max-streaming-backlog", "-1"]),
            ConfigError::InvalidValue { .. }
        ));
    }

    #[test]
    fn pipeline_frame_target_is_positive_finite_and_independent_of_acceptance() {
        for value in ["0", "-1", "NaN", "inf", "-inf", "1e999"] {
            assert!(matches!(
                parse_error(&["--streaming-frame-ms", value]),
                ConfigError::InvalidValue { .. }
            ));
        }
        let config = run_config(&["--streaming-frame-ms", "33.33"]);
        assert_eq!(config.streaming_frame_ms, 33.33);
        assert_eq!(config.accept_p95_ms, 16.67);
        assert_eq!(config.accept_min_fps, 60.0);
    }

    #[test]
    fn pipeline_options_require_values_without_swallowing_following_flags() {
        for option in [
            "--max-streaming-backlog",
            "--streaming-memory-mib",
            "--streaming-costs",
            "--streaming-headroom-mib",
            "--streaming-frame-ms",
        ] {
            assert!(matches!(
                parse_error(&[option]),
                ConfigError::InvalidValue { value: None, .. }
            ));
            assert!(matches!(
                parse_error(&[option, "--headless"]),
                ConfigError::InvalidValue { value: Some(value), .. } if value == "--headless"
            ));
        }
    }

    #[test]
    fn help_shows_the_model_spawn_default_the_engine_uses() {
        let line = HELP_TEXT
            .lines()
            .find(|line| {
                line.trim_start()
                    .starts_with("--max-model-spawns-per-frame ")
            })
            .expect("--help has a --max-model-spawns-per-frame line");
        let default = EngineConfig::default().max_model_spawns_per_frame;
        assert!(line.ends_with(&format!("(default: {default})")), "{line}");
    }

    #[test]
    fn parses_live_start_camera_and_opt_in_stdin_console() {
        let config = run_config(&[
            "--start-shot",
            "camera.json",
            "--stdin-console",
            "--no-render-diagnostics",
        ]);
        assert_eq!(config.start_shot, Some(PathBuf::from("camera.json")));
        assert!(config.stdin_console);
        assert!(config.no_render_diagnostics);
        assert!(config.interactive_world_physics());
        assert!(!EngineConfig::default().stdin_console);
        assert!(!EngineConfig::default().no_render_diagnostics);
        assert!(
            parse_error(&["--start-shot"])
                .to_string()
                .contains("--start-shot")
        );
    }

    #[test]
    fn parses_visual_preset_without_changing_diagnostic_grade() {
        assert_eq!(
            EngineConfig::default().visual_preset,
            SceneVisualPreset::CoolGray
        );
        for (value, expected) in [
            ("vivid", SceneVisualPreset::Vivid),
            ("COOL-GRAY", SceneVisualPreset::CoolGray),
        ] {
            let config = run_config(&["--visual-preset", value, "--color-grade", "saturation"]);
            assert_eq!(config.visual_preset, expected);
            assert_eq!(config.color_grade, SceneGradeMode::Saturation);
        }
        for args in [
            &["--visual-preset"][..],
            &["--visual-preset", "invalid"][..],
        ] {
            assert!(parse_error(args).to_string().contains("--visual-preset"));
        }
    }

    #[test]
    fn parses_explicit_image_space_sources_and_rejects_ambiguous_selections() {
        let direct = run_config(&[
            "--color-grade",
            "record-srgb",
            "--image-space",
            "0x12F88",
            "--grade-preset",
            "look.json",
        ]);
        assert_eq!(
            direct.image_space_selection().unwrap(),
            Some(ImageSpaceSelection::Direct(0x12F88))
        );
        assert_eq!(direct.grade_preset, Some(PathBuf::from("look.json")));
        let weather = run_config(&["--image-space-weather", "2074", "--image-space-slot", "DAY"]);
        assert_eq!(
            weather.image_space_selection().unwrap(),
            Some(ImageSpaceSelection::Weather {
                form_id: 0x81A,
                slot: ImageSpaceSlot::Day
            })
        );
        for args in [
            &["--color-grade", "record-srgb"][..],
            &[
                "--image-space",
                "100",
                "--image-space-weather",
                "2074",
                "--image-space-slot",
                "day",
            ][..],
            &["--image-space-weather", "2074"][..],
            &["--image-space-slot", "day"][..],
        ] {
            assert!(run_config(args).image_space_selection().is_err());
        }
        for args in [
            &["--image-space", "0xZZ"][..],
            &["--image-space-slot", "noon"][..],
            &["--grade-preset"][..],
        ] {
            assert!(EngineConfig::from_args(args.iter().map(|v| (*v).into())).is_err());
        }
    }

    #[test]
    fn visual_profile_defaults_to_mudcrab_and_parses_both_profiles() {
        assert_eq!(
            EngineConfig::default().visual_profile,
            VisualProfile::Mudcrab
        );
        for (value, expected) in [
            ("mudcrab", VisualProfile::Mudcrab),
            ("skyrim", VisualProfile::Skyrim),
            ("Skyrim", VisualProfile::Skyrim),
        ] {
            assert_eq!(
                run_config(&["--visual-profile", value]).visual_profile,
                expected
            );
        }
        for args in [
            &["--visual-profile"][..],
            &["--visual-profile", "vanilla"][..],
        ] {
            assert!(parse_error(args).to_string().contains("--visual-profile"));
        }
    }

    #[test]
    fn skyrim_profile_refuses_each_mudcrab_only_option_by_name() {
        for (option, value) in [
            ("--tone-map", "tony"),
            ("--tone-map-params", "curve.json"),
            ("--color-grade", "full"),
            ("--grade-preset", "look.json"),
            ("--visual-preset", "vivid"),
        ] {
            let error = parse_error(&["--visual-profile", "skyrim", option, value]);
            assert!(
                matches!(error, ConfigError::ForeignToProfile { option: named, profile: "skyrim", .. } if named == option),
                "{option}: {error}"
            );
            let message = error.to_string();
            assert!(
                message.contains(option) && message.contains("skyrim"),
                "{message}"
            );
            // The same option stays valid under the profile it belongs to.
            if option != "--tone-map-params" {
                run_config(&["--visual-profile", "mudcrab", option, value]);
            }
        }
        run_config(&["--visual-profile", "skyrim", "--image-space", "0x1"]);
    }

    #[test]
    fn tone_map_selection_is_explicit_and_keeps_the_approved_default() {
        let default = EngineConfig::default();
        assert_eq!(default.tone_map, SceneToneMapMode::Tony);
        assert!(default.tone_map_params.is_none());
        assert_eq!(default.color_grade, SceneGradeMode::SnapshotSrgb);
        let experiment = run_config(&[
            "--tone-map",
            "retail-curve",
            "--tone-map-params",
            "curve.json",
            "--image-space",
            "0x12F87",
        ]);
        assert_eq!(experiment.tone_map, SceneToneMapMode::RetailCurve);
        assert_eq!(
            experiment.tone_map_params,
            Some(PathBuf::from("curve.json"))
        );
        assert!(experiment.image_space_selection().unwrap().is_some());
        assert!(
            run_config(&["--tone-map", "retail-curve"])
                .image_space_selection()
                .is_err()
        );
        assert!(EngineConfig::from_args(["--tone-map".into(), "native".into()]).is_err());
    }

    #[test]
    fn parses_color_grade_and_rejects_unknown_modes() {
        for (value, wanted) in [
            ("off", SceneGradeMode::Off),
            ("saturation", SceneGradeMode::Saturation),
            ("FULL", SceneGradeMode::Full),
            ("SNAPSHOT-SRGB", SceneGradeMode::SnapshotSrgb),
            ("snapshot-linear", SceneGradeMode::SnapshotLinear),
        ] {
            assert_eq!(run_config(&["--color-grade", value]).color_grade, wanted);
        }
        assert_eq!(
            EngineConfig::default().color_grade,
            SceneGradeMode::SnapshotSrgb
        );
        assert!(
            parse_error(&["--color-grade", "bad"])
                .to_string()
                .contains(SCENE_GRADE_VALUES)
        );
        assert!(
            parse_error(&["--color-grade"])
                .to_string()
                .contains("--color-grade")
        );
    }

    /// Hidden capture selects rendering without enabling interactive mouse ownership.
    #[test]
    fn parses_hidden_window_capture_mode() {
        assert!(!EngineConfig::default().hidden_window);
        let config = run_config(&["--hidden-window", "--acceptance-screenshot", "capture.png"]);
        assert!(config.hidden_window);
        assert!(!config.headless);
        assert!(!config.interactive_world_physics());
        assert_eq!(
            config.acceptance_screenshot,
            Some(PathBuf::from("capture.png"))
        );
    }

    #[test]
    fn parses_the_model_spawn_budget_and_lets_zero_mean_unlimited() {
        let config = run_config(&["--max-model-spawns-per-frame", "12"]);
        assert_eq!(config.max_model_spawns_per_frame, 12);

        let unlimited = run_config(&["--max-model-spawns-per-frame", "0"]);
        assert_eq!(unlimited.max_model_spawns_per_frame, 0);
    }

    #[test]
    fn parses_the_unload_budget_and_lets_zero_mean_unlimited() {
        assert_eq!(
            run_config(&["--max-unloads-per-frame", "5"]).max_cell_unloads_per_frame,
            5
        );
        assert_eq!(
            run_config(&["--max-unloads-per-frame", "0"]).max_cell_unloads_per_frame,
            0
        );
    }

    #[test]
    fn parses_the_io_thread_count_and_refuses_one_past_the_cap() {
        assert_eq!(run_config(&["--io-threads", "6"]).io_threads, 6);
        assert_eq!(run_config(&["--io-threads", "64"]).io_threads, 64);
        // 0 still asks for the automatic size.
        assert_eq!(run_config(&["--io-threads", "0"]).io_threads, 0);
        // Past the cap is refused like a malformed number.
        assert!(matches!(
            parse_error(&["--io-threads", "65"]),
            ConfigError::InvalidValue { value: Some(ref value), .. } if value == "65"
        ));
        assert_eq!(
            IO_THREADS_EXPECTED,
            format!("a thread count from 0 to {MAX_IO_THREADS}")
        );
        // `--help` spells the cap out too, so a bump must update it.
        assert!(
            HELP_TEXT.contains(&format!("asset IO threads, 0 to {MAX_IO_THREADS};")),
            "--help states a different --io-threads cap"
        );
        // A count left out is an error naming `--io-threads`; it does not swallow the option
        // after it (`--headless` would otherwise be lost and a window opened).
        assert!(matches!(
            parse_error(&["--io-threads", "--headless"]),
            ConfigError::InvalidValue { value: Some(ref value), .. } if value == "--headless"
        ));
    }

    #[test]
    fn defaults_to_a_sixteen_mib_upload_budget_per_frame() {
        assert_eq!(
            EngineConfig::default().max_upload_bytes_per_frame(),
            Some(16 * 1024 * 1024)
        );
    }

    #[test]
    fn parses_the_upload_budget_and_lets_zero_mean_unlimited() {
        let config = run_config(&["--max-upload-mib-per-frame", "4"]);
        assert_eq!(config.max_upload_mib_per_frame, 4);
        assert_eq!(config.max_upload_bytes_per_frame(), Some(4 * 1024 * 1024));

        let unlimited = run_config(&["--max-upload-mib-per-frame", "0"]);
        assert_eq!(unlimited.max_upload_mib_per_frame, 0);
        assert_eq!(unlimited.max_upload_bytes_per_frame(), None);
    }

    #[test]
    fn parses_the_benchmark_jump_target() {
        let config = EngineConfig::run_from_args(["--benchmark-jump", "12,-7"].map(str::to_owned));
        assert_eq!(config.benchmark_jump, Some((12, -7)));
        assert_eq!(EngineConfig::default().benchmark_jump, None);
        for bad in ["12", "12,", "a,b", "1,2,3", "1.5,2", ""] {
            let config = EngineConfig::run_from_args(["--benchmark-jump", bad].map(str::to_owned));
            assert_eq!(config.benchmark_jump, None, "{bad:?}");
        }
        // A missing value sets no target and does not swallow the next option.
        let config = EngineConfig::run_from_args(["--benchmark-jump"].map(str::to_owned));
        assert_eq!(config.benchmark_jump, None);
        let config =
            EngineConfig::run_from_args(["--benchmark-jump", "--headless"].map(str::to_owned));
        assert_eq!(config.benchmark_jump, None);
        assert!(config.headless);
    }

    #[test]
    fn a_jump_grid_coordinate_is_dropped_when_out_of_range() {
        let args =
            |list: &[&str]| EngineConfig::run_from_args(list.iter().map(|v| (*v).to_owned()));
        // The ends of the accepted range are kept.
        assert_eq!(
            args(&["--benchmark-jump", "512,-512"]).benchmark_jump,
            Some((512, -512))
        );
        assert_eq!(
            args(&["--benchmark-jump", "-512,512"]).benchmark_jump,
            Some((-512, 512))
        );
        // One past either end is dropped, including the extremes that would overflow the position
        // maths, and a flag after it is still read.
        for bad in [
            "513,0",
            "0,513",
            "-513,0",
            "0,-513",
            "2147483647,0",
            "0,-2147483648",
        ] {
            let config = args(&["--benchmark-jump", bad, "--benchmark-frames", "60"]);
            assert_eq!(config.benchmark_jump, None, "{bad:?}");
            assert_eq!(
                config.benchmark_frames,
                Some(60),
                "{bad:?} ate the next flag"
            );
        }
    }

    #[test]
    fn a_negative_budget_is_refused() {
        for option in [
            "--max-unloads-per-frame",
            "--max-model-spawns-per-frame",
            "--max-upload-mib-per-frame",
        ] {
            assert!(
                matches!(
                    parse_error(&[option, "-1"]),
                    ConfigError::InvalidValue { value: Some(ref value), .. } if value == "-1"
                ),
                "{option} accepted -1"
            );
        }
    }

    #[test]
    fn pacing_is_measured_only_for_a_benchmark_or_a_jump() {
        assert!(!EngineConfig::default().measures_pacing());
        assert!(
            EngineConfig {
                benchmark_frames: Some(60),
                ..EngineConfig::default()
            }
            .measures_pacing()
        );
        assert!(
            EngineConfig {
                benchmark_duration_secs: Some(30.0),
                ..EngineConfig::default()
            }
            .measures_pacing()
        );
        // A jump without a frame limit or duration still runs, and warns that it has no report.
        assert!(
            EngineConfig {
                benchmark_jump: Some((3, 4)),
                ..EngineConfig::default()
            }
            .measures_pacing()
        );
    }

    #[test]
    fn fly_lag_is_recorded_only_while_flying_in_a_measured_run() {
        let flying = EngineConfig {
            auto_fly_speed: 900.0,
            ..EngineConfig::default()
        };
        assert!(!flying.records_fly_lag(), "flying alone measures nothing");
        assert!(
            EngineConfig {
                benchmark_frames: Some(60),
                ..flying.clone()
            }
            .records_fly_lag()
        );
        assert!(
            !EngineConfig {
                benchmark_frames: Some(60),
                ..EngineConfig::default()
            }
            .records_fly_lag(),
            "a benchmark that does not fly has no fly lag"
        );
        let routed = EngineConfig {
            route: Some("travel.json".into()),
            benchmark_duration_secs: Some(60.0),
            ..EngineConfig::default()
        };
        assert!(routed.flies());
        assert!(routed.records_fly_lag(), "a route moves the camera too");
    }

    #[test]
    fn parses_the_route_and_the_frame_budget() {
        let config = EngineConfig::run_from_args(
            [
                "--route",
                "scripts/routes/travel.json",
                "--frame-budget-ms",
                "8.33",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        assert_eq!(
            config.route.as_deref(),
            Some(std::path::Path::new("scripts/routes/travel.json"))
        );
        assert_eq!(config.frame_budget_ms, 8.33);
        let defaults = EngineConfig::default();
        assert_eq!(defaults.route, None);
        assert_eq!(defaults.frame_budget_ms, 6.25);
        for bad in [
            ["--frame-budget-ms", "0"],
            ["--frame-budget-ms", "-3"],
            ["--frame-budget-ms", "fast"],
            ["--frame-budget-ms", "--headless"],
            ["--route", "--headless"],
        ] {
            assert!(
                EngineConfig::from_args(bad.map(str::to_owned)).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(EngineConfig::from_args(["--route".to_owned()]).is_err());
    }

    /// A route owns the camera as auto-fly does: it is never an interactive play session, and a
    /// jump it would undo is dropped.
    #[test]
    fn a_route_owns_the_camera() {
        assert!(EngineConfig::default().interactive_world_physics());
        let routed = EngineConfig::run_from_args(
            [
                "--route",
                "travel.json",
                "--benchmark-jump",
                "3,4",
                "--benchmark-duration",
                "30",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        assert!(!routed.interactive_world_physics());
        assert_eq!(routed.benchmark_jump, None, "the route would overwrite it");
        let without_report = EngineConfig {
            route: Some("travel.json".into()),
            ..EngineConfig::default()
        };
        assert!(!without_report.interactive_world_physics());
    }

    #[test]
    fn a_jump_is_ignored_when_the_run_drives_the_camera() {
        let args =
            |list: &[&str]| EngineConfig::run_from_args(list.iter().map(|v| (*v).to_owned()));
        // In either order of the flags.
        for list in [
            ["--benchmark-jump", "3,4", "--auto-fly-speed", "900"],
            ["--auto-fly-speed", "900", "--benchmark-jump", "3,4"],
        ] {
            let config = args(&list);
            assert_eq!(config.benchmark_jump, None, "{list:?}");
            assert_eq!(config.auto_fly_speed, 900.0);
        }
        let config = args(&[
            "--benchmark-jump",
            "3,4",
            "--acceptance-screenshot",
            "a.png",
        ]);
        assert_eq!(config.benchmark_jump, None);
        // The streaming fixture drives the camera every frame too.
        let config = args(&["--benchmark-jump", "3,4", "--streaming-fixture"]);
        assert_eq!(config.benchmark_jump, None);
        assert!(config.streaming_fixture);
        // A shots run poses the camera for each shot, and a jump would move it off a pose.
        let config = args(&[
            "--benchmark-jump",
            "3,4",
            "--benchmark-frames",
            "600",
            "--shots",
            "shots.json",
        ]);
        assert_eq!(config.benchmark_jump, None);
        assert!(config.shots.is_some());
        for list in [
            ["--benchmark-jump", "3,4", "--matched-route", "route.json"],
            ["--matched-route", "route.json", "--benchmark-jump", "3,4"],
        ] {
            let config = args(&list);
            assert_eq!(config.benchmark_jump, None, "{list:?}");
            assert_eq!(config.matched_route, Some(PathBuf::from("route.json")));
        }
        // A synthetic benchmark loads no world, so no streaming system would issue the jump.
        let config = args(&[
            "--benchmark-only",
            "--benchmark-frames",
            "120",
            "--benchmark-jump",
            "1,2",
        ]);
        assert_eq!(config.benchmark_jump, None);
        assert!(config.benchmark_only);
        // The single-scene fixtures create no streaming runtime either.
        for flag in [
            "--material-fixture",
            "--terrain-water-fixture",
            "--transform-bounds-fixture",
            "--renderer-fixture",
            "--physics-fixture",
        ] {
            let config = args(&["--benchmark-jump", "3,4", "--benchmark-frames", "600", flag]);
            assert_eq!(config.benchmark_jump, None, "{flag}");
        }
        assert_eq!(
            args(&["--benchmark-jump", "3,4"]).benchmark_jump,
            Some((3, 4))
        );
    }

    /// Without a frame limit or duration no report is written, so the jump has nowhere to appear:
    /// the run warns but keeps the target (an interactive run may still be worth watching).
    #[test]
    fn a_jump_without_a_report_keeps_the_target() {
        let config = EngineConfig::run_from_args(["--benchmark-jump", "3,4"].map(str::to_owned));
        assert_eq!(config.benchmark_jump, Some((3, 4)));
        assert!(!config.is_benchmark_run());
        // The jump is an automated camera path: no player whose WALK camera follow would undo it.
        assert!(!config.interactive_world_physics());
        let config = EngineConfig::run_from_args(
            ["--benchmark-jump", "3,4", "--benchmark-frames", "600"].map(str::to_owned),
        );
        assert_eq!(config.benchmark_jump, Some((3, 4)));
        assert!(config.is_benchmark_run());
    }

    #[test]
    fn parses_screenshot_camera_offset() {
        let config = run_config(&["--screenshot-camera-offset", "0,6000,12000"]);
        assert_eq!(
            config.screenshot_camera_offset,
            Some((0.0, 6000.0, 12000.0))
        );
        let config = run_config(&["--screenshot-camera-offset", "-10, 2.5 ,0"]);
        assert_eq!(config.screenshot_camera_offset, Some((-10.0, 2.5, 0.0)));
        assert_eq!(EngineConfig::default().screenshot_camera_offset, None);
    }

    #[test]
    fn parses_the_optional_sun_bearing_and_elevation() {
        let config = run_config(&["--sun-position", "450,35"]);
        assert_eq!(config.sun_position, Some((90.0, 35.0)));

        for value in ["0", "north,35", "0,91", "0,-1", "0,35,2", "NaN,35"] {
            assert!(
                matches!(
                    parse_error(&["--sun-position", value]),
                    ConfigError::InvalidValue {
                        option: "--sun-position",
                        ..
                    }
                ),
                "{value} was accepted"
            );
        }
    }

    #[test]
    fn a_malformed_screenshot_camera_offset_is_refused() {
        assert_eq!(
            parse_error(&["--screenshot-camera-offset", "0,6000"]).to_string(),
            "option '--screenshot-camera-offset' does not accept '0,6000': expected three finite \
             numbers \"x,y,z\". Run with --help to list every option."
        );
        for invalid in ["0,0,0,0", "a,b,c", "NaN,0,0"] {
            assert!(
                matches!(
                    parse_error(&["--screenshot-camera-offset", invalid]),
                    ConfigError::InvalidValue { .. }
                ),
                "{invalid} was accepted"
            );
        }
        assert_eq!(
            parse_error(&["--screenshot-camera-offset"]),
            ConfigError::InvalidValue {
                option: "--screenshot-camera-offset",
                value: None,
                expected: "three finite numbers \"x,y,z\"",
            }
        );
    }

    #[test]
    fn rejects_non_finite_screenshot_camera_offset_components() {
        for invalid in [
            "NaN,0,0",
            "0,NaN,0",
            "0,0,NaN",
            "inf,0,0",
            "0,-inf,0",
            "0,0,Infinity",
        ] {
            assert_eq!(parse_offset(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn an_automated_run_says_what_it_is_in_its_title() {
        let args = |list: &[&str]| run_config(list);
        assert_eq!(
            args(&["--benchmark-duration", "20", "--run-label", "main rural r1"]).window_title(),
            "Mudcrab - benchmark: main rural r1"
        );
        assert_eq!(
            args(&["--benchmark-frames", "600"]).window_title(),
            "Mudcrab - benchmark"
        );
        // A label left out does not swallow the next option.
        let config = args(&["--run-label", "--benchmark-frames", "600"]);
        assert_eq!(config.run_label, None);
        assert_eq!(config.benchmark_frames, Some(600));
        let config = args(&["--benchmark-frame-times", "--benchmark-frames", "600"]);
        assert_eq!(config.benchmark_frame_times, None);
        assert_eq!(config.benchmark_frames, Some(600));
        // Either may also end the command line with its value left out.
        assert_eq!(args(&["--run-label"]).run_label, None);
        assert_eq!(
            args(&["--benchmark-frame-times"]).benchmark_frame_times,
            None
        );
        assert_eq!(
            args(&["--streaming-fixture"]).window_title(),
            "Mudcrab - streaming fixture"
        );
        assert_eq!(
            args(&["--shots", "poses.json", "--run-label", "riverwood"]).window_title(),
            "Mudcrab - shots: riverwood"
        );
        assert_eq!(args(&[]).window_title(), "Mudcrab");
    }

    /// A file route cannot share the camera with the streaming benchmark route. Refused while the
    /// arguments are parsed, before any asset or device work starts.
    #[test]
    fn a_file_route_refuses_the_streaming_benchmark_route() {
        let error = parse_error(&[
            "--route",
            "travel.json",
            "--streaming-benchmark-route-speed",
            "370",
            "--benchmark-duration",
            "30",
        ]);
        assert!(
            matches!(
                error,
                ConfigError::InvalidValue {
                    option: "--streaming-benchmark-route-speed",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn matched_route_is_a_separate_automated_camera_mode() {
        let config = run_config(&["--matched-route", "route.json"]);
        assert_eq!(config.matched_route, Some(PathBuf::from("route.json")));
        assert!(!config.interactive_world_physics());
        assert!(
            run_config(&["--matched-route", "route.json", "--run-label", "test"])
                .window_title()
                .contains("matched-route")
        );
    }

    #[test]
    fn shots_settle_timeout_is_finite_positive_and_bounded() {
        assert_eq!(run_config(&[]).shots_settle_timeout, 30.0);
        assert_eq!(
            run_config(&["--shots-settle-timeout", "120"]).shots_settle_timeout,
            120.0
        );
        for value in ["0", "-1", "NaN", "inf", "601"] {
            assert!(
                parse_error(&["--shots-settle-timeout", value])
                    .to_string()
                    .contains("--shots-settle-timeout")
            );
        }
    }

    /// A shots path left out does not swallow the next option, and it is an error rather than a
    /// silent fallback: continuing would run the engine interactively, a mode nobody asked for.
    #[test]
    fn a_valueless_shots_flag_is_an_error() {
        let error = parse_error(&["--shots", "--shots-out", "--lights"]).to_string();
        assert!(error.contains("--shots"), "{error}");
        let error = parse_error(&["--shots-out", "--lights"]).to_string();
        assert!(error.contains("--shots-out"), "{error}");
        let error = parse_error(&["--shots"]).to_string();
        assert!(error.contains("--shots"), "{error}");
    }

    #[test]
    fn parses_runtime_options() {
        let config = run_config(&[
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
            "--max-unloads-per-frame",
            "3",
            "--max-model-spawns-per-frame",
            "6",
            "--max-upload-mib-per-frame",
            "8",
            "--run-label",
            "stress r2",
            "--benchmark-frame-times",
            "out/frames.csv",
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
            "--screenshot-camera-offset",
            "0,6000,12000",
            "--shots",
            "reference/riverwood_shots.json",
            "--shots-out",
            "evidence/riverwood",
            "--diagnostic-asset-fallbacks",
            "--material-fixture",
            "--terrain-water-fixture",
            "--terrain-water-fixture-msaa",
            "4",
            "--transform-bounds-fixture",
            "--renderer-fixture",
            "--streaming-fixture",
            "--lights",
            "--physics-fixture",
        ]);
        assert_eq!(config.assets_dir, PathBuf::from("converted"));
        assert_eq!(config.worldspace_id, 0x3c);
        assert_eq!(config.start_grid, (4, 0));
        assert_eq!((config.stream_radius, config.unload_radius), (5, 6));
        assert!(config.headless);
        assert_eq!(config.max_commit_micros_per_frame, 8_500);
        assert_eq!(config.max_cell_unloads_per_frame, 3);
        assert_eq!(config.max_model_spawns_per_frame, 6);
        assert_eq!(config.max_upload_mib_per_frame, 8);
        assert_eq!(config.run_label.as_deref(), Some("stress r2"));
        assert_eq!(
            config.benchmark_frame_times,
            Some(PathBuf::from("out/frames.csv"))
        );
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
        assert_eq!(
            config.screenshot_camera_offset,
            Some((0.0, 6000.0, 12000.0))
        );
        assert_eq!(
            config.shots,
            Some(PathBuf::from("reference/riverwood_shots.json"))
        );
        assert_eq!(config.shots_out, Some(PathBuf::from("evidence/riverwood")));
        assert!(config.diagnostic_asset_fallbacks);
        assert!(config.material_fixture);
        assert!(config.terrain_water_fixture);
        assert_eq!(config.terrain_water_fixture_msaa, Msaa::Sample4);
        assert!(config.transform_bounds_fixture);
        assert!(config.renderer_fixture);
        assert!(config.streaming_fixture);
        assert!(config.lights);
        assert!(config.physics_fixture);
    }

    #[test]
    fn terrain_water_fixture_msaa_defaults_off_and_needs_the_fixture() {
        assert_eq!(run_config(&[]).terrain_water_fixture_msaa, Msaa::Off);
        for (value, expected) in [
            ("1", Msaa::Off),
            ("2", Msaa::Sample2),
            ("4", Msaa::Sample4),
            ("8", Msaa::Sample8),
        ] {
            assert_eq!(
                run_config(&[
                    "--terrain-water-fixture",
                    "--terrain-water-fixture-msaa",
                    value
                ])
                .terrain_water_fixture_msaa,
                expected,
                "{value}"
            );
        }
        assert_eq!(
            parse_error(&["--terrain-water-fixture-msaa", "4"]),
            ConfigError::MissingRequirement {
                option: "--terrain-water-fixture-msaa",
                requires: "--terrain-water-fixture",
            }
        );
        assert!(matches!(
            parse_error(&[
                "--terrain-water-fixture",
                "--terrain-water-fixture-msaa",
                "3"
            ]),
            ConfigError::InvalidValue { .. }
        ));
    }

    /// `--ini` files layer in order and every other option overrides them,
    /// wherever it appears on the command line.
    #[test]
    fn ini_files_apply_in_order_and_command_line_options_override_them() {
        let directory = tempfile::tempdir().unwrap();
        let skyrim = directory.path().join("Skyrim.ini");
        let prefs = directory.path().join("SkyrimPrefs.ini");
        std::fs::write(&skyrim, "[General]\nuGridsToLoad=7\n").unwrap();
        std::fs::write(
            &prefs,
            "[General]\nuGridsToLoad=9\n[TerrainManager]\nfSplitDistanceMult=1.5\n",
        )
        .unwrap();
        let path = |path: &std::path::Path| path.display().to_string();
        let layered = run_config(&["--ini", &path(&skyrim), "--ini", &path(&prefs)]);
        assert_eq!((layered.stream_radius, layered.unload_radius), (4, 5));
        assert_eq!(layered.terrain_lod.split_distance_mult, 1.5);

        let overridden = run_config(&["--stream-radius", "1", "--ini", &path(&prefs)]);
        assert_eq!((overridden.stream_radius, overridden.unload_radius), (1, 2));
        assert_eq!(overridden.terrain_lod.split_distance_mult, 1.5);

        let missing = run_config(&[
            "--ini",
            &path(&directory.path().join("absent.ini")),
            "--headless",
        ]);
        assert!(missing.headless, "a missing file is reported and skipped");
        assert_eq!(missing.terrain_lod, TerrainLodDistances::default());
    }

    /// Lights are opt-in: the flag is off unless it is given, so the default run - and every
    /// acceptance or benchmark baseline taken from one - is unchanged.
    #[test]
    fn lights_are_off_until_the_flag_is_given() {
        assert!(!EngineConfig::default().lights);
        assert!(
            !run_config(&["--headless"]).lights,
            "another flag does not turn lights on"
        );
        assert!(run_config(&["--lights"]).lights);
    }

    #[test]
    fn interactive_physics_preserves_automated_camera_paths() {
        assert!(EngineConfig::default().interactive_world_physics());
        for config in [
            EngineConfig {
                headless: true,
                ..EngineConfig::default()
            },
            EngineConfig {
                benchmark_frames: Some(60),
                ..EngineConfig::default()
            },
            EngineConfig {
                auto_fly_speed: 900.0,
                ..EngineConfig::default()
            },
            EngineConfig {
                acceptance_screenshot: Some("shot.png".into()),
                ..EngineConfig::default()
            },
            EngineConfig {
                streaming_fixture: true,
                ..EngineConfig::default()
            },
            EngineConfig {
                shots: Some("poses.json".into()),
                ..EngineConfig::default()
            },
            EngineConfig {
                benchmark_jump: Some((3, 4)),
                ..EngineConfig::default()
            },
            EngineConfig {
                physics_fixture: true,
                ..EngineConfig::default()
            },
        ] {
            assert!(!config.interactive_world_physics());
        }
    }

    #[test]
    fn help_is_requested_by_either_spelling() {
        for spelling in ["--help", "-h"] {
            let action = EngineConfig::from_args([spelling.to_owned()])
                .expect("the help flags are accepted options");
            assert!(
                matches!(action, ConfigAction::Help),
                "{spelling} did not ask for help"
            );
        }
    }

    #[test]
    fn help_wins_over_the_rest_of_the_command_line() {
        for arguments in [
            vec!["--assets", "converted", "--help"],
            // Even an unknown option before it must not fail the run.
            vec!["--typo", "--help"],
            vec!["--typo", "-h", "riverwood"],
        ] {
            match EngineConfig::from_args(arguments.iter().map(|argument| (*argument).to_owned())) {
                Ok(ConfigAction::Help) => {}
                other => panic!("{arguments:?} did not ask for help: {other:?}"),
            }
        }
    }

    #[test]
    fn a_bare_end_of_options_marker_is_ignored() {
        // `main` ignored it, and the parser has no positional arguments for it
        // to end.
        let config = run_config(&["--", "--headless", "--", "--grid-x", "3", "--"]);
        assert!(config.headless);
        assert_eq!(config.start_grid.0, 3);
    }

    #[test]
    fn the_option_scan_ignores_a_trailing_separator_and_dashes_inside_words() {
        // The script scan reads whole files, which may end with a bare `--`.
        assert_eq!(option_tokens("--"), Vec::<&str>::new());
        assert_eq!(option_tokens("engine --assets dir --"), vec!["--assets"]);
        // `--` inside a word or a number is not an option.
        assert!(option_tokens("word--word and 0--10").is_empty());
        // A token after a separator is still found.
        assert_eq!(option_tokens("run --  --grid-x 1"), vec!["--grid-x"]);
        // So is a quoted one, which is how the PowerShell scripts spell them.
        assert_eq!(option_tokens("\"--grid-y\""), vec!["--grid-y"]);
        // And one after shell punctuation, as in a bash array or assignment.
        assert_eq!(
            option_tokens("args+=(--assets \"$d\"); x=--headless"),
            vec!["--assets", "--headless"]
        );
    }

    #[test]
    fn an_unknown_option_is_refused_and_the_nearest_option_is_named() {
        assert_eq!(
            parse_error(&["--grid-z", "3"]).to_string(),
            "unknown option '--grid-z'; did you mean '--grid-x'? \
             Run with --help to list every option."
        );
    }

    #[test]
    fn an_unknown_option_with_no_close_match_is_still_named() {
        assert_eq!(
            parse_error(&["--frobnicate"]).to_string(),
            "unknown option '--frobnicate'. Run with --help to list every option."
        );
    }

    #[test]
    fn a_positional_argument_is_refused() {
        assert_eq!(
            parse_error(&["riverwood"]).to_string(),
            "unexpected argument 'riverwood'. Run with --help to list every option."
        );
    }

    #[test]
    fn ini_v88_refuses_missing_or_option_shaped_paths() {
        for args in [&["--ini"][..], &["--ini", "--headless"]] {
            assert!(matches!(
                parse_error(args),
                ConfigError::InvalidValue {
                    option: "--ini",
                    ..
                }
            ));
        }
        assert!(matches!(
            EngineConfig::from_args(["--ini".into(), "--help".into()]),
            Ok(ConfigAction::Help)
        ));
    }

    #[test]
    fn bug_report_directory_override_preserves_relative_absolute_and_spaced_paths() {
        assert!(EngineConfig::default().bug_report_dir.is_none());
        for path in ["reports", "/tmp/Mudcrab Reports", "-reports", r"C:\Reports"] {
            assert_eq!(
                run_config(&["--bug-report-dir", path]).bug_report_dir,
                Some(PathBuf::from(path))
            );
        }
        assert_eq!(
            run_config(&["--bug-report-dir", "old", "--bug-report-dir", "new"]).bug_report_dir,
            Some(PathBuf::from("new"))
        );
    }

    #[test]
    fn bug_report_directory_override_refuses_missing_empty_and_option_shaped_values() {
        for args in [
            &["--bug-report-dir"][..],
            &["--bug-report-dir", ""],
            &["--bug-report-dir", "  "],
            &["--bug-report-dir", "--headless"],
        ] {
            assert!(matches!(
                parse_error(args),
                ConfigError::InvalidValue {
                    option: "--bug-report-dir",
                    ..
                }
            ));
        }
    }

    #[test]
    fn a_missing_value_is_refused() {
        assert_eq!(
            parse_error(&["--grid-x"]).to_string(),
            "option '--grid-x' needs a value: expected a cell coordinate. \
             Run with --help to list every option."
        );
    }

    #[test]
    fn an_option_where_a_value_belongs_is_refused() {
        // Without the check the second option would become the first one's
        // value, and neither option would do anything.
        assert_eq!(
            parse_error(&["--profile-hardware", "--wroldspace"]).to_string(),
            "option '--profile-hardware' needs a value: expected a description of the hardware, \
             but '--wroldspace' is another option. Run with --help to list every option."
        );
    }

    #[test]
    fn a_value_that_does_not_parse_is_refused() {
        assert_eq!(
            parse_error(&["--grid-x", "abc"]).to_string(),
            "option '--grid-x' does not accept 'abc': expected a cell coordinate. \
             Run with --help to list every option."
        );
        assert_eq!(
            parse_error(&["--stream-radius", "wide"]).to_string(),
            "option '--stream-radius' does not accept 'wide': expected a cell count from 0 to \
             1073741822. Run with --help to list every option."
        );
    }

    #[test]
    fn a_value_outside_the_range_the_option_allows_is_refused() {
        assert_eq!(
            parse_error(&["--max-commit-ms", "0"]).to_string(),
            "option '--max-commit-ms' does not accept '0': expected a positive number of \
             milliseconds. Run with --help to list every option."
        );
        assert_eq!(
            parse_error(&["--worldspace", "0xzz"]).to_string(),
            "option '--worldspace' does not accept '0xzz': expected a worldspace form id, \
             decimal or 0x-hex. Run with --help to list every option."
        );
    }

    #[test]
    fn the_stream_radius_bound_is_spelled_out_in_its_message() {
        assert_eq!(
            STREAM_RADIUS_EXPECTED,
            format!("a cell count from 0 to {MAX_STREAM_RADIUS}")
        );
    }

    #[test]
    fn the_stream_radius_is_bounded_so_the_unload_ring_cannot_overflow() {
        let widest = run_config(&["--stream-radius", "1073741822"]);
        assert_eq!(widest.stream_radius, MAX_STREAM_RADIUS);
        assert_eq!(widest.unload_radius, MAX_STREAM_RADIUS + 1);
        assert!(
            widest
                .unload_radius
                .checked_mul(2)
                .and_then(|across| across.checked_add(1))
                .is_some()
        );
        assert_eq!(run_config(&["--stream-radius", "0"]).unload_radius, 1);

        assert_eq!(
            parse_error(&["--stream-radius", "2147483647"]).to_string(),
            "option '--stream-radius' does not accept '2147483647': expected a cell count from 0 \
             to 1073741822. Run with --help to list every option."
        );
        for refused in ["1073741823", "-1", "-4", "99999999999"] {
            assert!(
                matches!(
                    parse_error(&["--stream-radius", refused]),
                    ConfigError::InvalidValue { value: Some(ref value), .. } if value == refused
                ),
                "--stream-radius accepted {refused}"
            );
        }
    }

    #[test]
    fn numeric_options_refuse_nan_and_infinity() {
        for option in [
            "--max-commit-ms",
            "--benchmark-duration",
            "--accept-min-fps",
            "--accept-p95-ms",
            "--accept-max-memory-growth-gib",
            "--auto-fly-speed",
        ] {
            // `1e39` is finite as an f64 but overflows the f32 `--auto-fly-speed` feeds.
            let too_large = if option == "--auto-fly-speed" {
                "1e39"
            } else {
                "1e309"
            };
            for refused in ["NaN", "nan", "inf", "-inf", "infinity", too_large] {
                assert!(
                    matches!(
                        parse_error(&[option, refused]),
                        ConfigError::InvalidValue { value: Some(ref value), .. } if value == refused
                    ),
                    "{option} accepted {refused}"
                );
            }
        }
    }

    #[test]
    fn numeric_options_refuse_values_below_their_range() {
        for (option, refused) in [
            ("--max-commit-ms", "0"),
            ("--benchmark-duration", "0"),
            ("--benchmark-duration", "-5"),
            ("--accept-min-fps", "-1"),
            ("--accept-p95-ms", "-0.5"),
            ("--accept-max-memory-growth-gib", "-1"),
            ("--auto-fly-speed", "-500"),
        ] {
            assert!(
                matches!(
                    parse_error(&[option, refused]),
                    ConfigError::InvalidValue { value: Some(ref value), .. } if value == refused
                ),
                "{option} accepted {refused}"
            );
        }
        assert_eq!(
            parse_error(&["--benchmark-duration", "0"]).to_string(),
            "option '--benchmark-duration' does not accept '0': expected a positive number of \
             seconds. Run with --help to list every option."
        );
    }

    #[test]
    fn numeric_options_keep_zero_where_it_is_valid() {
        let config = run_config(&[
            "--accept-min-fps",
            "0",
            "--accept-p95-ms",
            "0",
            "--accept-max-memory-growth-gib",
            "0",
            "--auto-fly-speed",
            "0",
            "--benchmark-duration",
            "0.5",
        ]);
        assert_eq!(config.accept_min_fps, 0.0);
        assert_eq!(config.accept_p95_ms, 0.0);
        assert_eq!(config.accept_max_memory_growth_gib, 0.0);
        assert_eq!(config.auto_fly_speed, 0.0);
        assert_eq!(config.benchmark_duration_secs, Some(0.5));
    }

    #[test]
    fn a_single_dash_still_starts_a_value() {
        // Paths and negative numbers are values; only `--` marks an option.
        let config = run_config(&["--grid-y", "-12", "--profile-output", "-design/run-1"]);
        assert_eq!(config.start_grid.1, -12);
        assert_eq!(
            config.profile_output_dir,
            Some(PathBuf::from("-design/run-1"))
        );
    }

    #[test]
    fn the_arm_scan_still_finds_the_parser_arms() {
        // A floor, not an inventory: it fails if a reformat or a move leaves the
        // scan looking at nothing, which would make the drift tests below pass
        // without checking anything.
        let options = parser_options();
        assert!(
            options.len() >= 40,
            "only {} parser arms were found in config.rs",
            options.len()
        );
        for option in [
            "--assets",
            "--help",
            "-h",
            "--max-unloads-per-frame",
            "--max-model-spawns-per-frame",
            "--max-upload-mib-per-frame",
            "--run-label",
            "--benchmark-frame-times",
            "--screenshot-camera-offset",
            "--lights",
            "--physics-fixture",
        ] {
            assert!(options.contains(&option), "the scan missed {option}");
        }
    }

    #[test]
    fn help_lists_every_option_the_parser_accepts() {
        for option in parser_options() {
            assert!(help_names(option), "--help never names {option}");
        }
    }

    /// The option name on its own: a new arm called `--grid` must not be
    /// satisfied by the `--grid-x` and `--grid-y` lines.
    #[test]
    fn help_names_an_option_only_as_a_whole_option() {
        assert!(help_names("--grid-x"));
        assert!(help_names("--assets"));
        assert!(help_names("-h"));
        assert!(!help_names("--grid"));
        assert!(!help_names("--profile"));
        assert!(!help_names("--benchmark"));
        assert!(!help_names("--stream"));
    }

    /// True when the help text spells `option` as an option of its own, and not
    /// as the prefix of a longer one.
    fn help_names(option: &str) -> bool {
        HELP_TEXT.match_indices(option).any(|(start, _)| {
            let before = &HELP_TEXT[..start];
            // The comma is the separator in the `-h, --help` row.
            let after = &HELP_TEXT[start + option.len()..];
            (before.is_empty() || before.ends_with(char::is_whitespace))
                && (after.is_empty() || after.starts_with(|c: char| c.is_whitespace() || c == ','))
        })
    }

    /// The token scan sees `--` options only; the short `-h` is covered by
    /// `help_lists_every_option_the_parser_accepts`.
    #[test]
    fn help_names_no_option_the_parser_rejects() {
        let options = parser_options();
        for option in option_tokens(HELP_TEXT) {
            assert!(
                options.contains(&option),
                "--help names {option}, which the parser refuses"
            );
        }
    }

    /// The workspace `scripts/` directory, or `None` when this crate is built
    /// outside the repository: a packaged or vendored source tree has no
    /// scripts to read, and these scans are about this repository.
    fn scripts_directory() -> Option<std::path::PathBuf> {
        let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
        scripts.is_dir().then_some(scripts)
    }

    /// Utility `.ps1`, `.sh` and `.py` files under `root`, subdirectories included,
    /// in a stable order. Hidden directories and script tests are excluded.
    /// Converter-only profiling tools stay in the scan; their flags are checked
    /// only on lines that invoke the engine (see `checked_script_engine_flags`).
    fn script_paths(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            let entries = std::fs::read_dir(&directory)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
            for entry in entries {
                let entry = entry.expect("reading a scripts entry");
                let path = entry.path();
                // `file_type` does not follow links, so a linked folder cannot
                // loop the walk; hidden folders (`.venv`) hold no project scripts.
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    let name = entry.file_name();
                    // Tests inspect deliberately unsupported engine options.
                    if !name.to_string_lossy().starts_with('.') && name != "tests" {
                        pending.push(path);
                    }
                } else if matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("ps1" | "sh" | "py")
                ) {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths
    }

    fn checked_script_engine_flags(script: &str, path: &std::path::Path) -> usize {
        let options = parser_options();
        let mut checked = 0;
        // lod-performance tools parse converter options and lod-experiment
        // tools parse their own offline options. Only their lines that name the
        // engine are engine invocations; the rest is not checked.
        let converter_tool = path.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("lod-performance" | "lod-experiment")
            )
        });
        let engine_lines: String;
        let scanned = if converter_tool {
            engine_lines = script
                .lines()
                .filter(|line| {
                    line.split(|c: char| !c.is_alphanumeric())
                        .any(|word| word == "engine")
                })
                .collect::<Vec<_>>()
                .join("\n");
            engine_lines.as_str()
        } else {
            script
        };
        for flag in script_cli_options(scanned) {
            // This database-only tool owns --verify. Keep it scoped to the tool:
            // an engine launcher passing --verify must still fail the audit.
            let observation_option = path.file_name().is_some_and(|name| {
                name == "reference-enable-observations.py" && flag == "--verify"
            });
            // This standalone example owns these arguments; do not admit them for engine.
            let inspector_option = path.ends_with("scripts/model_inspector/runtime.py")
                && [
                    "--model",
                    "--record",
                    "--catalog",
                    "--build-only",
                    "--needed-gib",
                    "--isolated",
                    "--inside",
                    "--run-dir",
                    "--profile",
                    "--example",
                    "--message-format",
                    "--path-format",
                    "--git-common-dir",
                    "--export-",
                    "--export-frames",
                    "--export-gif",
                    "--export-video",
                    "--seconds",
                    "--fps",
                    "--size",
                    "--orbit",
                    "--background",
                    "--video-format",
                ]
                .contains(&flag);
            let release_option = path.file_name().is_some_and(|name| {
                SCRIPT_SCOPED_FLAGS
                    .iter()
                    .any(|&(script, scoped)| name == script && flag == scoped)
            });
            if NON_ENGINE_FLAGS.contains(&flag)
                || observation_option
                || release_option
                || inspector_option
            {
                continue;
            }
            assert!(
                options.contains(&flag),
                "{} passes {flag}, which the parser refuses",
                path.display()
            );
            checked += 1;
        }
        checked
    }

    #[test]
    fn the_script_audit_scopes_observation_options_and_rejects_unknown_engine_flags() {
        let observation = std::path::Path::new("reference-enable-observations.py");
        assert_eq!(
            checked_script_engine_flags(
                "--output manifest.json --verify manifest.json",
                observation
            ),
            0
        );
        assert_eq!(
            checked_script_engine_flags("engine --headless", observation),
            1
        );
        assert!(
            std::panic::catch_unwind(|| {
                checked_script_engine_flags("engine --unknown-engine-option", observation)
            })
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| {
                checked_script_engine_flags(
                    "engine --verify manifest.json",
                    std::path::Path::new("launch.sh"),
                )
            })
            .is_err()
        );
        for (script, flag) in SCRIPT_SCOPED_FLAGS {
            let line = format!("{flag} value");
            assert_eq!(
                checked_script_engine_flags(&line, std::path::Path::new(script)),
                0,
                "{flag} must be exempt in {script}"
            );
            assert!(
                std::panic::catch_unwind(|| {
                    checked_script_engine_flags(&line, std::path::Path::new("launch.sh"))
                })
                .is_err(),
                "{flag} must still be rejected in other scripts"
            );
        }
    }

    /// Standalone inspector flags must not become general engine CLI options.
    #[test]
    fn inspector_options_are_scoped_to_the_standalone_example_launcher() {
        let tool = std::path::Path::new("scripts/model_inspector/runtime.py");
        assert_eq!(
            checked_script_engine_flags("--record 1 --model meshes/a.glb --build-only", tool),
            0
        );
        for flag in [
            "--export-",
            "--export-frames",
            "--export-gif",
            "--export-video",
            "--seconds",
            "--fps",
            "--size",
            "--orbit",
            "--background",
            "--video-format",
        ] {
            assert_eq!(checked_script_engine_flags(flag, tool), 0, "{flag}");
            assert!(
                std::panic::catch_unwind(|| {
                    checked_script_engine_flags(flag, std::path::Path::new("scripts/other.py"))
                })
                .is_err(),
                "{flag} was accepted for an ordinary engine launcher"
            );
        }
        assert!(
            std::panic::catch_unwind(|| checked_script_engine_flags(
                "--record 1",
                std::path::Path::new("scripts/other.py")
            ))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| checked_script_engine_flags(
                "--unknown-inspector-option",
                tool
            ))
            .is_err()
        );
    }

    /// Every utility `.ps1`, `.sh` and `.py` file in `scripts/`, subdirectories
    /// included, is read from disk, so a script added later cannot pass an
    /// option the parser refuses without failing this test. The scan is skipped
    /// when the repository's `scripts/` is not next to this crate.
    #[test]
    fn the_scripts_only_pass_options_the_parser_accepts() {
        let Some(scripts) = scripts_directory() else {
            return;
        };
        let mut checked = 0;
        for path in script_paths(&scripts) {
            let script = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            checked += checked_script_engine_flags(&script, &path);
        }
        assert!(
            checked >= 20,
            "the script scan found {checked} engine flags; the scripts moved or changed shape"
        );
    }

    fn script_cli_options(script: &str) -> Vec<&str> {
        option_tokens(script)
            .into_iter()
            .filter(|flag| {
                // HTML-producing utilities use CSS custom properties. Keep an
                // identically named CLI argument visible to the parser check.
                let offset = flag.as_ptr() as usize - script.as_ptr() as usize;
                let prefix = &script[..offset];
                !(prefix.trim_end().ends_with("var(")
                    || prefix.ends_with(".setProperty('")
                    || prefix.ends_with(".setProperty(\""))
            })
            .collect()
    }

    #[test]
    fn script_scan_ignores_css_references_but_keeps_identically_named_cli_arguments() {
        let script = "color:var(--foreground); x.style.setProperty('--aspect','16/9'); args=['--foreground','--assets','pack'];";
        assert_eq!(script_cli_options(script), ["--foreground", "--assets"]);
    }

    #[test]
    fn checked_script_audit_ignores_css_but_rejects_the_same_unknown_cli_option() {
        let path = std::path::Path::new("capture_tonemap_tuning.py");
        let script = "color:var(--foreground); x.style.setProperty('--aspect','16/9'); engine --headless --assets pack";
        assert_eq!(checked_script_engine_flags(script, path), 2);

        assert!(
            std::panic::catch_unwind(|| {
                checked_script_engine_flags("color:var(--foreground); engine --foreground", path)
            })
            .is_err()
        );
    }

    /// The numbers a script writes after an engine option still parse. Values a
    /// script computes (`$StreamRadius`, `$(...)`, a bare sh variable name),
    /// paths and names are not read, and a Python audit script's `add_argument`
    /// declarations are not a command line, so only numbers in the shell and
    /// PowerShell scripts are checked: best effort where the flag scan above is
    /// exact.
    #[test]
    fn the_scripts_literal_values_still_parse() {
        let options = parser_options();
        let Some(scripts) = scripts_directory() else {
            return;
        };
        let mut checked = 0;
        for path in script_paths(&scripts) {
            if !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("ps1" | "sh")
            ) {
                continue;
            }
            let script = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            // An option and its value are paired only within one line, and
            // comment lines (usage notes such as `--max-commit-ms <ms>`) are
            // skipped.
            for line in script
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
            {
                let tokens = script_tokens(line);
                for pair in tokens.windows(2) {
                    let (option, value) = (pair[0].as_str(), pair[1].as_str());
                    if !options.contains(&option) || !is_number_literal(value) {
                        continue;
                    }
                    match EngineConfig::from_args([option.to_owned(), value.to_owned()]) {
                        Ok(_) => checked += 1,
                        // A flag that takes no value is followed by the next word.
                        Err(ConfigError::UnexpectedArgument { .. }) => {}
                        Err(error) => panic!(
                            "{} passes {option} {value}, which the parser refuses: {error}",
                            path.display()
                        ),
                    }
                }
            }
        }
        assert!(
            checked >= 8,
            "the value scan found {checked} literal values; the scripts moved or changed shape"
        );
    }

    /// The words of one script line, split at whitespace, quotes and
    /// punctuation: a rough split, not a shell's, but enough to find a number
    /// written after an option.
    fn script_tokens(script: &str) -> Vec<String> {
        script
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '(' | ')' | ';' | '=' | '[' | ']' | '`'
                    )
            })
            // Commas at the edges delimit PowerShell lists. Internal commas belong
            // to a CSV value such as a camera offset and must stay together.
            .map(|token| token.trim_matches(','))
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn script_v89_token_scan_preserves_csv_values_and_list_delimiters() {
        assert_eq!(
            script_tokens(r#"--screenshot-camera-offset "0,6000,8000""#),
            ["--screenshot-camera-offset", "0,6000,8000"]
        );
        assert_eq!(script_tokens("('--grid-x',5)"), ["--grid-x", "5"]);
    }

    #[test]
    fn script_v89_scan_excludes_other_tool_scopes_and_keeps_nested_utilities() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("tests")).unwrap();
        std::fs::create_dir(directory.path().join("lod-performance")).unwrap();
        std::fs::create_dir(directory.path().join("utils")).unwrap();
        std::fs::write(directory.path().join("tests/check.py"), "--log-file").unwrap();
        std::fs::write(
            directory.path().join("lod-performance/audit.py"),
            "--data-root --expected-chunks --lod-encoder",
        )
        .unwrap();
        let utility = directory.path().join("utils/capture.sh");
        std::fs::write(&utility, "--headless").unwrap();
        assert_eq!(
            script_paths(directory.path()),
            vec![directory.path().join("lod-performance/audit.py"), utility]
        );
    }

    #[test]
    fn lod_performance_flags_are_checked_only_on_engine_lines() {
        let tool = std::path::Path::new("scripts/lod-performance/run_conversion_observed.py");
        assert_eq!(
            checked_script_engine_flags("converter --data-root x --lod-encoder cpu", tool),
            0
        );
        assert_eq!(
            checked_script_engine_flags("engine --headless\nconverter --data-root x", tool),
            1
        );
        assert!(
            std::panic::catch_unwind(|| {
                checked_script_engine_flags("engine --unknown-engine-option", tool)
            })
            .is_err()
        );
    }

    #[test]
    fn lod_experiment_flags_are_checked_only_on_engine_lines() {
        let tool = std::path::Path::new("scripts/lod-experiment/build.py");
        assert_eq!(
            checked_script_engine_flags(
                "builder --meshoptimizer x --max-cluster-triangles 124 --output out",
                tool
            ),
            0
        );
        assert_eq!(
            checked_script_engine_flags("engine --headless\nbuilder --meshoptimizer x", tool),
            1
        );
        assert!(
            std::panic::catch_unwind(|| {
                checked_script_engine_flags("engine --unknown-engine-option", tool)
            })
            .is_err()
        );
    }

    #[test]
    fn unregistered_lod_experiment_utilities_stay_in_the_scan() {
        let directory = tempfile::tempdir().unwrap();
        let experiment = directory.path().join("lod-experiment");
        std::fs::create_dir(&experiment).unwrap();
        let registered = experiment.join("build.py");
        let utility = experiment.join("unregistered.py");
        std::fs::write(&registered, "--meshoptimizer x").unwrap();
        std::fs::write(&utility, "--meshoptimizer x").unwrap();
        assert_eq!(
            script_paths(directory.path()),
            vec![registered.clone(), utility.clone()]
        );
        assert_eq!(
            checked_script_engine_flags(&std::fs::read_to_string(&utility).unwrap(), &utility),
            0
        );
    }

    /// True when a script spells out a finite number.
    fn is_number_literal(value: &str) -> bool {
        value.parse::<f64>().is_ok_and(f64::is_finite)
    }
}
