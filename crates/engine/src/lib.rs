pub mod actor_animation;
pub mod actor_body_weight;
pub mod actor_drive;
pub mod actor_head;
pub mod actor_skin_tone;
pub mod actors;
pub mod app;
pub mod bug_report;
mod capture_frame;
pub mod cell_commands;
pub mod color_pipeline;
pub mod config;
pub mod console;
mod console_selection;
pub mod door_crossing;
pub mod doors;
pub mod emittance;
pub mod fingerprint;
pub mod frame_budget;
pub mod hdr_probe;
pub mod indirect_metrics;
pub mod lights;
mod live_console;
mod matched_route;
pub mod mesh_preparation_retry;
pub mod metrics;
pub mod nif_depth;
pub mod nif_material;
pub mod pacing;
pub mod papyrus_runtime;
pub mod physics;
pub mod prepass_vertex_alpha;
pub mod profiling;
pub mod render;
pub mod render_timing;
mod renderer_init;
pub mod route;
mod scene_evidence;
pub mod schedule_timing;
pub mod settings;
pub mod settings_ui;
pub mod shots;
pub mod skin_material;
pub mod sky;
pub mod skyrim_ini;
pub mod streaming;
mod streaming_gpu_metrics;
mod streaming_preparation;
mod streaming_trace;
pub mod swap_bridge;
pub mod terrain_merge;
pub mod terrain_upload;
pub mod tone_mapping;
pub mod visibility_optimization;
pub mod visual_profile;
pub mod world;

pub use app::run;

mod mesh_residency_audit;

#[doc(hidden)]
pub mod collision_measure;

mod perf_span;

#[cfg(feature = "collision-measure")]
mod scene_placement_probe;
