//! Offline conversion of Skyrim assets into runtime-ready Mudcrab assets.

pub mod actor_models;
pub mod animation;
pub mod archive;
pub mod asset_path;
pub mod cache;
pub mod check;
pub mod collision;
pub mod compose;
pub mod config;
pub mod esm;
pub mod extensions;
pub mod integration;
pub mod lip;
pub mod lod;
pub mod material;
pub mod mesh;
mod metadata;
pub mod pipeline;
pub mod progress;
pub mod reader_diff;
pub mod records;
pub mod repair;
pub mod script;
pub mod streaming_costs;
pub mod terrain_arrays;
pub mod terrain_chunks;
pub mod terrain_pipeline;
pub mod texture;
pub mod texture_gpu;
mod texture_ktx2;

#[cfg(test)]
mod test_strategies;
pub use check::{
    CheckCancelled, CheckMode, CheckProblem, CheckReport, check_output, check_output_with_cancel,
};
pub use config::{
    OutputDirError, PipelineConfig, TextureEncoder, check_output_dir, find_resumable_staging,
};
pub use esm::EsmParser;
pub use integration::IntegrationReport;
pub use pipeline::{AssetPipeline, Cancellation, PipelineFailure, PipelineReport};
pub use progress::{AssetOutcome, ProgressEstimate, ProgressEvent, ProgressStage};
pub use shared;
