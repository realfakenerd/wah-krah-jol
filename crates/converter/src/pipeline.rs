use crate::{
    animation,
    archive::{
        ArchiveExtractor, ExtractOptions, ExtractionProgress, ExtractionTimings,
        RawIngestionSession, prune_stale_reader_packs,
    },
    asset_path::{AssetKind, canonical_asset_path, normalize_separators, resolve_asset_uri},
    cache::{
        BODY_COMPOSE_CONTEXT, BODY_COMPOSE_RECIPE, CONVERTER_SCHEMA_VERSION, CacheEntry,
        ConversionManifest, DEFAULT_GAIT_RECIPE, DerivedOutput, IngestionJournal,
        IngestionSelection, StagedOutput, StagingJournal, asset_configuration_hash_for_schema,
        body_compose_inputs, can_reuse_meshes, can_reuse_scripts_and_archives, configuration_hash,
        configuration_hash_for_schema, current_configuration_hash_for_schema, default_gait_inputs,
        hash_bytes, hash_file, link_or_copy, load_staged_outputs,
    },
    compose::{self, BodyPart, BodyPartKind},
    config::{PipelineConfig, TextureEncoder},
    esm::{
        EsmParser,
        cell_cache::write_cell_cache,
        exporter::validate_database,
        lodsettings::{LodSettings, sidecar_path},
        plugins_txt::read_with_selection_diagnostics,
    },
    integration::{IntegrationReport, finalize_world_database_with_sources},
    lod::{
        albedo::{LoadedDefault, TerrainAtlas, TerrainTextureCache, TerrainTextures},
        reuse::{LodManifest, LodReuse},
        terrain::{
            PreparedTerrainChunk, TerrainCellCache, TerrainChunk, exterior_terrain_cells,
            publish_chunk, read_cached_heights, terrain_jobs, validate_chunk_members,
        },
    },
    mesh::MeshConverter,
    progress::{AssetOutcome, ProgressEvent, ProgressStage},
    script::ScriptConverter,
    texture::{TextureConverter, TextureEncoding, TextureSemantic, publish_ktx2_file},
    texture_gpu::{self, GpuJob, GpuUastc, PreparedTexture},
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use log::{info, warn};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use shared::{
    asset_lock::{AssetLock, AssetLockMode, resolve_asset_path},
    lod::LodOrigin,
    plugin_selection::CreationFallbackWarning,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc::{Sender, unbounded_channel},
    task::spawn_blocking,
};
use walkdir::WalkDir;

#[path = "database_cache.rs"]
mod database_cache;

#[cfg(test)]
#[path = "pipeline/rv_b_tests.rs"]
mod rv_b_tests;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineReport {
    pub complete: bool,
    pub converted: u64,
    pub cache_hits: u64,
    pub skipped: u64,
    /// Dangling texture references the published meshes omit because the game
    /// data does not contain those textures: references this run pruned plus the
    /// records it carried forward for meshes it reused. Matches the
    /// `pruned_texture_references` map published in the manifest. Counted
    /// separately from `skipped` and `warnings`: nothing failed to convert, so a
    /// prune never makes the run incomplete.
    pub pruned_texture_references: u64,
    /// Terrain LOD chunks produced this run, including verified reused chunks. Zero when no worldspace had a
    /// valid origin or LOD was disabled; a world without one is recorded as an LOD omission, never
    /// given an assumed origin (GEOM-02).
    #[serde(default)]
    pub lod_chunks: u64,
    /// Verified prior-package chunks reused without baking/encoding.
    #[serde(default)]
    pub lod_cache_hits: u64,
    #[serde(default)]
    pub lod_gpu_chunks: u64,
    #[serde(default)]
    pub lod_cpu_fallback_chunks: u64,
    /// Wall time spent compiling terrain LOD, excluding other conversion stages.
    #[serde(default)]
    pub lod_elapsed_ms: u128,
    /// Wall time of package publication, including seals/verification when replacing
    /// an output. Excludes subsequent ingestion-cache persistence and pruning.
    #[serde(default)]
    pub publication_elapsed_ms: u128,
    /// Exclusive publication operations within `publication_elapsed_ms`. Kept separate
    /// from `phase_elapsed_ms`, whose publication entry already includes these spans.
    #[serde(default)]
    pub publication_phase_elapsed_ms: BTreeMap<String, u128>,
    /// Wall-clock spans measured at the owning operation. These are separate from
    /// the CLI's first and last progress events.
    #[serde(default)]
    pub phase_elapsed_ms: BTreeMap<String, u128>,
    #[serde(default)]
    pub archive_timings: BTreeMap<String, ExtractionTimings>,
    #[serde(default)]
    pub database_cache_hit: bool,
    #[serde(default)]
    pub database_cache_key: Option<String>,
    #[serde(default)]
    pub database_cache_files: u64,
    /// Surviving runtime winners exported by a fresh frontend; zero on database reuse.
    #[serde(default)]
    pub database_runtime_records_built: u64,
    #[serde(default)]
    pub lod_warnings: Vec<String>,
    /// The merged terrain (#215) this run built, when `merged_terrain` is on: the land texture
    /// arrays, one merged mesh per exterior cell, the chunk files, and every cell or texture
    /// left out with its reason. `None` when the option is off or no world database was built.
    #[serde(default)]
    pub terrain: Option<crate::terrain_pipeline::TerrainReport>,
    /// Wall time spent building merged terrain, excluding other conversion stages.
    #[serde(default)]
    pub terrain_elapsed_ms: u128,
    /// Merged-terrain omissions. Like `lod_warnings`, these do not make the run incomplete: a
    /// skipped land texture or cell falls back at runtime, and the full list is in `terrain`.
    #[serde(default)]
    pub terrain_warnings: Vec<String>,
    pub warnings: Vec<String>,
    /// Advisory messages, including successful reader compatibility remaps,
    /// that do not affect completeness.
    #[serde(default)]
    pub notices: Vec<String>,
    /// Original plugin filenames omitted by selection recovery, once each in selection order.
    /// Full exclusions stay available without expanding the bounded user-facing warning summaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_plugins: Vec<String>,
    /// Surviving original plugin filenames in the slot order published in the database.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub load_order: Vec<String>,
    /// Input-policy warnings, distinct from failed asset conversions in `warnings`.
    /// A Creation fallback is visible to the user without making usable output incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selection_warnings: Vec<CreationFallbackWarning>,
    pub artifacts: Vec<PathBuf>,
    pub inputs_by_kind: BTreeMap<String, u64>,
    pub elapsed_ms: u128,
    pub integration: Option<IntegrationReport>,
}

/// A run is complete when no asset conversion was skipped and no conversion warnings remain.
/// Recoverable plugin-selection exclusions are advisory notices and `skipped_plugins` entries.
/// Creation fallback warnings describe input policy; they do not indicate failed conversions.
/// Pruned dangling
/// texture references are deliberately absent: the game data does not contain those
/// textures, so dropping the reference is a fact about the source, not a failure to
/// convert. A failed archive, a mesh that will not convert or a failed integration
/// still skip or warn, so they still land here. Reader data-loss summaries warn
/// while usable records still publish; successful compatibility remaps are notices.
pub(crate) fn conversion_is_complete(report: &PipelineReport) -> bool {
    report.skipped == 0 && report.warnings.is_empty()
}

/// One line per projection omission category (#297): a lost light row warns, so the run is
/// incomplete; an unresolved emittance target keeps the model's emission and is only noted.
pub(crate) fn report_projection_omissions(
    report: &mut PipelineReport,
    omissions: &crate::esm::projection_omissions::ProjectionOmissions,
) {
    use crate::esm::projection_omissions::{Severity, summary_line};
    for (category, outcome) in &omissions.categories {
        let message = format!(
            "world database projection left rows out: {}",
            summary_line(category, outcome)
        );
        match outcome.severity {
            Severity::DataLoss => report.warnings.push(message),
            Severity::Fallback => report.notices.push(message),
        }
    }
}

/// A run's stop button. The command line sets it when the user interrupts (Ctrl+C); the pipeline
/// checks it between units of work, finishes the asset in flight, and leaves the staging folder
/// behind so `--resume-staging` can pick the run up where it stopped.
#[derive(Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    changed: Arc<tokio::sync::Notify>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Resolves once `cancel` has run, including a cancel that raced the first check.
    async fn cancelled(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// The error a run ends with when it was stopped (Ctrl+C, a launcher's Stop button) rather than
/// failing. A stop that raced a real error keeps that error as its cause, so a front end can still
/// show it; a plain stop has no cause. Front ends find it with `report.downcast_ref::<Interrupted>()`
/// instead of matching the error text.
#[derive(Debug, Default)]
pub struct Interrupted {
    cause: Option<color_eyre::Report>,
}

impl Interrupted {
    /// A stop with no error behind it.
    pub fn new() -> Self {
        Self::default()
    }

    /// A stop that landed while `error` was ending the same work. An `error` that is itself a
    /// plain stop (the extractor noticed the stop first) adds nothing, so it is not kept as a cause.
    pub fn after(error: color_eyre::Report) -> Self {
        match error.downcast_ref::<Interrupted>() {
            Some(stop) if stop.cause.is_none() => Self::new(),
            _ => Self { cause: Some(error) },
        }
    }

    /// The error that raced the stop, if there was one.
    pub fn cause(&self) -> Option<&color_eyre::Report> {
        self.cause.as_ref()
    }
}

impl fmt::Display for Interrupted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Some(cause) => write!(formatter, "conversion interrupted ({cause:#})"),
            None => formatter.write_str("conversion interrupted"),
        }
    }
}

impl std::error::Error for Interrupted {}

/// A run that stopped short of publishing. `staging` is the folder left behind, when one was kept,
/// and `cancelled` says whether the user interrupted the run rather than it failing.
#[derive(Debug)]
pub struct PipelineFailure {
    pub error: color_eyre::Report,
    pub staging: Option<PathBuf>,
    pub cancelled: bool,
}

impl fmt::Display for PipelineFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#}", self.error)
    }
}

impl std::error::Error for PipelineFailure {}

impl From<color_eyre::Report> for PipelineFailure {
    fn from(error: color_eyre::Report) -> Self {
        Self {
            error,
            staging: None,
            cancelled: false,
        }
    }
}

/// Renames the staged tree over the output, unless the run was interrupted while it was packing
/// up. The check here is the last one before the output changes, so an interrupt that lands after
/// the final stage stops the run instead of publishing over a good output — the staging folder is
/// kept either way, so the run resumes from where it stopped.
#[cfg(test)]
fn publish_if_not_interrupted(
    staging: &Path,
    output: &Path,
    cancellation: &Cancellation,
) -> Result<(), PipelineFailure> {
    if cancellation.is_cancelled() {
        return Err(failure(Interrupted::new().into(), staging, cancellation));
    }
    publish_directory(staging, output).map_err(|error| failure(error, staging, cancellation))
}

/// A failure with the staging folder attached when one was kept, so the caller can tell the user
/// how to resume.
fn failure(
    error: color_eyre::Report,
    staging: &Path,
    cancellation: &Cancellation,
) -> PipelineFailure {
    PipelineFailure {
        error,
        staging: staging.exists().then(|| staging.to_path_buf()),
        cancelled: cancellation.is_cancelled(),
    }
}

pub struct AssetPipeline;

impl AssetPipeline {
    /// Runs a conversion, reporting on `progress_tx`.
    ///
    /// The channel is bounded, and a front end should keep reading it. Events that matter on their
    /// own (a stage or an archive starting and ending, an asset failing, a notice) wait for room;
    /// the per-file updates sent while an archive is extracted are dropped when the channel is
    /// full, so extraction never waits on the front end. Each of them carries cumulative numbers,
    /// so the next one that gets through is as good as any that were dropped.
    pub async fn run_async(
        config: PipelineConfig,
        progress_tx: Sender<ProgressEvent>,
    ) -> Result<PipelineReport, PipelineFailure> {
        Self::run_async_with_cancel(config, progress_tx, Cancellation::new()).await
    }

    /// Freeze configuration inputs before staging and honor cancellation at publication.
    pub async fn run_async_with_cancel(
        mut config: PipelineConfig,
        progress_tx: Sender<ProgressEvent>,
        cancellation: Cancellation,
    ) -> Result<PipelineReport, PipelineFailure> {
        config.validate()?;
        config.freeze_schema_addons()?;
        reject_symlink_output(&config.output_dir)?;
        config.output_dir =
            resolve_asset_path(&config.output_dir).wrap_err("failed to resolve asset output")?;
        fs::create_dir_all(
            config
                .output_dir
                .parent()
                .ok_or_else(|| color_eyre::eyre::eyre!("output has no parent"))?,
        )
        .wrap_err("failed to create asset output parent")?;
        let output_lock = Arc::new(AssetLock::acquire_exclusive(&config.output_dir).map_err(
            |error| {
                let message = if error.is_held() {
                    format!(
                        "asset output {} is in use; close its engine or inspector before conversion",
                        config.output_dir.display()
                    )
                } else {
                    format!(
                        "failed to lock asset output {} before conversion",
                        config.output_dir.display()
                    )
                };
                color_eyre::Report::from(error).wrap_err(message)
            },
        )?);
        // Hold the runtime lock before repair-journal recovery can write the pack,
        // and the distinct repair lock before directory recovery or publication.
        // Both guards remain alive through cache cleanup; publication reuses output_lock.
        let ownership = Arc::new(crate::repair::OutputOwnership::acquire(&config.output_dir)?);
        config.output_dir = ownership.output().to_path_buf();
        recover_interrupted_publication(&config.output_dir)?;
        let started = Instant::now();
        send(
            &progress_tx,
            ProgressStage::Discovering,
            0,
            0,
            None,
            "Discovering Skyrim assets",
        )
        .await;
        let loaded_manifest = if config.invalidate_cache {
            ConversionManifest::default()
        } else {
            ConversionManifest::load(&config.output_dir.join("conversion-manifest.json"))?
        };
        let mut previous_manifest = reusable_manifest(&config, loaded_manifest)?;
        if config.invalidate_cache {
            // Invalidation forgets converted outputs, not the capabilities the published pack
            // declared: the new pack must still say which of them it dropped.
            previous_manifest.extensions = published_extensions(&config.output_dir);
        }
        let staging = config
            .resume_staging
            .clone()
            .unwrap_or_else(|| staging_path(&config.output_dir));
        fs::create_dir_all(staging.join("vfs"))
            .wrap_err_with(|| format!("failed to create {}", staging.join("vfs").display()))?;
        if config.resume_staging.is_some() {
            invalidate_staged_generated_outputs(&staging)?;
        }

        // A run that stops keeps its staging folder, whether it failed or was interrupted: the
        // folder is everything the run has done so far, and the caller reports the command that
        // resumes from it. Only a successful publish removes it, once the runtime pack is out.
        let setup_elapsed_ms = started.elapsed().as_millis();
        // The terrain worker keeps the runtime and repair leases until it has stopped, even if
        // this caller is dropped first; publication below still reuses the same guards.
        let lod_ownership = Arc::new(LodWorkerOwnership {
            _runtime: vec![output_lock.clone()],
            _repair: Some(ownership.clone()),
        });
        let run_result = Self::run_into(
            &config,
            &staging,
            &previous_manifest,
            &progress_tx,
            &cancellation,
            lod_ownership,
        )
        .await;
        let mut report = match run_result {
            Ok(report) => report,
            Err(error) => return Err(failure(error, &staging, &cancellation)),
        };
        report
            .phase_elapsed_ms
            .insert("run_setup".into(), setup_elapsed_ms);
        send(
            &progress_tx,
            ProgressStage::Publishing,
            1,
            1,
            None,
            "Publishing converted assets",
        )
        .await;
        // The run's own checks are behind it, but an interrupt that landed while it was packing up
        // must still stop it before the runtime pack is published. The folder is kept, so the run
        // resumes from where it stopped.
        if cancellation.is_cancelled() {
            return Err(failure(Interrupted::new().into(), &staging, &cancellation));
        }
        let publication_started = Instant::now();
        report.publication_phase_elapsed_ms =
            publish_runtime_pack(&staging, &config.output_dir, &mut report, &output_lock)
                .map_err(|error| failure(error, &staging, &cancellation))?;
        report.artifacts.push(PathBuf::from(
            shared::streaming_costs::STREAMING_COST_FILE_NAME,
        ));
        report.publication_elapsed_ms = publication_started.elapsed().as_millis();
        report
            .phase_elapsed_ms
            .insert("runtime_publication".into(), report.publication_elapsed_ms);
        // The runtime pack is published and sealed; everything below only maintains the
        // ingestion cache. A failure here is a notice, never a failed conversion: the pack on
        // disk is good and complete, so the run must not hand back a resume command for it.
        let cache_persistence_started = Instant::now();
        let cache_root = config.ingestion_cache_dir().join(".ingestion-cache");
        if staging.join(".ingestion-cache").is_dir()
            && let Err(error) =
                persist_ingestion_cache(&staging.join(".ingestion-cache"), &cache_root)
        {
            push_post_publication_notice(
                &mut report.notices,
                format!(
                    "published the asset pack, but could not save the ingestion cache to {}: {error:#}",
                    cache_root.display()
                ),
            );
        }
        report.phase_elapsed_ms.insert(
            "ingestion_cache_persistence".into(),
            cache_persistence_started.elapsed().as_millis(),
        );
        let manifest_reload_started = Instant::now();
        let published_manifest =
            ConversionManifest::load(&config.output_dir.join("conversion-manifest.json"));
        report.phase_elapsed_ms.insert(
            "published_manifest_reload".into(),
            manifest_reload_started.elapsed().as_millis(),
        );
        let cache_pruning_started = Instant::now();
        match published_manifest {
            Ok(manifest) => {
                if let Err(error) = prune_stale_ingestion_blobs(&cache_root, &manifest) {
                    push_post_publication_notice(
                        &mut report.notices,
                        format!(
                            "published the asset pack, but stale ingestion-cache pruning failed: {error:#}"
                        ),
                    );
                }
            }
            Err(error) => push_post_publication_notice(
                &mut report.notices,
                format!(
                    "published the asset pack, but its conversion manifest could not be reloaded, so stale ingestion-cache pruning was skipped: {error:#}"
                ),
            ),
        }
        report.phase_elapsed_ms.insert(
            "ingestion_cache_pruning".into(),
            cache_pruning_started.elapsed().as_millis(),
        );
        // Resumed or not, the staging folder has done its job: the pack links the published files,
        // so removing it frees only names. Kept, a resumed folder would be offered for resume
        // again (`find_resumable_staging`) while holding a full copy's worth of disk. The output is
        // already published, so a folder that cannot or must not be removed is only a warning.
        let cleanup_started = Instant::now();
        remove_staging(&staging, &config.data_dir);
        report.phase_elapsed_ms.insert(
            "staging_cleanup".into(),
            cleanup_started.elapsed().as_millis(),
        );
        report.elapsed_ms = started.elapsed().as_millis();
        if report.complete {
            send(
                &progress_tx,
                ProgressStage::Complete,
                1,
                1,
                None,
                "Asset conversion complete",
            )
            .await;
        }
        Ok(report)
    }

    /// Converts resolved inputs into staging, reusing valid cached outputs and reporting progress.
    ///
    /// Rebuilds MO2 input selection on resume and records conversion results in the staging manifest.
    /// Preserves usable outputs through texture repair, alias publication, pruning, and artifact
    /// verification, and returns the report for pack publication.
    async fn run_into(
        config: &PipelineConfig,
        staging: &Path,
        previous: &ConversionManifest,
        progress_tx: &Sender<ProgressEvent>,
        cancellation: &Cancellation,
        lod_ownership: Arc<LodWorkerOwnership>,
    ) -> Result<PipelineReport> {
        let mut report = PipelineReport::default();
        let initialization_started = Instant::now();
        let expected_configuration = configuration_hash(config)?;
        // Provenance of the outputs already in `staging`, written by the run
        // that was interrupted. Invalidation drops it along with the published
        // manifest, so with `--invalidate-cache` every output is converted
        // again.
        let staged_outputs = Arc::new(if config.invalidate_cache {
            BTreeMap::new()
        } else {
            load_staged_outputs(staging).wrap_err_with(|| {
                format!(
                    "failed to read the staging journal in {}",
                    staging.display()
                )
            })?
        });
        let mut journal = StagingJournal::open(staging)?;
        let staged_archives = if config.invalidate_cache {
            BTreeMap::new()
        } else {
            IngestionJournal::load(staging)?
        };
        let mut ingestion_journal = IngestionJournal::open(staging)?;
        let mut manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            retained_mesh_schema_version: None,
            retained_asset_configuration_hash: None,
            complete: false,
            // A fresh run has no record losses when there are no selected plugins.
            // The authoritative reader replaces this proof when plugins are converted.
            record_data_complete: Some(true),
            configuration_hash: expected_configuration.clone(),
            schema_addons: config.schemas()?.identities().to_vec(),
            asset_configuration_hashes: ["nif", "dds", "pex"]
                .into_iter()
                .map(|kind| {
                    Ok((
                        kind.to_owned(),
                        asset_configuration_hash_for_schema(
                            config,
                            CONVERTER_SCHEMA_VERSION,
                            kind,
                        )?,
                    ))
                })
                .collect::<Result<_>>()?,
            inputs_by_kind: Default::default(),
            failures: Default::default(),
            excluded_inputs: Default::default(),
            pruned_texture_references: Default::default(),
            archives: Default::default(),
            entries: Default::default(),
            derived_outputs: Default::default(),
            extensions: Vec::new(),
        };
        report.phase_elapsed_ms.insert(
            "conversion_initialization".into(),
            initialization_started.elapsed().as_millis(),
        );
        let discovery_started = Instant::now();
        let mut resolved_files = BTreeMap::new();
        let mut mo2_plugins = None;
        let mut mo2_candidates = Vec::new();
        if let Some(selection) = &config.mo2 {
            let instance = mo2::Instance::open(&selection.instance_path)?;
            let data = config.data_dir.clone();
            let profile = selection.profile.clone();
            let stop = cancellation.clone();
            let resolved = spawn_blocking(move || {
                instance.resolve_with_cancel(&data, &profile, &|| stop.is_cancelled())
            })
            .await
            .wrap_err("MO2 resolver worker panicked")?;
            interrupt(cancellation)?;
            let resolved = resolved?;
            report.notices.extend(resolved.notices);
            report
                .selection_warnings
                .extend(resolved.selection_warnings);
            mo2_candidates = resolved.plugin_candidates;
            // Rebuild archive-derived inputs on resume; loose assets are read directly from
            // their winning source paths. Remove legacy copied inputs from older runs too.
            for root in [staging.join(".mo2-input")] {
                if root.exists() {
                    fs::remove_dir_all(&root)?;
                }
            }
            mo2_plugins = Some(
                resolved
                    .plugins
                    .iter()
                    .map(|name| resolved.files[name].clone())
                    .collect::<Vec<_>>(),
            );
            resolved_files = resolved.files;
        }
        let files = if config.mo2.is_some() {
            resolved_files.values().cloned().collect()
        } else {
            discover(&config.data_dir)?
        };
        let archive_keys: BTreeMap<_, _> = resolved_files
            .iter()
            .filter(|(_, source)| extension(source, &["bsa", "ba2"]))
            .map(|(relative, source)| (source.clone(), relative.clone()))
            .collect();
        let plugins = match mo2_plugins {
            Some(plugins) => {
                let profile = &config.mo2.as_ref().unwrap().profile;
                let mut recovery_notices = Vec::new();
                let plugins = crate::esm::plugins_txt::recover_with_diagnostics(
                    plugins,
                    &mut recovery_notices,
                    &mut report.skipped_plugins,
                )?;
                crate::esm::plugins_txt::record_recovery_skips(
                    &mo2_candidates,
                    &plugins,
                    &mut report.skipped_plugins,
                );
                report.notices.extend(
                    recovery_notices
                        .into_iter()
                        .map(|notice| format!("MO2 profile {profile}: {notice}")),
                );
                ensure!(
                    !plugins.is_empty(),
                    "MO2 profile {profile}: no usable active plugins remain"
                );
                crate::esm::load_order::LoadOrder::read(&plugins).wrap_err_with(|| {
                    format!(
                        "MO2 profile {}: invalid plugin/master order",
                        config.mo2.as_ref().unwrap().profile
                    )
                })?;
                plugins
            }
            None => plugin_paths_with_diagnostics(
                config,
                &files,
                &mut report.notices,
                &mut report.skipped_plugins,
                &mut report.selection_warnings,
            )?,
        };
        report.load_order = plugins
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        // Notices go out on the progress channel, like the pruned-texture warnings, so the CLI
        // prints them without splicing into its status line and the launcher shows them in its
        // notice pane. They stay in `report.notices` for the summary and the JSON report.
        // Automatic discovery with no usable plugins can only carry the subfolder note; once a
        // plugin is selected, every selection notice reports a recovery and is a warning.
        let selection_warns =
            config.plugins_file.is_some() || config.mo2.is_some() || !plugins.is_empty();
        for notice in &report.notices {
            send_notice(
                progress_tx,
                ProgressStage::Discovering,
                None,
                &format!(
                    "{}: {notice}",
                    if selection_warns { "warning" } else { "note" }
                ),
            )
            .await;
        }
        for warning in &report.selection_warnings {
            send_notice(
                progress_tx,
                ProgressStage::Discovering,
                None,
                &format!("warning: {warning}"),
            )
            .await;
        }
        let plugin_hashes = plugins
            .iter()
            .map(|plugin| hash_file(plugin))
            .collect::<Result<Vec<_>>>()?;
        let archives: Vec<_> = files
            .iter()
            .filter(|path| extension(path, &["bsa", "ba2"]))
            .cloned()
            .collect();
        let archive_owners = if config.mo2.is_some() {
            mo2_installed_archive_owners(&resolved_files, &plugins)
        } else {
            installed_archive_owners(&config.data_dir, &files, &plugins)
        };
        let mut enabled_archives: Vec<_> = archives
            .into_iter()
            .filter(|archive| !extension(archive, &["ba2"]) || config.enable_ba2)
            .filter(|archive| {
                if config.mo2.is_some() {
                    mo2_archive_is_active(archive, &plugins)
                        && non_mo2_archive_is_active(archive, &archive_owners)
                } else {
                    non_mo2_archive_is_active(archive, &archive_owners)
                }
            })
            .collect();
        sort_archives_by_load_order(&mut enabled_archives, &plugins);
        report.phase_elapsed_ms.insert(
            "input_discovery_and_plugin_hashes".into(),
            discovery_started.elapsed().as_millis(),
        );
        let extraction_started = Instant::now();
        if !enabled_archives.is_empty() {
            send(
                progress_tx,
                ProgressStage::Extracting,
                0,
                enabled_archives.len() as u64,
                None,
                "Extracting Skyrim archives",
            )
            .await;
        }

        let vfs_dir = staging.join("vfs");
        // Reconstruct the effective input set; removed archives or loose files
        // must not survive from an interrupted run's VFS.
        fs::remove_dir_all(&vfs_dir)?;
        fs::create_dir_all(&vfs_dir)?;

        // Bytes already extracted, so each archive reports progress against the whole run instead
        // of restarting at zero.
        let mut extracted_bytes = 0u64;

        let raw_ingestion_session =
            Arc::new(RawIngestionSession::new(&staging.join(".ingestion-cache")));
        for (index, archive) in enabled_archives.iter().enumerate() {
            interrupt(cancellation)?;
            send(
                progress_tx,
                ProgressStage::Extracting,
                index as u64,
                enabled_archives.len() as u64,
                Some(archive.clone()),
                "Extracting archive",
            )
            .await;
            let archive_for_worker = archive.clone();
            let vfs_for_worker = vfs_dir.clone();
            let archive_key = archive_keys.get(archive).cloned().unwrap_or_else(|| {
                archive
                    .strip_prefix(&config.data_dir)
                    .unwrap_or(archive)
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_lowercase()
            });
            let previous_cache_root = if staged_archives.contains_key(&archive_key) {
                staging.join(".ingestion-cache")
            } else {
                config.ingestion_cache_dir().join(".ingestion-cache")
            };
            let cache_root = staging.join(".ingestion-cache");
            let previous_entry = staged_archives
                .get(&archive_key)
                .or_else(|| previous.archives.get(&archive_key))
                .cloned();
            let verify_cache = config.verify_cache;
            let ingestion_session_for_worker = Arc::clone(&raw_ingestion_session);
            let mut extraction_options =
                ExtractOptions::converter(config.cpu_jobs, config.io_jobs, None);
            // Canonical English reads only its own banks; any other language needs every bank
            // that the in-house reader may select, so it extracts the converter-input superset.
            extraction_options.selection = if config.extract_all_archive_files {
                IngestionSelection::All
            } else if crate::esm::inhouse::is_english(&config.language) {
                IngestionSelection::RuntimeEnglishV1
            } else {
                IngestionSelection::ConverterInputsV1
            };
            extraction_options.sync = config.ingestion_sync;
            extraction_options.reuse_cache = !config.invalidate_cache;

            // The extractor reports file by file; the event keeps the archive count the launcher
            // reads and carries the smoother per-file fraction for the status line.
            let archive_totals = Arc::new(Mutex::new((0u64, 0u64)));
            let totals_for_worker = Arc::clone(&archive_totals);
            let progress_for_worker = progress_tx.clone();
            let progress_archive = archive.clone();
            let archives_total = enabled_archives.len() as u64;
            let archive_index = index as u64;
            let progress = move |extracted: ExtractionProgress| {
                *totals_for_worker
                    .lock()
                    .expect("archive totals mutex poisoned") =
                    (extracted.total_files, extracted.total_bytes);
                let archive_fraction = if extracted.total_bytes > 0 {
                    extracted.completed_bytes as f32 / extracted.total_bytes as f32
                } else if extracted.total_files > 0 {
                    extracted.completed_files as f32 / extracted.total_files as f32
                } else {
                    1.0
                };
                // The extraction threads never wait on the front end: an update that finds the
                // channel full is dropped, and the next one carries the cumulative numbers. The
                // archive's start and end events, and every failure, are still sent reliably.
                let _ = progress_for_worker.try_send(
                    ProgressEvent::new(
                        ProgressStage::Extracting,
                        archive_index,
                        archives_total,
                        Some(progress_archive.clone()),
                        "Extracting archive",
                    )
                    .with_bytes(
                        extracted_bytes + extracted.completed_bytes,
                        extracted_bytes + extracted.total_bytes,
                    )
                    .with_stage_fraction(
                        (archive_index as f32 + archive_fraction) / archives_total as f32,
                    ),
                );
            };

            // A stop is checked before every entry, so it does not wait for a multi-gigabyte
            // archive to finish.
            let stop_for_worker = cancellation.clone();
            #[cfg(test)]
            let output_for_hook = config.output_dir.clone();
            let stop = move || {
                #[cfg(test)]
                stop_hook::tick(&output_for_hook, &stop_for_worker);
                stop_for_worker.is_cancelled()
            };

            let result = spawn_blocking(move || {
                ArchiveExtractor::extract_batched_with_session(
                    &archive_for_worker,
                    &vfs_for_worker,
                    &previous_cache_root,
                    &cache_root,
                    previous_entry.as_ref(),
                    verify_cache,
                    &extraction_options,
                    Some(&progress),
                    Some(&stop),
                    &ingestion_session_for_worker,
                )
            })
            .await
            .wrap_err("archive worker panicked")?;
            // An archive abandoned by a stop ends the run as an interrupt, the same as a stop
            // between archives, rather than being counted as a skipped archive. Its cache entry
            // was never recorded; a resume recovers its sealed partial batches.
            let result = match result {
                // A stop that races an archive error keeps the archive's error as its cause.
                Err(error) if cancellation.is_cancelled() => {
                    return Err(Interrupted::after(error).into());
                }
                result => result,
            };

            send(
                progress_tx,
                ProgressStage::Extracting,
                (index + 1) as u64,
                enabled_archives.len() as u64,
                Some(archive.clone()),
                "Extracted archive",
            )
            .await;

            match result {
                Ok(outcome) => {
                    report
                        .archive_timings
                        .insert(archive_key.clone(), outcome.timings);
                    let (_, bytes) = *archive_totals
                        .lock()
                        .expect("archive totals mutex poisoned");
                    extracted_bytes += bytes;
                    if outcome.cache_hit {
                        report.cache_hits += outcome.files.len() as u64;
                    } else {
                        report.converted += outcome.files.len() as u64;
                    }
                    ingestion_journal.record(&archive_key, &outcome.cache_entry)?;
                    manifest.archives.insert(archive_key, outcome.cache_entry);
                }
                Err(error) if !config.fail_fast => {
                    report.skipped += 1;
                    let message = format!("{}: {error:#}", archive.display());
                    manifest.failures.insert(
                        archive.to_string_lossy().replace('\\', "/"),
                        message.clone(),
                    );
                    report.warnings.push(message);
                }
                Err(error) => return Err(error),
            }
        }

        if config.mo2.is_none() {
            overlay_loose_assets(&config.data_dir, &staging.join("vfs"), &files)?;
        }
        report.phase_elapsed_ms.insert(
            "archive_ingestion_and_loose_overlay".into(),
            extraction_started.elapsed().as_millis(),
        );
        interrupt(cancellation)?;

        let mut merged_terrain_records = None;
        let mut merged_terrain_offsets = std::collections::HashMap::new();
        if !plugins.is_empty() {
            send(
                progress_tx,
                ProgressStage::Database,
                0,
                plugins.len() as u64,
                None,
                "Building skyrim_world.db",
            )
            .await;
            let db_path = staging.join("skyrim_world.db");
            let strings_root = staging.join("vfs");
            let string_roots = if config.mo2.is_some() {
                crate::esm::inhouse::StringRoots::EffectiveData {
                    archive_root: &strings_root,
                    loose_files: &resolved_files,
                }
            } else {
                crate::esm::inhouse::StringRoots::PluginLooseFirst(&strings_root)
            };
            let string_sources = crate::esm::inhouse::StringSources {
                roots: string_roots,
                language: &config.language,
            };
            let identity_started = Instant::now();
            let cache_identity =
                database_cache::identity(config, &plugins, string_sources, cancellation)?;
            report.database_cache_key =
                cache_identity.as_ref().map(|identity| identity.key.clone());
            report.phase_elapsed_ms.insert(
                "database_cache_identity".into(),
                identity_started.elapsed().as_millis(),
            );
            let database_cache_root = config.ingestion_cache_dir().join(".database-cache");
            let lookup_started = Instant::now();
            // Merged terrain reads the records of a fresh conversion, so it never restores a database.
            let mut restored = if !config.invalidate_cache && !config.merged_terrain {
                match &cache_identity {
                    Some(identity) => database_cache::restore(
                        &database_cache_root,
                        identity,
                        staging,
                        cancellation,
                    )?,
                    None => None,
                }
            } else {
                None
            };
            let validation_ms = restored
                .as_ref()
                .map_or(0, |restored| restored.validation_elapsed_ms);
            let restore_ms = lookup_started
                .elapsed()
                .as_millis()
                .saturating_sub(validation_ms);
            report
                .phase_elapsed_ms
                .insert("database_cache_lookup".into(), restore_ms);
            if restored.is_some() {
                let recheck_started = Instant::now();
                let unchanged = database_cache::inputs_unchanged(
                    cache_identity.as_ref().unwrap(),
                    config,
                    &plugins,
                    string_sources,
                    cancellation,
                )?;
                report.phase_elapsed_ms.insert(
                    "database_cache_restore_input_recheck".into(),
                    recheck_started.elapsed().as_millis(),
                );
                if !unchanged {
                    restored = None;
                    report
                        .phase_elapsed_ms
                        .insert("database_cache_rejected_validation".into(), validation_ms);
                    report.notices.push("base database cache was not reused because its inputs changed during restore".into());
                }
            }
            if let Some(restored) = restored {
                report.database_cache_hit = true;
                report.database_cache_files = restored.files;
                // Restored data repeats the fresh run's reader findings exactly once.
                let notes = database_cache::read_conversion_notes(staging)?;
                manifest.record_data_complete = Some(notes.warnings.is_empty());
                report.warnings.extend(notes.warnings);
                report.notices.extend(notes.notices);
                send(
                    progress_tx,
                    ProgressStage::Database,
                    plugins.len() as u64,
                    plugins.len() as u64,
                    None,
                    "Reusing verified skyrim_world.db",
                )
                .await;
                report
                    .phase_elapsed_ms
                    .insert("database_conversion".into(), 0);
                report.phase_elapsed_ms.insert("terrain_caches".into(), 0);
                report
                    .phase_elapsed_ms
                    .insert("database_validation".into(), validation_ms);
                let artifacts_started = Instant::now();
                database_cache::mark_reused_profile(
                    staging,
                    cache_identity.as_ref().unwrap(),
                    restore_ms,
                    validation_ms,
                )?;
                report.phase_elapsed_ms.insert(
                    "database_artifacts_and_release".into(),
                    artifacts_started.elapsed().as_millis(),
                );
            } else {
                let database_started = Instant::now();
                // SQLite writes in place. Every fresh build unlinks old names, and a restored
                // bundle uses private copies, so neither can mutate the previous pack or cache.
                database_cache::clear_outputs(staging)?;
                let conversion =
                    EsmParser::convert_plugins_with_reader_and_schemas_with_string_sources(
                        &plugins,
                        &db_path,
                        config.record_reader,
                        string_sources,
                        Some(config.schemas()?.as_ref()),
                    )?;
                database_cache::write_conversion_notes(
                    staging,
                    &database_cache::ConversionNotes {
                        warnings: conversion.warnings.clone(),
                        notices: conversion.notices.clone(),
                    },
                )?;
                manifest.record_data_complete = Some(conversion.warnings.is_empty());
                report.warnings.extend(conversion.warnings);
                report.notices.extend(conversion.notices);
                let phases = conversion.phases;
                let merged = conversion.records;
                report.database_runtime_records_built = merged.len() as u64;
                report.phase_elapsed_ms.insert(
                    "database_conversion".into(),
                    database_started.elapsed().as_millis(),
                );
                interrupt(cancellation)?;
                let database_validation_started = Instant::now();
                validate_database(&Connection::open(&db_path)?)?;
                report.phase_elapsed_ms.insert(
                    "database_validation".into(),
                    database_validation_started.elapsed().as_millis(),
                );
                let terrain_caches_started = Instant::now();
                if config.record_reader == crate::config::RecordReader::Inhouse {
                    crate::esm::inhouse::write_database_profile(staging, &db_path, &phases)?;
                    crate::esm::inhouse::write_terrain_caches(&merged, &db_path, staging)?;
                } else {
                    write_cell_cache(&merged, &staging.join("cell_cache.rkyv"))?;
                }
                report.phase_elapsed_ms.insert(
                    "terrain_caches".into(),
                    terrain_caches_started.elapsed().as_millis(),
                );
                let database_artifacts_started = Instant::now();
                crate::esm::inhouse::write_configured_reader_identity(staging, config)?;
                // Merged terrain (#215) keeps only the records its stage reads, and the LAND each
                // cell keeps is checked before the texture, mesh and LOD stages run, so a bad load
                // order fails in minutes. The positions are the ones the cell cache was ordered by.
                if config.merged_terrain {
                    merged_terrain_offsets = crate::terrain_pipeline::cell_cache_source_offsets(
                        config.record_reader,
                        &db_path,
                    )?;
                    crate::esm::cell_cache::merged_terrain_lands(&merged, &merged_terrain_offsets)?;
                    merged_terrain_records = Some(crate::terrain_pipeline::terrain_records(merged));
                } else {
                    drop(merged);
                }
                report.phase_elapsed_ms.insert(
                    "database_artifacts_and_release".into(),
                    database_artifacts_started.elapsed().as_millis(),
                );
                interrupt(cancellation)?;
                if let Some(identity) = &cache_identity {
                    let recheck_started = Instant::now();
                    let current =
                        database_cache::identity(config, &plugins, string_sources, cancellation)?;
                    report.phase_elapsed_ms.insert(
                        "database_cache_input_recheck".into(),
                        recheck_started.elapsed().as_millis(),
                    );
                    if current
                        .as_ref()
                        .is_some_and(|current| current.key == identity.key)
                    {
                        let store_started = Instant::now();
                        if let Err(error) = database_cache::store(
                            &database_cache_root,
                            identity,
                            staging,
                            cancellation,
                        ) {
                            if error.downcast_ref::<Interrupted>().is_some() {
                                return Err(error);
                            }
                            report
                                .notices
                                .push(format!("base database cache was not stored: {error:#}"));
                        }
                        report.phase_elapsed_ms.insert(
                            "database_cache_persistence".into(),
                            store_started.elapsed().as_millis(),
                        );
                    } else if current.is_some() {
                        report.notices.push("base database cache was not stored because its inputs changed during conversion".into());
                    } else {
                        report.notices.push("base database cache was not stored because its identity could not be verified".into());
                    }
                }
            }
            if config.record_reader == crate::config::RecordReader::Inhouse {
                report
                    .artifacts
                    .push(PathBuf::from(crate::esm::inhouse::DATABASE_PROFILE_FILE));
                report
                    .artifacts
                    .push(PathBuf::from("inhouse-reader-diagnostics.json"));
                report
                    .artifacts
                    .push(PathBuf::from("cell_cache_preserved.rkyv"));
            }
            report.artifacts.extend([
                PathBuf::from("skyrim_world.db"),
                PathBuf::from("cell_cache.rkyv"),
                PathBuf::from("record-reader.json"),
            ]);
        }

        let asset_setup_started = Instant::now();
        let vfs_root = staging.join("vfs");
        let mut vfs_files: BTreeMap<PathBuf, PathBuf> = discover(&vfs_root)?
            .into_iter()
            .map(|path| (path.clone(), path))
            .collect();
        let mut loose_keys = BTreeSet::new();
        for (relative, source) in &resolved_files {
            if crate::asset_path::is_authoring_resource(relative) {
                manifest.excluded_inputs.insert(
                    relative.clone(),
                    "BodySlide/Outfit Studio authoring resource, not runtime data".into(),
                );
                continue;
            }
            let (kind, ext) = if extension(source, &["dds"]) {
                (AssetKind::Texture, "dds")
            } else if extension(source, &["nif"]) {
                (AssetKind::Mesh, "nif")
            } else if extension(source, &["pex"]) {
                (AssetKind::Script, "pex")
            } else {
                continue;
            };
            let canonical = canonical_asset_path(relative, kind, ext)?;
            ensure!(
                loose_keys.insert(canonical.clone()),
                "loose assets contain normalized path collision for {canonical}"
            );
            vfs_files.insert(vfs_root.join(canonical), source.clone());
        }
        vfs_files.retain(|path, _| {
            let relative = path
                .strip_prefix(&vfs_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if crate::asset_path::is_authoring_resource(&relative) {
                manifest.excluded_inputs.insert(
                    relative,
                    "BodySlide/Outfit Studio authoring resource, not runtime data".into(),
                );
                false
            } else {
                true
            }
        });
        let source_paths = vfs_files;
        let vfs_files: Vec<_> = source_paths.keys().cloned().collect();
        if config.mo2.is_some() {
            prune_removed_mo2_outputs(staging, &vfs_files)?;
            if plugins.is_empty() {
                for name in [
                    "skyrim_world.db",
                    "cell_cache.rkyv",
                    "integration-report.json",
                ] {
                    let path = staging.join(name);
                    if path.is_file() {
                        fs::remove_file(path)?;
                    }
                }
            }
        }
        // Canonical source texture keys, used to tell a failed publication from absent game data
        // and to detect a pruned texture whose source is back in the installed data.
        let source_textures = texture_source_keys(staging, &vfs_files);
        let restored_meshes = restored_mesh_outputs(previous, &source_textures);
        remove_orphan_staged_assets(staging, &vfs_files)?;
        report.phase_elapsed_ms.insert(
            "asset_discovery_and_setup".into(),
            asset_setup_started.elapsed().as_millis(),
        );
        let meshes_started = Instant::now();
        {
            let mut batch = ConversionBatch {
                config,
                staging,
                previous,
                staged: Arc::clone(&staged_outputs),
                journal: &mut journal,
                manifest: &mut manifest,
                report: &mut report,
                progress_tx,
                cancellation,
            };
            batch
                .convert_kind(
                    &vfs_files,
                    &source_paths,
                    "nif",
                    ProgressStage::Meshes,
                    None,
                    &restored_meshes,
                )
                .await?;
        }
        // Only accepted current-source outputs may participate in directory scans.
        // Delaying cleanup preserves verified same-schema pruned bytes; audit
        // records alone cannot supply provenance or survive a failed conversion.
        let accepted_meshes = report
            .artifacts
            .iter()
            .filter(|path| extension(path, &["glb"]))
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        invalidate_staged_mesh_outputs(staging, &accepted_meshes)?;
        compose_default_bodies(
            staging,
            &source_paths,
            config.fail_fast,
            &mut report,
            &mut manifest,
        )?;
        for sex in ["male", "female"] {
            let relative = compose::composed_body_path("default", sex);
            if staging.join(&relative).is_file() {
                report
                    .artifacts
                    .push(normalize_separators(&relative).into());
            }
        }
        if config.mo2.is_some() {
            stage_mo2_animation_inputs(&staging.join("vfs"), &resolved_files)?;
        }
        convert_default_animations(
            staging,
            &source_paths,
            config.fail_fast,
            &mut report,
            &mut manifest,
        )?;
        for sex in ["male", "female"] {
            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                let relative = default_animation_output(sex, stem);
                if staging.join(&relative).is_file() {
                    report.artifacts.push(relative.into());
                }
            }
        }
        let texture_semantics = collect_texture_semantics(staging, &source_textures)?;
        report.phase_elapsed_ms.insert(
            "meshes_and_texture_semantics".into(),
            meshes_started.elapsed().as_millis(),
        );
        let textures_started = Instant::now();
        {
            let mut batch = ConversionBatch {
                config,
                staging,
                previous,
                staged: Arc::clone(&staged_outputs),
                journal: &mut journal,
                manifest: &mut manifest,
                report: &mut report,
                progress_tx,
                cancellation,
            };
            batch
                .convert_kind(
                    &vfs_files,
                    &source_paths,
                    "dds",
                    ProgressStage::Textures,
                    Some(&texture_semantics),
                    &BTreeSet::new(),
                )
                .await?;
            // Repair after the canonical DDS target publishes, retaining sampler aliases in
            // the URI so the alias publisher below creates the correct per-image variant.
            let mut repaired =
                MeshConverter::repair_volcanic_grass_normals(staging, &source_textures)?;
            let aliases = publish_texture_aliases(staging)?;
            batch.report.artifacts.extend(aliases);
            // A reused mesh that still holds its published bytes keeps the prune record of the
            // run that wrote it: a prune only removes references, so an older record stays true.
            // The published copy proves that, and so does the previous manifest when the staged
            // mesh has the hash it recorded for the same source: that manifest was published
            // together with the record and holds the pruned bytes' hash, so the published copy
            // may be missing. Captured before the prune pass rewrites any staged GLB, because
            // afterwards a mesh pruned again no longer matches either.
            let mut carried_prunes: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // Resolving an output to the keys that produced it once keeps the scan below linear:
            // a real pack holds hundreds of thousands of entries, and walking them for every
            // pruned candidate would be quadratic.
            // Built only when there are prune records to check, so a fresh run pays nothing.
            let entries_by_output: BTreeMap<&str, Vec<&str>> = {
                let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                let entries = if previous.pruned_texture_references.is_empty() {
                    None
                } else {
                    Some(&batch.manifest.entries)
                };
                for (key, entry) in entries.into_iter().flatten() {
                    map.entry(entry.output.as_str())
                        .or_default()
                        .push(key.as_str());
                }
                map
            };
            for (glb, references) in &previous.pruned_texture_references {
                let staged_glb = staging.join(glb);
                let published = files_are_identical(&staged_glb, &config.output_dir.join(glb));
                if published
                    || staged_matches_previous_entry(&batch, &entries_by_output, glb, &staged_glb)
                {
                    carried_prunes.insert(glb.clone(), references.iter().cloned().collect());
                }
            }
            // A missing artifact whose DDS source exists under `staging/vfs` is a failed
            // publication, not absent game data: the prune leaves those references alone.
            let mut pruned =
                MeshConverter::prune_dangling_texture_uris_with_sources(staging, &source_textures)?;
            pruned.append(&mut repaired);
            let pruned_uris: u64 = pruned
                .iter()
                .map(|file| file.removed_uris.len() as u64)
                .sum();
            let mut pruned_completed = 0;
            for file in &pruned {
                for (original, target) in &file.repaired_uris {
                    send_notice(
                        progress_tx,
                        ProgressStage::Textures,
                        Some(PathBuf::from(&file.glb)),
                        &format!(
                            "repaired texture {original} -> {target} referenced by {}",
                            file.glb
                        ),
                    )
                    .await;
                }
                for uri in &file.removed_uris {
                    pruned_completed += 1;
                    // The warning goes out as a notice on the progress channel rather than to
                    // stderr, so it cannot splice into the status line the CLI is redrawing.
                    send_notice(
                        progress_tx,
                        ProgressStage::Textures,
                        Some(PathBuf::from(&file.glb)),
                        &format!(
                            "warning: pruned dangling texture {uri} referenced by {} (no converted artifact)",
                            file.glb
                        ),
                    )
                    .await;
                    let reference = resolve_asset_uri(staging, &staging.join(&file.glb), uri)
                        .ok()
                        .and_then(|resolved| {
                            resolved.strip_prefix(staging).ok().map(Path::to_path_buf)
                        })
                        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|| uri.clone());
                    if batch.record_pruned_texture_reference(&file.glb, &reference) {
                        send(
                            batch.progress_tx,
                            ProgressStage::Textures,
                            pruned_completed,
                            pruned_uris,
                            Some(PathBuf::from(&file.glb)),
                            "Texture reference pruned",
                        )
                        .await;
                    }
                }
            }
            // Merge the carried records with this run's prunes: a reused mesh that already
            // omitted A and has just had B pruned must report both. A mesh whose source changed
            // was converted again before this pass, so its staged bytes no longer matched the
            // published copy and its stale records were not carried.
            for (glb, references) in &carried_prunes {
                for reference in references {
                    batch.record_pruned_texture_reference(glb, reference);
                }
            }
            // Repair or pruning rewrote the GLB after its cache entry was recorded, so refresh every
            // entry that publishes it from the rewritten bytes. Otherwise the next run fails the
            // entry's size and hash check and converts the mesh again.
            //
            // The staging journal is deliberately left alone. A journal record carries no prune
            // references, so certifying the pruned bytes would let a resumed run reuse the mesh
            // with nothing left for the prune pass to find, and the published manifest would lose
            // the record. The mesh's journal record still holds its original conversion hash,
            // which repaired or pruned bytes no longer match, so a resume rewrites it again.
            let pruned_outputs: BTreeSet<String> = pruned
                .iter()
                .filter(|file| !file.removed_uris.is_empty() || !file.repaired_uris.is_empty())
                .map(|file| file.glb.clone())
                .collect();
            for entry in batch.manifest.entries.values_mut() {
                if !pruned_outputs.contains(&entry.output) {
                    continue;
                }
                let path = staging.join(&entry.output);
                entry.output_size = fs::metadata(&path)
                    .wrap_err_with(|| format!("failed to inspect pruned {}", path.display()))?
                    .len();
                entry.output_hash = hash_file(&path)?;
            }
            finalize_derived_outputs(staging, batch.report, batch.manifest)?;
            // The run summary reports what the manifest records, whether this run
            // pruned it or carried the record forward for a reused mesh.
            let recorded: u64 = batch
                .manifest
                .pruned_texture_references
                .values()
                .map(|references| references.len() as u64)
                .sum();
            batch.report.pruned_texture_references = recorded;
            batch.report.phase_elapsed_ms.insert(
                "textures_aliases_and_reference_repair".into(),
                textures_started.elapsed().as_millis(),
            );
            let scripts_started = Instant::now();
            batch
                .convert_kind(
                    &vfs_files,
                    &source_paths,
                    "pex",
                    ProgressStage::Scripts,
                    None,
                    &BTreeSet::new(),
                )
                .await?;
            batch
                .report
                .phase_elapsed_ms
                .insert("scripts".into(), scripts_started.elapsed().as_millis());
        }
        if config.mo2.is_some() && !config.no_lod && !plugins.is_empty() {
            stage_mo2_lod_inputs(&staging.join("vfs"), &resolved_files)?;
        }
        let lod_started = Instant::now();
        compile_lod_chunks_with_cancel(
            config,
            staging,
            &config.output_dir,
            &plugins,
            &plugin_hashes,
            progress_tx,
            cancellation,
            &mut report,
            Some(lod_ownership),
        )
        .await?;
        report.lod_elapsed_ms = lod_started.elapsed().as_millis();
        interrupt(cancellation)?;
        let terrain_started = Instant::now();
        // Merged terrain (#215): the arrays, one mesh per exterior cell and the chunk files,
        // once the world database and every converted texture are in staging. Off by default
        // until the engine half that reads these outputs lands.
        if config.merged_terrain {
            crate::terrain_pipeline::run_merged_terrain_stage(
                staging,
                merged_terrain_records.take(),
                std::mem::take(&mut merged_terrain_offsets),
                &plugins,
                &crate::terrain_pipeline::TerrainOptions::from_config(config),
                progress_tx,
                &mut report,
            )
            .await?;
        }
        report.terrain_elapsed_ms = terrain_started.elapsed().as_millis();
        let integration_started = Instant::now();
        let integration_sources = source_paths
            .iter()
            .map(|(path, source)| {
                (
                    path.strip_prefix(staging.join("vfs"))
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                        .to_ascii_lowercase(),
                    source.clone(),
                )
            })
            .collect();
        if let Some(integration) =
            finalize_world_database_with_sources(staging, &integration_sources)?
        {
            if !integration.passed {
                report.warnings.push(format!(
                    "asset integration failed: {} missing models, {} invalid models, {} missing textures, terrain/cache cells {}/{}",
                    integration.missing_model_count,
                    integration.invalid_model_count,
                    integration.missing_texture_count,
                    integration.terrain_cells,
                    integration.cache_cells,
                ));
            }
            report_projection_omissions(&mut report, &integration.projection_omissions);
            report.integration = Some(integration);
            report
                .artifacts
                .push(PathBuf::from("integration-report.json"));
        }
        report.phase_elapsed_ms.insert(
            "database_asset_integration".into(),
            integration_started.elapsed().as_millis(),
        );
        let runtime_support_started = Instant::now();
        let runtime_path = staging.join("scripts/papyrus_runtime.luau");
        if let Some(parent) = runtime_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // The output tree holds hard links (a `vfs` entry and its cache blob, a runtime texture
        // and its sRGB alias), so every writer replaces its path instead of writing through it.
        if runtime_path.exists() {
            fs::remove_file(&runtime_path)?;
        }
        fs::write(
            &runtime_path,
            include_str!("../../shared/src/papyrus_runtime.luau"),
        )?;
        report
            .artifacts
            .push(PathBuf::from("scripts/papyrus_runtime.luau"));
        report.phase_elapsed_ms.insert(
            "runtime_support".into(),
            runtime_support_started.elapsed().as_millis(),
        );
        let artifact_validation_started = Instant::now();
        send(
            progress_tx,
            ProgressStage::Validating,
            0,
            report.artifacts.len() as u64,
            None,
            "Validating generated artifacts",
        )
        .await;
        validate_artifacts(
            staging,
            &report.artifacts,
            &texture_semantics,
            config.cpu_jobs,
        )?;
        report.phase_elapsed_ms.insert(
            "artifact_validation".into(),
            artifact_validation_started.elapsed().as_millis(),
        );
        send(
            progress_tx,
            ProgressStage::Validating,
            report.artifacts.len() as u64,
            report.artifacts.len() as u64,
            None,
            "Generated artifacts are valid",
        )
        .await;
        interrupt(cancellation)?;
        let manifest_save_started = Instant::now();
        report.inputs_by_kind = manifest.inputs_by_kind.clone();
        // The new pack starts without capability declarations, but the pack it replaces may have
        // carried some. Offer each to the refresh: one whose payload is still valid in staging is
        // kept and its files are published with the pack; every other one is dropped with a
        // warning that is part of the report, so the completeness below accounts for it.
        manifest.extensions = previous.extensions.clone();
        report
            .warnings
            .extend(crate::extensions::refresh_manifest_with_notices(
                &mut manifest,
                staging,
            )?);
        for payload in crate::extensions::retained_payload_paths(&manifest, staging)? {
            if !report.artifacts.contains(&payload) {
                report.artifacts.push(payload);
            }
        }
        manifest.complete = conversion_is_complete(&report);
        report.complete = manifest.complete;
        manifest.save(&staging.join("conversion-manifest.json"))?;
        report
            .artifacts
            .push(PathBuf::from("conversion-manifest.json"));
        report.phase_elapsed_ms.insert(
            "manifest_serialization".into(),
            manifest_save_started.elapsed().as_millis(),
        );
        Ok(report)
    }
}

/// The capability declarations of the published pack, for a run that ignores the rest of its
/// manifest. An unreadable manifest is logged and treated as declaring nothing, because
/// `--invalidate-cache` is also the way to recover from one.
fn published_extensions(output: &Path) -> Vec<shared::extensions::ExtensionEntry> {
    let path = output.join("conversion-manifest.json");
    match ConversionManifest::load(&path) {
        Ok(manifest) => manifest.extensions,
        Err(error) => {
            log::warn!(
                "cannot read the capabilities declared by {}: {error:#}",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Keep raw archive candidates and select converted outputs using their original producer.
/// Matching a retained metadata hash never upgrades the producer of the retained bytes.
fn reusable_manifest(
    config: &PipelineConfig,
    mut manifest: ConversionManifest,
) -> Result<ConversionManifest> {
    // Verified archive ingestion survives derived-output invalidation; only derived entries are gated.
    let archives = std::mem::take(&mut manifest.archives);
    let producer = manifest
        .retained_mesh_schema_version
        .unwrap_or(manifest.schema_version);
    let unproven_retention = manifest.retained_mesh_schema_version.is_none()
        && config.output_dir.join("metadata-rebuild.json").exists();
    let provenance_is_consistent = producer <= manifest.schema_version && !unproven_retention;
    let mut compatible = BTreeMap::new();
    for kind in ["nif", "dds", "pex"] {
        let producer_is_compatible = match kind {
            "nif" => can_reuse_meshes(producer),
            "dds" => producer == CONVERTER_SCHEMA_VERSION,
            "pex" => {
                producer == CONVERTER_SCHEMA_VERSION || can_reuse_scripts_and_archives(producer)
            }
            _ => unreachable!(),
        };
        let settings_match = if let Some(recorded) = manifest.asset_configuration_hashes.get(kind) {
            recorded == &asset_configuration_hash_for_schema(config, producer, kind)?
        } else {
            // Older manifests have one combined proof. Require an exact known
            // historical settings hash; an opaque stale schema cannot prove it.
            let recorded = manifest
                .retained_asset_configuration_hash
                .as_deref()
                .unwrap_or(&manifest.configuration_hash);
            let mut settings = config.clone();
            let mut matched = false;
            for reader in [
                crate::config::RecordReader::Legacy,
                crate::config::RecordReader::Inhouse,
            ] {
                settings.record_reader = reader;
                matched |= recorded == current_configuration_hash_for_schema(&settings, producer)?;
                matched |= recorded == configuration_hash_for_schema(&settings, producer)?;
                if reader == crate::config::RecordReader::Inhouse {
                    for identity in [
                        crate::cache::inhouse_adapter10_reader_identity(
                            settings.schemas()?.as_ref(),
                        ),
                        crate::cache::inhouse_adapter11_reader_identity(
                            settings.schemas()?.as_ref(),
                        ),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        matched |= crate::cache::retained_configuration_matches_for_reader(
                            &settings, producer, recorded, &identity,
                        )?;
                    }
                }
                if reader == crate::config::RecordReader::Legacy {
                    matched |= recorded
                        == crate::cache::legacy_projection_v1_configuration_hash(
                            &settings, producer,
                        )?;
                    matched |= recorded
                        == crate::cache::legacy_projection_v2_configuration_hash(
                            &settings, producer,
                        )?;
                    if crate::esm::inhouse::is_english(&settings.language) {
                        matched |= recorded
                            == crate::cache::legacy_projection_v3_configuration_hash(
                                &settings, producer,
                            )?;
                    }
                }
            }
            // Combined metadata proofs cannot name fixed-producer mesh contracts.
            matched && (kind != "nif" || !crate::cache::mesh_output_contracts_apply(producer))
        };
        compatible.insert(
            kind,
            provenance_is_consistent && producer_is_compatible && settings_match,
        );
    }
    manifest.entries.retain(|_, entry| {
        let kind = match Path::new(&entry.output)
            .extension()
            .and_then(|ext| ext.to_str())
        {
            Some(ext) if ext.eq_ignore_ascii_case("glb") => "nif",
            Some(ext) if ext.eq_ignore_ascii_case("ktx2") => "dds",
            Some(ext) if ext.eq_ignore_ascii_case("luau") => "pex",
            _ => return false,
        };
        compatible[kind]
    });
    let outputs: BTreeSet<_> = manifest
        .entries
        .values()
        .map(|entry| entry.output.clone())
        .collect();
    manifest
        .pruned_texture_references
        .retain(|output, _| outputs.contains(output));
    // Archive extraction checks source hashes and cached bytes independently.
    // It remains available even when every generated-output proof was rejected.
    manifest.archives = archives;
    Ok(manifest)
}

struct ConversionBatch<'a> {
    config: &'a PipelineConfig,
    staging: &'a Path,
    previous: &'a ConversionManifest,
    /// Provenance of the outputs already in `staging`, keyed by canonical
    /// source key; empty when the cache is invalidated.
    staged: Arc<BTreeMap<String, StagedOutput>>,
    journal: &'a mut StagingJournal,
    manifest: &'a mut ConversionManifest,
    report: &'a mut PipelineReport,
    progress_tx: &'a Sender<ProgressEvent>,
    cancellation: &'a Cancellation,
}

/// How a worker produced a conversion output.
enum Produced {
    /// A cached output was reused.
    CacheHit,
    /// Freshly converted; `digest` is the output's (size, SHA-256) when the
    /// converter already computed it, so it need not be read back.
    Converted { digest: Option<(u64, String)> },
}

struct GpuTag {
    index: usize,
    key: String,
    hash: String,
    target_rel: PathBuf,
    relative: PathBuf,
    source: PathBuf,
    target: PathBuf,
    encoding: TextureEncoding,
}

impl ConversionBatch<'_> {
    /// Converts every file of one kind (`dds`, `nif` or `pex`) on the worker pool, reusing cached
    /// and staged outputs, and records each result in the manifest, the journal and the report.
    /// Textures the GPU encoder takes are batched to it instead of being encoded on a worker.
    /// Scoped mesh repair hashes use source-texture presence from the full VFS snapshot.
    async fn convert_kind(
        &mut self,
        files: &[PathBuf],
        source_paths: &BTreeMap<PathBuf, PathBuf>,
        source_ext: &str,
        stage: ProgressStage,
        texture_semantics: Option<&BTreeMap<String, BTreeSet<TextureSemantic>>>,
        force_reconvert: &BTreeSet<String>,
    ) -> Result<()> {
        let selected_paths: Vec<_> = files
            .iter()
            .filter(|path| extension(path, &[source_ext]))
            .cloned()
            .collect();

        let (target_ext, asset_kind) = match source_ext {
            "dds" => ("ktx2", AssetKind::Texture),
            "nif" => ("glb", AssetKind::Mesh),
            "pex" => ("luau", AssetKind::Script),
            _ => unreachable!(),
        };
        let staging_vfs = self.staging.join("vfs");
        let mut target_sources = BTreeMap::<String, PathBuf>::new();
        let mut selected = Vec::with_capacity(selected_paths.len());
        // The source sizes are one `stat` each and give the status line something to weigh the
        // stage by: a mesh and a texture are the same "item" but not the same work.
        let mut source_sizes = BTreeMap::<String, u64>::new();
        let mut bytes_total = 0u64;
        for source in selected_paths {
            let relative = source.strip_prefix(&staging_vfs)?.to_owned();
            let target_key =
                canonical_asset_path(&relative.to_string_lossy(), asset_kind, target_ext)?;
            if let Some(previous) = target_sources.insert(target_key.clone(), relative.clone()) {
                bail!(
                    "normalized output collision for {target_key}: {} and {}",
                    previous.display(),
                    relative.display()
                );
            }
            let source_key =
                canonical_asset_path(&relative.to_string_lossy(), asset_kind, source_ext)?;
            let encoding = if source_ext == "dds" {
                let known_semantics = texture_semantics
                    .and_then(|semantics| semantics.get(&target_key))
                    .cloned()
                    .unwrap_or_default();
                Some(TextureEncoding::from_semantics(&known_semantics)?)
            } else {
                None
            };
            let source = source_paths.get(&source).unwrap_or(&source).clone();
            let source_bytes = fs::metadata(&source).map_or(0, |metadata| metadata.len());
            bytes_total += source_bytes;
            source_sizes.insert(source_key.clone(), source_bytes);
            selected.push((
                source,
                relative,
                PathBuf::from(target_key),
                source_key,
                encoding,
            ));
        }

        self.manifest
            .inputs_by_kind
            .insert(source_ext.to_owned(), selected.len() as u64);

        if selected.is_empty() {
            return Ok(());
        }
        interrupt(self.cancellation)?;

        let total_files = selected.len() as u64;
        let progress_tx = self.progress_tx.clone();
        let (outcome_tx, mut outcome_rx) = unbounded_channel();

        let staging_root = self.staging.to_path_buf();
        let output_dir = self.config.output_dir.clone();
        let source_kind = source_ext.to_owned();
        let mesh_sources = source_paths
            .iter()
            .map(|(path, source)| {
                Ok((
                    path.strip_prefix(&staging_vfs)?.to_path_buf(),
                    source.clone(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        // Parsing every skeleton once per run keeps the per-mesh resolution
        // cheap; only the directory-union cache inside needs a lock.
        let skeleton_index =
            (source_kind == "nif").then(|| crate::mesh::SkeletonIndex::from_sources(&mesh_sources));
        let source_textures = texture_source_keys(self.staging, files);
        let etc1s_quality = self.config.texture_fallback_quality;
        let uastc_level = self.config.texture_uastc_level;
        let zstd_level = self.config.texture_zstd_level;
        let texture_encoder = self.config.texture_encoder;
        let cpu_jobs = self.config.cpu_jobs;
        let previous_entries = self.previous.entries.clone();
        let staged_outputs = Arc::clone(&self.staged);
        let expected_configuration =
            asset_configuration_hash_for_schema(self.config, CONVERTER_SCHEMA_VERSION, source_ext)?;
        let worker_configuration = expected_configuration.clone();
        let force_reconvert = force_reconvert.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let run_cancelled = self.cancellation.clone();

        let rayon_handle = spawn_blocking(move || -> Result<()> {
            use rayon::prelude::*;

            let gpu = if source_kind == "dds" {
                if let TextureEncoder::Gpu { quality, batch_mb } = texture_encoder {
                    match GpuUastc::new(quality, batch_mb) {
                        Ok(mut gpu) => {
                            gpu.zstd_level = zstd_level;
                            // The batch size is capped to what the GPU's buffers allow.
                            eprintln!(
                                "GPU texture encoder: {} (quality {quality}, batch {} MiB)",
                                gpu.adapter_name,
                                gpu.batch_bytes >> 20
                            );
                            Some(gpu)
                        }
                        Err(error) => {
                            eprintln!(
                                "GPU texture encoder unavailable ({error:#}); using CPU encoder"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            // Cache label of GPU-encoded textures (see the hash below).
            let gpu_label = gpu
                .as_ref()
                .map(|gpu| texture_gpu::cache_label(gpu.quality));
            let (gpu_sender, gpu_thread) = if let Some(gpu) = gpu {
                let (sender, receiver) = texture_gpu::job_channel::<GpuTag>(&gpu);
                let gpu_outcomes = outcome_tx.clone();
                let gpu_label = gpu_label.clone().unwrap_or_default();
                let gpu_cancelled = Arc::clone(&worker_cancelled);
                let gpu_run_cancelled = run_cancelled.clone();
                // Like the workers, the GPU stops taking textures once the
                // batch or the run is stopped.
                let stopped = move || {
                    gpu_cancelled.load(Ordering::Relaxed) || gpu_run_cancelled.is_cancelled()
                };
                let handle = std::thread::spawn(move || {
                    // Called on the batcher's writer threads, so the writes
                    // run in parallel.
                    let finish = |tag: GpuTag, result: Result<texture_gpu::EncodedTexture>| {
                        let GpuTag {
                            index,
                            key,
                            hash,
                            target_rel,
                            relative,
                            source,
                            target,
                            encoding,
                        } = tag;
                        let written = result.and_then(|encoded| {
                            publish_ktx2_file(&target, &encoded.bytes)?;
                            Ok((encoded.bytes.len() as u64, encoded.sha256))
                        });
                        let (hash, result, fatal) = match written {
                            Ok(digest) => (
                                hash,
                                Ok(Produced::Converted {
                                    digest: Some(digest),
                                }),
                                false,
                            ),
                            Err(error) => {
                                eprintln!(
                                    "GPU texture conversion failed for {} ({error:#}); using CPU encoder",
                                    relative.display()
                                );
                                // A CPU-encoded fallback does not carry the GPU
                                // label, so the next GPU run retries the GPU.
                                let (result, fatal) = match remove_invalid_staged_output(&target) {
                                    Err(error) => (Err(error), true),
                                    Ok(()) => remove_partial_output_after_failure(
                                        &target,
                                        TextureConverter::convert_dds_to_ktx2_with_options(
                                            &source,
                                            &target,
                                            encoding,
                                            etc1s_quality,
                                            uastc_level,
                                            zstd_level,
                                        )
                                        .map(|_| ()),
                                    ),
                                };
                                (
                                    hash.replace(&gpu_label, ""),
                                    result.map(|_| Produced::Converted { digest: None }),
                                    fatal,
                                )
                            }
                        };
                        let result = result
                            .wrap_err_with(|| format!("failed to convert {}", relative.display()));
                        let _ = gpu_outcomes.send((
                            index, key, hash, target_rel, relative, result, target, fatal,
                        ));
                    };
                    let stats = texture_gpu::run_batcher(&gpu, receiver, cpu_jobs, stopped, finish);
                    eprintln!(
                        "GPU texture encoder: {} textures in {} batches, {:.1} GB uploaded",
                        stats.textures,
                        stats.batches,
                        stats.source_bytes as f64 / 1e9
                    );
                });
                (Some(sender), Some(handle))
            } else {
                (None, None)
            };

            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(cpu_jobs)
                .build()
                .wrap_err("failed to create asset conversion worker pool")?;
            pool.install(|| {
                selected.into_par_iter().enumerate().for_each(
                    |(index, (source, relative, target_rel, key, encoding))| {
                        // An interrupted run stops before starting another asset; the one already
                        // in flight finishes, so no half-written output is left behind.
                        if worker_cancelled.load(Ordering::Relaxed) || run_cancelled.is_cancelled()
                        {
                            return;
                        }
                        // Retry a mesh whose pruned source or scoped fallback is back. General
                        // mesh hashes omit texture bytes, and the volcanic presence bits alone
                        // cannot detect a failed fallback DDS repaired in place.
                        let forced =
                            force_reconvert.contains(target_rel.to_string_lossy().as_ref());
                        let target = staging_root.join(&target_rel);

                        // Dependencies come from the merged winners below, not physical neighbors.
                        let source_hash = hash_file(&source);
                        let mut hash = match source_hash {
                            Ok(h) => h,
                            Err(err) => {
                                let (err, fatal) = match remove_invalid_staged_output(&target) {
                                    Ok(()) => (err, false),
                                    Err(remove_error) => (remove_error, true),
                                };
                                let _ = outcome_tx.send((
                                    index,
                                    key,
                                    String::new(),
                                    target_rel,
                                    relative.clone(),
                                    Err(err),
                                    target,
                                    fatal,
                                ));
                                return;
                            }
                        };

                        if let Some(encoding) = encoding {
                            hash.push_str(&format!(":texture-encoding:{encoding:?}"));
                            // Marks the textures the GPU encodes. Natively preserved
                            // textures and those only the CPU encoder handles
                            // (volumes, arrays) convert the same way in either
                            // mode and keep one cache entry.
                            if gpu_sender.is_some()
                                && let Some(label) = &gpu_label
                                && texture_gpu::takes(&source, encoding)
                            {
                                hash.push_str(label);
                            }
                        }

                        let mut unresolved_skin = None;
                        let skeleton = if source_kind == "nif" {
                            let resolution = crate::mesh::resolve_skeleton_with_index(
                                &relative,
                                &mesh_sources,
                                skeleton_index
                                    .as_ref()
                                    .expect("a nif batch always builds a skeleton index"),
                            );
                            match resolution {
                                crate::mesh::SkeletonResolution::Skeleton(key) => skeleton_index
                                    .as_ref()
                                    .and_then(|index| index.source_for(&key))
                                    .map(Path::to_path_buf)
                                    .or(Some(key)),
                                crate::mesh::SkeletonResolution::SelfContained => None,
                                crate::mesh::SkeletonResolution::Unresolved { best_coverage } => {
                                    hash.push_str(crate::mesh::UNRESOLVED_SKIN_HASH_SUFFIX);
                                    unresolved_skin =
                                        Some(crate::mesh::unresolved_skeleton_diagnostic(
                                            format_args!("NIF {}", relative.display()),
                                            best_coverage,
                                        ));
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        if source_kind == "nif"
                            && let Some(dependency) = &skeleton
                        {
                            match hash_file(dependency) {
                                Ok(dep_hash) => {
                                    hash.push(':');
                                    hash.push_str(&dep_hash);
                                }
                                Err(err) => {
                                    let (err, fatal) = match remove_invalid_staged_output(&target) {
                                        Ok(()) => (err, false),
                                        Err(remove_error) => (remove_error, true),
                                    };
                                    let _ = outcome_tx.send((
                                        index,
                                        key,
                                        hash,
                                        target_rel,
                                        relative.clone(),
                                        Err(err),
                                        target,
                                        fatal,
                                    ));
                                    return;
                                }
                            }
                        }

                        if source_kind == "nif"
                            && let Some(suffix) =
                                crate::mesh::volcanic_normal_cache_suffix(&key, &source_textures)
                        {
                            hash.push_str(&suffix);
                        }

                        // Check cache
                        if !forced
                            && let Some(entry) =
                                previous_entries.get(&key).filter(|e| e.source_hash == hash)
                        {
                            let old = output_dir.join(&entry.output);
                            if old.is_file()
                                && fs::metadata(&old).is_ok_and(|m| m.len() == entry.output_size)
                                && hash_file(&old).is_ok_and(|h| h == entry.output_hash)
                            {
                                if let Some(parent) = target.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                // `link_or_copy`, not `fs::copy`: on a resumed
                                // run the staged target may already be a hard
                                // link to the pack file, and copying a file
                                // onto its own link truncates both. A shared
                                // inode is already the cached bytes.
                                if link_or_copy(&old, &target).is_ok() {
                                    let _ = outcome_tx.send((
                                        index,
                                        key,
                                        hash,
                                        target_rel,
                                        relative.clone(),
                                        Ok(Produced::CacheHit),
                                        target,
                                        false,
                                    ));
                                    return;
                                };
                            }
                        }

                        // A staged output survives from an earlier run, so it
                        // is reused only when the journal says it was produced
                        // from the current source under the current schema and
                        // asset-kind settings, and its bytes still match the
                        // recorded size and hash, or the `previous_entries` fallback
                        // finds the previous manifest's entry for the same
                        // source and its recorded output hash matches the
                        // staged bytes (a staging tree copied from a published
                        // pack carries no journal). Any other output is
                        // converted again.
                        let staged_is_current = !forced
                            && (staged_outputs.get(&key).is_some_and(|record| {
                                record.is_current(&target, &hash, &worker_configuration)
                            }) || previous_entries.get(&key).is_some_and(|entry| {
                                entry.source_hash == hash
                                    && entry.output_hash == hash_file(&target).unwrap_or_default()
                            }));
                        let existing_is_valid = staged_is_current
                            && fs::metadata(&target).is_ok_and(|metadata| metadata.len() > 0)
                            && match source_kind.as_str() {
                                "dds" => fs::read(&target).is_ok_and(|bytes| {
                                    crate::texture::inspect_ktx2(
                                        &bytes,
                                        encoding.expect("DDS conversion requires an encoding"),
                                    )
                                    .is_ok()
                                }),
                                "nif" | "pex" => true,
                                _ => false,
                            };

                        if !existing_is_valid
                            && source_kind == "dds"
                            && let Some(sender) = &gpu_sender
                            && let Ok(bytes) = fs::read(&source)
                            && let Some(encoding) = encoding
                            && let Ok(texture) = PreparedTexture::from_dds(bytes, encoding)
                        {
                            if let Err(error) = remove_invalid_staged_output(&target) {
                                let _ = outcome_tx.send((
                                    index,
                                    key,
                                    hash,
                                    target_rel,
                                    relative,
                                    Err(error),
                                    target,
                                    true,
                                ));
                                return;
                            }
                            let tag = GpuTag {
                                index,
                                key: key.clone(),
                                hash: hash.clone(),
                                target_rel: target_rel.clone(),
                                relative: relative.clone(),
                                source: source.clone(),
                                target: target.clone(),
                                encoding,
                            };
                            if sender
                                .send(GpuJob {
                                    texture,
                                    encoding: tag.encoding,
                                    tag,
                                })
                                .is_ok()
                            {
                                return;
                            }
                        }
                        // A labelled texture the GPU path declined after all
                        // (undecodable pixels, a stopped GPU thread) is encoded
                        // on the CPU below; its cache entry must say so.
                        if !existing_is_valid
                            && let Some(label) = &gpu_label
                            && hash.ends_with(label.as_str())
                        {
                            hash.truncate(hash.len() - label.len());
                        }

                        let (result, fatal) = if existing_is_valid {
                            (Ok(()), false)
                        } else {
                            match remove_invalid_staged_output(&target) {
                                Err(error) => (Err(error), true),
                                Ok(()) => {
                                    let conversion = match source_kind.as_str() {
                                        "dds" => {
                                            TextureConverter::convert_dds_to_ktx2_with_options(
                                                &source,
                                                &target,
                                                encoding
                                                    .expect("DDS conversion requires an encoding"),
                                                etc1s_quality,
                                                uastc_level,
                                                zstd_level,
                                            )
                                            .map(|_| ())
                                        }
                                        "nif" => {
                                            MeshConverter::convert_nif_to_glb_with_skin_resolution(
                                                &source,
                                                &target,
                                                skeleton.as_deref(),
                                                unresolved_skin.as_deref(),
                                            )
                                        }
                                        "pex" => {
                                            ScriptConverter::convert_pex_to_luau(&source, &target)
                                        }
                                        _ => unreachable!(),
                                    };
                                    remove_partial_output_after_failure(&target, conversion)
                                }
                            }
                        };

                        let result = result
                            .map(|_| Produced::Converted { digest: None })
                            .wrap_err_with(|| format!("failed to convert {}", relative.display()));
                        let _ = outcome_tx.send((
                            index,
                            key,
                            hash,
                            target_rel,
                            relative.to_path_buf(),
                            result,
                            target,
                            fatal,
                        ));
                    },
                );
            });
            drop(gpu_sender);
            if let Some(handle) = gpu_thread {
                handle
                    .join()
                    .map_err(|_| color_eyre::eyre::eyre!("GPU texture worker panicked"))?;
            }
            Ok(())
        });

        let mut completed = 0u64;
        let mut first_error = None;
        let fail_fast = self.config.fail_fast;
        let mut bytes_completed = 0u64;
        while let Some((_, key, hash, target_rel, relative, conversion, target, fatal)) =
            outcome_rx.recv().await
        {
            completed += 1;
            bytes_completed += source_sizes.get(&key).copied().unwrap_or(0);
            if self.cancellation.is_cancelled() {
                break;
            }

            match conversion {
                Ok(produced) => {
                    // Without fail-fast only a journal write sets the first
                    // error, and after one nothing more can be recorded.
                    if first_error.is_some() {
                        continue;
                    }
                    let (is_cache_hit, known_digest) = match produced {
                        Produced::CacheHit => (true, None),
                        Produced::Converted { digest } => (false, digest),
                    };
                    if !is_cache_hit {
                        let known_size = known_digest.as_ref().map(|(size, _)| *size);
                        let size = match known_size
                            .map(Ok)
                            .unwrap_or_else(|| fs::metadata(&target).map(|metadata| metadata.len()))
                        {
                            Ok(size) => size,
                            Err(error) => {
                                if fail_fast {
                                    return Err(error).wrap_err_with(|| {
                                        format!("failed to convert {}", relative.display())
                                    });
                                }
                                self.record_skip(
                                    stage,
                                    completed,
                                    total_files,
                                    key,
                                    relative,
                                    error.into(),
                                )
                                .await;
                                continue;
                            }
                        };
                        let output_hash = match known_digest
                            .map(|(_, sha256)| Ok(sha256))
                            .unwrap_or_else(|| hash_file(&target))
                        {
                            Ok(output_hash) => output_hash,
                            Err(error) => {
                                if fail_fast {
                                    return Err(error).wrap_err_with(|| {
                                        format!("failed to convert {}", relative.display())
                                    });
                                }
                                self.record_skip(
                                    stage,
                                    completed,
                                    total_files,
                                    key,
                                    relative,
                                    error,
                                )
                                .await;
                                continue;
                            }
                        };
                        send_with_bytes(
                            &progress_tx,
                            stage,
                            completed,
                            total_files,
                            Some(relative.clone()),
                            "Converted asset",
                            (bytes_completed, bytes_total),
                        )
                        .await;
                        let entry = CacheEntry {
                            source_hash: hash,
                            output: target_rel.to_string_lossy().into_owned().replace('\\', "/"),
                            output_size: size,
                            output_hash,
                        };
                        if let Err(error) = self
                            .journal
                            .record(&key, &staged_output(&entry, &expected_configuration))
                        {
                            stop_batch(&cancelled, &mut first_error, error);
                            continue;
                        }
                        self.manifest.entries.insert(key, entry);
                        self.report.converted += 1;
                    } else {
                        send_with_bytes(
                            &progress_tx,
                            stage,
                            completed,
                            total_files,
                            Some(relative.clone()),
                            "Converted asset",
                            (bytes_completed, bytes_total),
                        )
                        .await;
                        if let Some(entry) = self.previous.entries.get(&key).cloned() {
                            // The staged copy holds the published bytes, so the
                            // published entry is its provenance.
                            if let Err(error) = self
                                .journal
                                .record(&key, &staged_output(&entry, &expected_configuration))
                            {
                                stop_batch(&cancelled, &mut first_error, error);
                                continue;
                            }
                            self.manifest.entries.insert(key, entry);
                        }
                        self.report.cache_hits += 1;
                    }
                    self.report.artifacts.push(target_rel);
                }
                Err(error) => {
                    if fail_fast || fatal {
                        if first_error.is_none() {
                            cancelled.store(true, Ordering::Relaxed);
                            let _ = progress_tx
                                .send(
                                    ProgressEvent::new(
                                        stage,
                                        completed,
                                        total_files,
                                        Some(relative),
                                        "Asset conversion failed",
                                    )
                                    .with_outcome(AssetOutcome::Failed),
                                )
                                .await;
                            first_error = Some(error);
                        }
                    } else {
                        self.record_skip(stage, completed, total_files, key, relative, error)
                            .await;
                    }
                }
            }
        }

        rayon_handle
            .await
            .wrap_err("rayon batch worker panicked")??;
        interrupt(self.cancellation)?;
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn record_skip(
        &mut self,
        stage: ProgressStage,
        completed: u64,
        total: u64,
        key: String,
        relative: PathBuf,
        error: color_eyre::eyre::Error,
    ) {
        let _ = self
            .progress_tx
            .send(
                ProgressEvent::new(
                    stage,
                    completed,
                    total,
                    Some(relative.clone()),
                    "Asset skipped",
                )
                .with_outcome(AssetOutcome::Skipped),
            )
            .await;
        let message = format!("{}: {error:#}", relative.display());
        self.manifest.failures.insert(key, message.clone());
        self.report.warnings.push(message);
        self.report.skipped += 1;
    }

    /// Records a texture reference a published mesh omits because the game data
    /// does not contain that texture, whether this run pruned it or an earlier
    /// run did and the mesh was reused. Returns whether the reference is new, so
    /// the caller reports progress only for work this run performed.
    ///
    /// A prune is loud like a skip - the progress stream and the run summary name
    /// the mesh - but it is not a skip: no warning is recorded and nothing lands
    /// in `manifest.failures`, so the conversion stays complete.
    fn record_pruned_texture_reference(&mut self, glb: &str, reference: &str) -> bool {
        self.manifest
            .pruned_texture_references
            .entry(glb.to_owned())
            .or_default()
            .insert(reference.to_owned())
    }
}

/// Hashes winning raw inputs under their canonical logical VFS source paths.
/// Called inside optional producer attempts so unreadable inputs become notices.
fn derived_input_hashes<const N: usize>(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    logical_inputs: [PathBuf; N],
) -> Result<BTreeMap<String, String>> {
    logical_inputs
        .into_iter()
        .map(|logical| {
            let key = logical
                .strip_prefix(staging.join("vfs"))?
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            let winner = sources.get(&logical).unwrap_or(&logical);
            Ok((key, hash_file(winner)?))
        })
        .collect()
}

/// Captures a successful recipe's bytes; finalization refreshes postprocessed outputs.
fn derived_output_proof(
    recipe: &str,
    inputs: BTreeMap<String, String>,
    output: &Path,
) -> Result<DerivedOutput> {
    Ok(DerivedOutput {
        recipe: recipe.to_owned(),
        inputs,
        output_size: fs::metadata(output)?.len(),
        output_hash: hash_file(output)?,
    })
}

/// Hashes effective skin choices and the optional thin preparation's winning bytes.
/// A missing thin part skips all thin preparation, matching the composition path.
fn body_compose_context_hash(
    parts: &[BodyPart<'_>],
    thin_paths: &[(PathBuf, BodyPartKind)],
    skins: &[compose::DefaultSkinTextureSet],
) -> Result<String> {
    let heavy: Vec<_> = parts
        .iter()
        .map(|part| {
            (
                part.kind.slot(),
                part.skin_textures.map(|set| (set.form_id, &set.slots)),
            )
        })
        .collect();
    let mut thin = Vec::new();
    if thin_paths.iter().all(|(path, _)| path.is_file()) {
        for (path, kind) in thin_paths {
            let skin = compose::skin_texture_set_for(skins, *kind, path);
            thin.push((
                kind.slot(),
                hash_file(path)?,
                skin.map(|set| (set.form_id, &set.slots)),
            ));
        }
    }
    Ok(hash_bytes(&serde_json::to_vec(&(heavy, thin))?))
}

/// Binds derived proofs to final bytes after material repair and texture pruning.
/// An unreadable optional output loses its proof and artifact instead of aborting neighbors.
fn finalize_derived_outputs(
    staging: &Path,
    report: &mut PipelineReport,
    manifest: &mut ConversionManifest,
) -> Result<()> {
    let pending = std::mem::take(&mut manifest.derived_outputs);
    for (key, proof) in pending {
        let output = staging.join(&key);
        match derived_output_proof(&proof.recipe, proof.inputs, &output) {
            Ok(proof) => {
                manifest.derived_outputs.insert(key, proof);
            }
            Err(error) => {
                match fs::remove_file(&output) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error)
                            .wrap_err_with(|| format!("failed to remove {}", output.display()));
                    }
                    _ => {}
                }
                report.artifacts.retain(|path| path != Path::new(&key));
                manifest.pruned_texture_references.remove(&key);
                let message =
                    format!("{key}: failed to finalize optional derived output: {error:#}");
                report.notices.push(message.clone());
                warn!("{message}");
            }
        }
    }
    Ok(())
}

/// Composes the default actor bodies from the character part NIFs.
///
/// Runs after the mesh stage and before texture dependency collection so the
/// composed GLBs' texture references take part in the texture stage. Every input is
/// resolved through the winning source map first, so an MO2 loose override is composed
/// from the same file the mesh stage converted; the staged vfs copy is the fallback for
/// physical Data and for sources without a loose winner. A load order missing any part
/// leaves the body for that sex uncomposed rather than failing the conversion.
fn compose_default_bodies(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    fail_fast: bool,
    report: &mut PipelineReport,
    manifest: &mut ConversionManifest,
) -> Result<()> {
    const PARTS: [(&str, BodyPartKind); 3] = [
        ("body_1.nif", BodyPartKind::Body),
        ("hands_1.nif", BodyPartKind::Hands),
        ("feet_1.nif", BodyPartKind::Feet),
    ];
    let assets = staging.join("vfs/meshes/actors/character/character assets");
    let winning = |logical: PathBuf| sources.get(&logical).cloned().unwrap_or(logical);
    let skeleton = winning(assets.join("skeleton.nif"));
    let database = staging.join("skyrim_world.db");
    for sex in ["male", "female"] {
        let relative = compose::composed_body_path("default", sex);
        let output = staging.join(&relative);
        let key = relative.to_string_lossy().replace('\\', "/");
        // Derived bodies are optional outputs, not source conversion failures. Clear old
        // failure markers and proofs before deciding whether this run can publish one.
        manifest.failures.remove(&key);
        manifest.derived_outputs.remove(&key);
        // A body skipped or failed on this run must not survive from an earlier one: orphan
        // sweeps keep composed outputs, and the caller reports whatever is on disk.
        // Successful attempts retain unchanged output so the writer can avoid republishing it.
        let remove_output = || -> Result<()> {
            match fs::remove_file(&output) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error).wrap_err_with(|| format!("failed to remove {}", output.display()))
                }
                _ => Ok(()),
            }
        };
        match fs::symlink_metadata(&output) {
            Ok(metadata) if !metadata.file_type().is_file() => remove_output()?,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", output.display()));
            }
            _ => {}
        }
        let mut attempt = || -> Result<Option<(usize, DerivedOutput)>> {
            let part_paths =
                PARTS.map(|(suffix, kind)| (winning(assets.join(format!("{sex}{suffix}"))), kind));
            if !part_paths.iter().any(|(path, _)| path.is_file()) {
                return Ok(None);
            }
            if !skeleton.is_file() {
                warn!(
                    "skipping composed default {sex} actor body: missing {}",
                    skeleton.display()
                );
                return Ok(None);
            }
            if let Some((missing, _)) = part_paths.iter().find(|(path, _)| !path.is_file()) {
                warn!(
                    "skipping composed default {sex} actor body: missing {}",
                    missing.display()
                );
                return Ok(None);
            }
            // Apply sex and slot-matched DefaultRace skin sets to validated skin shapes.
            // Old catalogs or a missing database keep each NIF's original textures.
            let skins = if database.is_file() {
                compose::default_skin_texture_sets(&database, sex).wrap_err_with(|| {
                    format!("failed to read the default {sex} armature skin texture sets")
                })?
            } else {
                Vec::new()
            };
            let parts: Vec<_> = part_paths
                .iter()
                .map(|(path, kind)| BodyPart {
                    path,
                    kind: *kind,
                    skin_textures: compose::skin_texture_set_for(&skins, *kind, path),
                })
                .collect();
            let mut inputs = derived_input_hashes(
                staging,
                sources,
                body_compose_inputs(sex).map(|path| staging.join("vfs").join(path)),
            )?;

            let thin_paths: Vec<(PathBuf, BodyPartKind)> = PARTS
                .iter()
                .map(|(suffix, kind)| {
                    let thin_name = suffix.replace("_1.nif", "_0.nif");
                    (winning(assets.join(format!("{sex}{thin_name}"))), *kind)
                })
                .collect();
            let mut model = compose::compose_body(&parts, &skeleton)?;
            attach_default_body_weight(&mut model, &thin_paths, &skeleton, &skins, sex, report)?;
            inputs.insert(
                BODY_COMPOSE_CONTEXT.into(),
                body_compose_context_hash(&parts, &thin_paths, &skins)?,
            );
            compose::write_composed_body(&model, &output)?;
            let proof = derived_output_proof(BODY_COMPOSE_RECIPE, inputs, &output)?;
            Ok(Some((parts.len(), proof)))
        };
        match attempt() {
            Ok(Some((part_count, proof))) => {
                manifest.derived_outputs.insert(key, proof);
                info!(
                    "composed default {sex} actor body from {part_count} parts to {}",
                    output.display()
                );
            }
            Ok(None) => remove_output()?,
            Err(error) if fail_fast => {
                remove_output()?;
                return Err(error)
                    .wrap_err_with(|| format!("failed to compose default {sex} actor body"));
            }
            Err(error) => {
                remove_output()?;
                let message =
                    format!("{key}: failed to compose default {sex} actor body: {error:#}");
                // Notices describe optional derived-output problems without making source
                // repair incomplete; manifest failures are reserved for repairable inputs.
                report.notices.push(message.clone());
                warn!("{message}");
            }
        }
    }
    Ok(())
}

/// Adds the weight-0 body as a `thin` morph target, or records why it cannot and leaves the
/// heavy body alone. A missing or differently shaped thin part never fails a conversion.
fn attach_default_body_weight(
    model: &mut compose::ComposedBody,
    thin_paths: &[(PathBuf, BodyPartKind)],
    skeleton: &Path,
    skins: &[compose::DefaultSkinTextureSet],
    sex: &str,
    report: &mut PipelineReport,
) -> Result<()> {
    let unavailable = |report: &mut PipelineReport, reason: String| {
        let message = format!("actor_body_weight_unavailable: {sex} body: {reason}");
        warn!("{message}");
        report.notices.push(message);
    };

    if let Some((missing, _)) = thin_paths.iter().find(|(path, _)| !path.is_file()) {
        unavailable(report, format!("thin part missing: {}", missing.display()));
        return Ok(());
    }
    let thin_parts: Vec<_> = thin_paths
        .iter()
        .map(|(path, kind)| BodyPart {
            path,
            kind: *kind,
            skin_textures: compose::skin_texture_set_for(skins, *kind, path),
        })
        .collect();
    let part_names = thin_paths
        .iter()
        .filter_map(|(path, _)| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let thin = compose::compose_body(&thin_parts, skeleton)
        .wrap_err_with(|| format!("failed to prepare thin {sex} body parts {part_names}"))?;
    match compose::attach_thin_target(model, &thin)? {
        compose::ThinTargetOutcome::Attached => {}
        compose::ThinTargetOutcome::Unavailable(reason) => {
            unavailable(report, format!("thin parts {part_names}: {reason}"));
        }
    }
    Ok(())
}

/// The gait clips every character ships, in emission order.
///
/// Each entry is the clip name followed by the source/output stem. Ordering is
/// deterministic so a resumed run, the artifact report, and the logs agree.
const DEFAULT_ANIMATION_CLIPS: [(&str, &str); 3] = [
    ("idle", "mt_idle"),
    ("walkforward", "mt_walkforward"),
    ("runforward", "mt_runforward"),
];

/// The canonical asset key of one sex's converted clip, also used for report text.
/// Native filesystem paths are formed only when this key is joined to staging.
fn default_animation_output(sex: &str, stem: &str) -> String {
    canonical_asset_path(
        &format!("meshes/actors/character/animations/{sex}/{stem}.glb"),
        AssetKind::Mesh,
        "glb",
    )
    .expect("built-in default gait names form valid mesh asset keys")
}

/// Rejects non-directory ancestors, including those Windows masks as a missing child.
fn validate_animation_output_parent(output: &Path) -> Result<()> {
    for parent in output.ancestors().skip(1) {
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                bail!(
                    "failed to inspect {}: parent {} is not a directory",
                    output.display(),
                    parent.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", output.display()));
            }
        }
    }
    Ok(())
}

/// Captures exactly the four raw dependencies used to bind and decode one gait.
fn default_animation_input_hashes(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    sex: &str,
    stem: &str,
) -> Result<BTreeMap<String, String>> {
    derived_input_hashes(
        staging,
        sources,
        default_gait_inputs(sex, stem).map(|path| staging.join("vfs").join(path)),
    )
}

/// Checks every built-in gait ancestor before a pass changes manifest markers or staged files.
fn validate_animation_output_parents(staging: &Path) -> Result<()> {
    for sex in ["male", "female"] {
        for (_, stem) in DEFAULT_ANIMATION_CLIPS {
            validate_animation_output_parent(&staging.join(default_animation_output(sex, stem)))?;
        }
    }
    Ok(())
}

/// Removes blocking non-regular paths at every derived gait output before the pass can skip or write.
fn clear_nonregular_animation_outputs(staging: &Path) -> Result<()> {
    for sex in ["male", "female"] {
        for (_, stem) in DEFAULT_ANIMATION_CLIPS {
            let output = staging.join(default_animation_output(sex, stem));
            validate_animation_output_parent(&output)?;
            match fs::symlink_metadata(&output) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(metadata) => {
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                        return Err(eyre!(
                            "refusing to remove directory at gait output {}",
                            output.display()
                        ));
                    }
                    fs::remove_file(&output)
                        .wrap_err_with(|| format!("failed to remove {}", output.display()))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("failed to inspect {}", output.display()));
                }
            }
        }
    }
    Ok(())
}

/// Decodes staged default gait clips and writes animation-only GLBs.
///
/// Builds the binding bone tree from the raw body NIF and skeleton NIF; clips
/// also consume skeleton HKX and clip HKX bytes, independently of composed GLBs.
/// Missing inputs and conversion errors are optional notices.
/// Each input resolves through the winning-source map, including MO2 overrides.
fn convert_default_animations(
    staging: &Path,
    sources: &BTreeMap<PathBuf, PathBuf>,
    fail_fast: bool,
    report: &mut PipelineReport,
    manifest: &mut ConversionManifest,
) -> Result<()> {
    let animations = staging.join("vfs/meshes/actors/character/animations");
    let assets = staging.join("vfs/meshes/actors/character/character assets");
    let winning = |logical: PathBuf| sources.get(&logical).cloned().unwrap_or(logical);
    let remove_stale = |output: &Path| -> Result<()> {
        match fs::remove_file(output) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).wrap_err_with(|| format!("failed to remove {}", output.display()))
            }
            _ => Ok(()),
        }
    };

    // Ancestors and exact outputs are rejected before any marker is removed, so a rejected pass
    // keeps its incoming failure and proof maps for both sexes.
    validate_animation_output_parents(staging)?;
    let prior_derived_relevance: BTreeMap<_, _> = ["male", "female"]
        .into_iter()
        .map(|sex| {
            let relevant = DEFAULT_ANIMATION_CLIPS.iter().any(|(_, stem)| {
                let key = default_animation_output(sex, stem);
                staging.join(&key).is_file()
                    || manifest.failures.contains_key(&key)
                    || manifest.derived_outputs.contains_key(&key)
            });
            (sex, relevant)
        })
        .collect();
    clear_nonregular_animation_outputs(staging)?;
    for sex in ["male", "female"] {
        for (_, stem) in DEFAULT_ANIMATION_CLIPS {
            let key = default_animation_output(sex, stem);
            manifest.failures.remove(&key);
            manifest.derived_outputs.remove(&key);
        }
    }

    for sex in ["male", "female"] {
        let outputs: Vec<_> = DEFAULT_ANIMATION_CLIPS
            .iter()
            .map(|(clip_name, stem)| (*clip_name, *stem, default_animation_output(sex, stem)))
            .collect();
        let skeleton_hkx = winning(assets.join("skeleton.hkx"));
        let skeleton_nif = winning(assets.join("skeleton.nif"));
        let body_path = winning(assets.join(format!("{sex}body_1.nif")));
        let actor_work_exists = skeleton_hkx.is_file()
            || skeleton_nif.is_file()
            || body_path.is_file()
            || outputs.iter().any(|(_, stem, _)| {
                winning(animations.join(sex).join(format!("{stem}.hkx"))).is_file()
            });
        let notices_are_relevant =
            actor_work_exists || prior_derived_relevance.get(sex).copied().unwrap_or(false);
        let missing = [
            ("skeleton.hkx", &skeleton_hkx),
            ("body", &body_path),
            ("skeleton.nif", &skeleton_nif),
        ]
        .into_iter()
        .find(|(_, path)| !path.is_file());
        if let Some((label, path)) = missing {
            for (clip_name, _, relative) in &outputs {
                remove_stale(&staging.join(relative))?;
                if notices_are_relevant {
                    report.notices.push(format!(
                        "{relative}: skipping default {sex} {clip_name} animation: missing {label} {}",
                        path.display()
                    ));
                }
            }
            continue;
        }

        let prepared = (|| -> Result<_> {
            let body_model = crate::mesh::build_skinned_model(&body_path, &skeleton_nif)
                .wrap_err_with(|| format!("failed to build the default {sex} actor body"))?;
            let bones = body_model
                .skeletal_meshes
                .first()
                .ok_or_else(|| eyre!("default {sex} actor body has no skeletal mesh"))?
                .bones
                .clone();
            let skeleton_bytes = fs::read(&skeleton_hkx)
                .wrap_err_with(|| format!("failed to read {}", skeleton_hkx.display()))?;
            Ok((bones, skeleton_bytes))
        })();
        let (bones, skeleton_bytes) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                for (clip_name, _, relative) in &outputs {
                    remove_stale(&staging.join(relative))?;
                    let message = format!(
                        "{relative}: failed to prepare the default {sex} {clip_name} animation: {error:#}"
                    );
                    if fail_fast {
                        for cleanup_sex in ["male", "female"] {
                            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                                manifest
                                    .derived_outputs
                                    .remove(&default_animation_output(cleanup_sex, stem));
                                remove_stale(
                                    &staging.join(default_animation_output(cleanup_sex, stem)),
                                )?;
                            }
                        }
                        return Err(eyre!("{message}"));
                    }
                    report.notices.push(message.clone());
                    warn!("{message}");
                }
                continue;
            }
        };

        for (clip_name, stem, relative) in outputs {
            let output = staging.join(&relative);
            let clip_path = winning(animations.join(sex).join(format!("{stem}.hkx")));
            if !clip_path.is_file() {
                remove_stale(&output)?;
                let message = format!(
                    "{relative}: skipping default {sex} {clip_name} animation: missing clip {}",
                    clip_path.display()
                );
                report.notices.push(message.clone());
                warn!("{message}");
                continue;
            }

            let converted = (|| -> Result<DerivedOutput> {
                let inputs = default_animation_input_hashes(staging, sources, sex, stem)?;
                let clip_bytes = fs::read(&clip_path)
                    .wrap_err_with(|| format!("failed to read {}", clip_path.display()))?;
                let mut clip =
                    animation::decode_clip_with_skeleton(&clip_bytes, &skeleton_bytes)
                        .wrap_err_with(|| format!("failed to decode {}", clip_path.display()))?;

                let candidate_dir = tempfile::tempdir().wrap_err_with(|| {
                    format!("failed to create a temporary directory for the {sex} {clip_name} clip")
                })?;
                let candidate = candidate_dir.path().join(format!("{stem}.glb"));
                let binding = animation::write_clip_glb(&mut clip, &bones, stem, &candidate)
                    .wrap_err_with(|| format!("failed to write {}", output.display()))?;
                let bytes = fs::read(&candidate)?;
                if !fs::read(&output).is_ok_and(|existing| existing == bytes) {
                    crate::mesh::write_glb_atomic(&output, &bytes)?;
                }
                info!(
                    "default {sex} {clip_name} animation over {} body bones at {} ({} bound, {} skipped)",
                    bones.bones.len(),
                    output.display(),
                    binding.bound(),
                    binding.skipped.len()
                );
                derived_output_proof(DEFAULT_GAIT_RECIPE, inputs, &output)
            })();
            if let Ok(proof) = converted {
                manifest.derived_outputs.insert(relative, proof);
            } else if let Err(error) = converted {
                remove_stale(&output)?;
                let message = format!(
                    "{relative}: failed to convert the default {sex} {clip_name} animation: {error:#}"
                );
                if fail_fast {
                    for cleanup_sex in ["male", "female"] {
                        for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                            manifest
                                .derived_outputs
                                .remove(&default_animation_output(cleanup_sex, stem));
                            remove_stale(
                                &staging.join(default_animation_output(cleanup_sex, stem)),
                            )?;
                        }
                    }
                    return Err(eyre!("{message}"));
                }
                report.notices.push(message.clone());
                warn!("{message}");
            }
        }
    }
    Ok(())
}

/// Collects texture usage from staged GLBs and the world database, including scoped replacement normals.
pub(crate) fn collect_texture_semantics(
    staging: &Path,
    source_textures: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<TextureSemantic>>> {
    use rayon::prelude::*;
    let mut semantics = BTreeMap::<String, BTreeSet<TextureSemantic>>::new();
    let glbs: Vec<_> = discover(staging)?
        .into_iter()
        .filter(|path| extension(path, &["glb"]))
        .collect();
    let dependencies = glbs
        .par_iter()
        .map(|glb| -> Result<Vec<_>> {
            MeshConverter::glb_texture_dependencies(glb)?
                .into_iter()
                .map(|dependency| {
                    let resolved = resolve_asset_uri(staging, glb, &dependency.uri)?;
                    let relative = resolved.strip_prefix(staging)?;
                    let key = canonical_asset_path(
                        &relative.to_string_lossy(),
                        AssetKind::Texture,
                        "ktx2",
                    )
                    .and_then(|key| source_texture_key(&key))
                    .wrap_err_with(|| {
                        format!(
                            "invalid texture dependency {:?} resolved from {}",
                            dependency.uri,
                            glb.display()
                        )
                    })?;
                    let replacement = if dependency.semantic == TextureSemantic::Normal {
                        let model = glb
                            .strip_prefix(staging)
                            .ok()
                            .map(|path| path.to_string_lossy().replace('\\', "/"));
                        model.and_then(|model| {
                            crate::mesh::volcanic_normal_replacement(&model, &key, source_textures)
                        })
                    } else {
                        None
                    };
                    Ok((key, dependency.semantic, replacement))
                })
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;
    for (key, semantic, replacement) in dependencies.into_iter().flatten() {
        semantics.entry(key).or_default().insert(semantic);
        if let Some(replacement) = replacement {
            semantics.entry(replacement).or_default().insert(semantic);
        }
    }

    let database = staging.join("skyrim_world.db");
    if database.is_file() {
        let connection =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let columns = [
            ("diffuse_path", TextureSemantic::BaseColor),
            ("normal_path", TextureSemantic::Normal),
            ("glow_path", TextureSemantic::Emissive),
            ("height_path", TextureSemantic::Height),
            ("environment_path", TextureSemantic::EnvironmentCube),
            ("mask_path", TextureSemantic::EnvironmentMask),
            ("specular_path", TextureSemantic::SpecularGlossiness),
            ("detail_path", TextureSemantic::Detail),
        ];
        for (column, semantic) in columns {
            let query = format!(
                "SELECT {column} FROM texture_sets WHERE {column} IS NOT NULL AND {column} <> ''"
            );
            let mut statement = connection.prepare(&query)?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for path in paths {
                insert_texture_semantic(&mut semantics, &path, semantic).wrap_err_with(|| {
                    format!("invalid texture_sets.{column} reference {path:?}")
                })?;
            }
        }
        let mut statement = connection.prepare(
            "SELECT flow_normal_path FROM waters \
             WHERE flow_normal_path IS NOT NULL AND flow_normal_path <> ''",
        )?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for path in paths {
            insert_texture_semantic(&mut semantics, &path, TextureSemantic::Normal)
                .wrap_err_with(|| format!("invalid waters.flow_normal_path reference {path:?}"))?;
        }
    }

    for (path, texture_semantics) in &semantics {
        TextureEncoding::from_semantics(texture_semantics)
            .wrap_err_with(|| format!("incompatible texture uses for {path}"))?;
    }
    Ok(semantics)
}

fn insert_texture_semantic(
    semantics: &mut BTreeMap<String, BTreeSet<TextureSemantic>>,
    path: &str,
    semantic: TextureSemantic,
) -> Result<()> {
    let key = canonical_asset_path(path, AssetKind::Texture, "ktx2")?;
    semantics.entry(key).or_default().insert(semantic);
    Ok(())
}

/// Canonical source texture keys of every discovered texture under `staging/vfs`, as
/// [`canonical_asset_path`] produces them (`textures/rock01.dds`). Used to tell "the game data
/// never contained this texture" from "the game data contains it and its publication failed".
fn texture_source_keys(staging: &Path, files: &[PathBuf]) -> BTreeSet<String> {
    let vfs = staging.join("vfs");
    files
        .iter()
        .filter(|path| extension(path, &["dds"]))
        .filter_map(|path| {
            let relative = path.strip_prefix(&vfs).ok()?;
            canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "dds").ok()
        })
        .collect()
}

/// The canonical `.dds` source key a pruned mesh reference came from: `textures/foo.ktx2` and
/// its `.opensky-srgb` alias both map to `textures/foo.dds`. Returns `None` for references that
/// are not converted texture paths.
fn texture_reference_source_key(reference: &str) -> Option<String> {
    let source = crate::asset_path::runtime_texture_source(reference)?;
    canonical_asset_path(&source, AssetKind::Texture, "dds").ok()
}

/// Meshes whose pruned reference has its source back, or whose scoped volcanic fallback source
/// is present. Retry the latter even when source presence is unchanged: its first DDS conversion
/// may have failed, leaving the authored reference pruned behind the mesh cache.
fn restored_mesh_outputs(
    previous: &ConversionManifest,
    source_textures: &BTreeSet<String>,
) -> BTreeSet<String> {
    previous
        .pruned_texture_references
        .iter()
        .filter(|(glb, references)| {
            references.iter().any(|reference| {
                texture_reference_source_key(reference)
                    .is_some_and(|key| source_textures.contains(&key))
                    || crate::mesh::volcanic_normal_replacement(glb, reference, source_textures)
                        .and_then(|replacement| texture_reference_source_key(&replacement))
                        .is_some_and(|key| source_textures.contains(&key))
            })
        })
        .map(|(glb, _)| glb.clone())
        .collect()
}

/// Resolves a runtime texture or alias to its source KTX2 key, rejecting invalid paths.
pub(crate) fn source_texture_key(runtime_key: &str) -> Result<String> {
    crate::asset_path::runtime_texture_source(runtime_key)
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid runtime texture path: {runtime_key}"))
}

pub(crate) use publish_texture_aliases as publish_srgb_texture_aliases;

/// Links or copies existing source textures to aliases referenced by staged GLBs.
///
/// Returns the published paths relative to staging; references without a source file are skipped.
pub(crate) fn publish_texture_aliases(staging: &Path) -> Result<Vec<PathBuf>> {
    let mut aliases = BTreeSet::new();
    for entry in WalkDir::new(staging)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && extension(entry.path(), &["glb"]))
    {
        let glb = entry.path();
        for dependency in MeshConverter::glb_texture_dependencies(glb)? {
            let destination = resolve_asset_uri(staging, glb, &dependency.uri)?;
            let relative = destination.strip_prefix(staging)?.to_owned();
            let runtime_key =
                canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "ktx2")?;
            let source_key = source_texture_key(&runtime_key)?;
            if source_key != runtime_key {
                aliases.insert(PathBuf::from(&runtime_key));
                // Bevy's skin material loads the same bytes with linear decoding. Publish its
                // companion path as well, retaining `.opensky-wrapN` so clamp/repeat images keep
                // distinct AssetServer identities and sampler settings.
                if let Some(linear) = runtime_key.strip_suffix(".ktx2") {
                    let linear = linear.replace(".opensky-srgb", "");
                    let linear = format!("{linear}.ktx2");
                    // The canonical source is already the default linear path. Never republish
                    // it over itself; only sampler-specific companion aliases need a file.
                    if linear != source_key {
                        aliases.insert(PathBuf::from(linear));
                    }
                }
            }
        }
    }
    let mut published = Vec::new();
    for alias in aliases {
        let source = staging.join(source_texture_key(&alias.to_string_lossy())?);
        if !source.is_file() {
            continue;
        }
        let destination = staging.join(&alias);
        if destination.is_file() {
            fs::remove_file(&destination)?;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        link_or_copy(&source, &destination).wrap_err_with(|| {
            format!(
                "failed to publish texture alias {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        published.push(alias);
    }
    Ok(published)
}

pub(crate) fn discover(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry =
            entry.wrap_err_with(|| format!("failed to discover inputs in {}", root.display()))?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    Ok(files)
}

/// Whether two paths hold byte-identical files. Used to decide whether a prune
/// record from an earlier manifest still describes the mesh about to be
/// published, so a record is never repeated for a mesh that changed.
fn files_are_identical(left: &Path, right: &Path) -> bool {
    let (Ok(left_metadata), Ok(right_metadata)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    if left_metadata.len() != right_metadata.len() {
        return false;
    }
    match (hash_file(left), hash_file(right)) {
        (Ok(left_hash), Ok(right_hash)) => left_hash == right_hash,
        _ => false,
    }
}

/// Checks the generated artifacts on a pool of `jobs` threads, up to the first failure. Decoding and hashing a KTX2 is
/// CPU work and reading each file waits on the disk, so one file at a time left most cores idle.
/// `jobs` is handed to the pool exactly as the conversion stage hands it `cpu_jobs`, where 0
/// selects rayon's own thread count rather than a single thread.
///
/// The failure of the lowest artifact index is returned, so the reported error does not depend on
/// which thread finished first.
pub(crate) fn validate_artifacts(
    staging: &Path,
    artifacts: &[PathBuf],
    texture_semantics: &BTreeMap<String, BTreeSet<TextureSemantic>>,
    jobs: usize,
) -> Result<()> {
    use rayon::prelude::*;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .wrap_err("failed to create artifact validation worker pool")?;
    // Only the lowest-index failure is kept, so memory stays constant however many fail. Once
    // a failure is known, artifacts after it are skipped; earlier ones are still checked, so
    // the reported failure is the first in list order whatever the thread count.
    let lowest_failure = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let first_failure = Mutex::new(None::<(usize, color_eyre::Report)>);
    pool.install(|| {
        artifacts.par_iter().enumerate().for_each_init(
            // `mlua::Lua` cannot move between threads, so each worker builds its own when it
            // first meets a script.
            || None,
            |lua, (index, relative)| {
                if index > lowest_failure.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if let Err(error) = validate_artifact(staging, relative, texture_semantics, lua) {
                    lowest_failure.fetch_min(index, std::sync::atomic::Ordering::Relaxed);
                    let mut first = first_failure.lock().unwrap();
                    if first.as_ref().is_none_or(|(kept, _)| index < *kept) {
                        *first = Some((index, error));
                    }
                }
            },
        );
    });
    match first_failure.into_inner().unwrap() {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// Checks one generated artifact. `lua` is the compiler for Luau scripts, created on first use so
/// a caller that never meets a script never builds one.
pub(crate) fn validate_artifact(
    staging: &Path,
    relative: &Path,
    texture_semantics: &BTreeMap<String, BTreeSet<TextureSemantic>>,
    lua: &mut Option<mlua::Lua>,
) -> Result<()> {
    let path = staging.join(relative);
    match path.extension().and_then(|extension| extension.to_str()) {
        // Land texture arrays (#215) are not converted source textures: they are KTX2 texture
        // arrays whose class (usage, block format, size) the land texture table records, and
        // their transfer function (sRGB diffuse, linear normal) is a property of that class, not
        // of any source texture semantics. The stage that built them checked every layer and
        // level against the class; here the file's own shape is checked.
        Some("ktx2") if relative.starts_with("terrain") => {
            // A class can be many gigabytes and validation runs on every CPU job at once, so only
            // the header and the first level's index entry are read, never the pixel data.
            let mut file = fs::File::open(&path)?;
            let file_len = file.metadata()?.len();
            let mut prefix = [0u8; 104];
            std::io::Read::read_exact(&mut file, &mut prefix)
                .map_err(|error| eyre!("invalid KTX2 array {}: {error}", path.display()))?;
            let header_bytes: [u8; ktx2::Header::LENGTH] = prefix[..ktx2::Header::LENGTH]
                .try_into()
                .expect("the prefix is longer than the header");
            let header = ktx2::Header::from_bytes(&header_bytes)
                .map_err(|error| eyre!("invalid KTX2 array {}: {error:?}", path.display()))?;
            let level_offset = u64::from_le_bytes(prefix[80..88].try_into().expect("eight bytes"));
            let level_length = u64::from_le_bytes(prefix[88..96].try_into().expect("eight bytes"));
            ensure!(
                header.layer_count >= 2
                    && header.level_count > 0
                    && header.format.is_some()
                    && level_length > 0
                    && level_offset
                        .checked_add(level_length)
                        .is_some_and(|end| end <= file_len),
                "invalid KTX2 array {}",
                path.display()
            );
        }
        Some("ktx2") => {
            let bytes = fs::read(&path)?;
            let key = source_texture_key(&canonical_asset_path(
                &relative.to_string_lossy(),
                AssetKind::Texture,
                "ktx2",
            )?)?;
            let known_semantics = texture_semantics.get(&key).cloned().unwrap_or_default();
            let encoding = TextureEncoding::from_semantics(&known_semantics)?;
            let metadata = crate::texture::inspect_ktx2(&bytes, encoding)
                .wrap_err_with(|| format!("invalid KTX2 {}", path.display()))?;
            ensure!(
                metadata.encoded_bytes == fs::metadata(&path)?.len()
                    && !metadata.sha256.is_empty()
                    && metadata.expanded_rgba_bytes > 0,
                "KTX2 metadata validation failed for {}",
                path.display()
            );
        }
        Some("glb") => {
            // Only the 12-byte GLB header is checked, so only the header is read.
            let mut header = [0_u8; 12];
            let read = fs::File::open(&path)
                .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header));
            match read {
                Ok(()) if &header[..4] == b"glTF" => {}
                Ok(()) => bail!("invalid GLB artifact {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    bail!("invalid GLB artifact {}", path.display())
                }
                Err(error) => return Err(error.into()),
            }
        }
        Some("luau") => {
            let source = fs::read_to_string(&path)?;
            lua.get_or_insert_with(mlua::Lua::new)
                .load(&source)
                .set_name(path.to_string_lossy())
                .into_function()
                .map_err(|error| {
                    color_eyre::eyre::eyre!("invalid Luau artifact {}: {error}", path.display())
                })?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn overlay_loose_assets(data: &Path, vfs: &Path, files: &[PathBuf]) -> Result<()> {
    let mut seen = BTreeMap::<String, PathBuf>::new();
    for source in files
        .iter()
        .filter(|path| extension(path, &["dds", "nif", "pex", "lod", "hkx"]))
    {
        let relative = source.strip_prefix(data)?;
        let (kind, extension) = if extension(source, &["dds"]) {
            (AssetKind::Texture, "dds")
        } else if extension(source, &["nif"]) {
            (AssetKind::Mesh, "nif")
        } else if extension(source, &["pex"]) {
            (AssetKind::Script, "pex")
        } else if extension(source, &["hkx"]) {
            if !crate::archive::is_animation_input(relative) {
                continue;
            }
            (AssetKind::Animation, "hkx")
        } else {
            (AssetKind::LodSettings, "lod")
        };
        let canonical = canonical_asset_path(&relative.to_string_lossy(), kind, extension)?;
        if let Some(previous) = seen.insert(canonical.clone(), source.to_owned()) {
            bail!(
                "loose assets contain normalized path collision for {canonical}: {} and {}",
                previous.display(),
                source.display()
            );
        }
        let destination = vfs.join(canonical);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        // A reused archive entry links its `vfs` file to the cache blob, so this override has to
        // replace the path rather than write through it: copying over the link would truncate the
        // file every other name shares, the blob and the previous output included.
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

/// Select explicit plugins or direct Data children, warning about nested-only discovery.
pub(crate) fn plugin_paths(
    config: &PipelineConfig,
    files: &[PathBuf],
    notices: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let mut selection_warnings = Vec::new();
    let result = plugin_paths_with_diagnostics(
        config,
        files,
        notices,
        &mut Vec::new(),
        &mut selection_warnings,
    );
    notices.extend(
        selection_warnings
            .into_iter()
            .map(|warning| warning.to_string()),
    );
    result
}

/// Select surviving plugin paths before hashes, archive priorities, merging and publication.
fn plugin_paths_with_diagnostics(
    config: &PipelineConfig,
    files: &[PathBuf],
    notices: &mut Vec<String>,
    skipped_plugins: &mut Vec<String>,
    selection_warnings: &mut Vec<CreationFallbackWarning>,
) -> Result<Vec<PathBuf>> {
    if let Some(path) = &config.plugins_file {
        return read_with_selection_diagnostics(
            path,
            &config.data_dir,
            config.skyrim_ccc_file.as_deref(),
            config.skyrim_ini_file.as_deref(),
            notices,
            skipped_plugins,
            selection_warnings,
        );
    }
    let discovered: Vec<_> = files
        .iter()
        .filter(|path| extension(path, &["esm", "esp", "esl"]))
        .collect();
    let mut plugins: Vec<_> = discovered
        .iter()
        .copied()
        .filter(|path| {
            path.strip_prefix(&config.data_dir)
                .is_ok_and(|relative| relative.components().count() == 1)
        })
        .cloned()
        .collect();
    if !discovered.is_empty() && plugins.is_empty() {
        // The caller forwards notices on the progress channel; printing here would splice
        // into the CLI's status line.
        notices.push(format!(
            "found {} plugin {}, but none directly in {}; plugins in subfolders are ignored",
            discovered.len(),
            if discovered.len() == 1 {
                "file"
            } else {
                "files"
            },
            config.data_dir.display()
        ));
    }
    plugins.sort_by_key(|path| {
        let name = shared::plugin_selection::normalize_plugin_name(
            &path.file_name().unwrap_or_default().to_string_lossy(),
        );
        let rank = crate::esm::OFFICIAL_MASTERS
            .iter()
            .position(|official| official.eq_ignore_ascii_case(&name))
            .unwrap_or(10);
        (rank, name, path.file_name().unwrap_or_default().to_owned())
    });
    let had_plugins = !plugins.is_empty();
    let plugins = crate::esm::plugins_txt::recover_discovered(plugins, notices, skipped_plugins)?;
    ensure!(
        !had_plugins || !plugins.is_empty(),
        "no usable plugins remain after automatic master/dependency/slot recovery: {}",
        notices
            .last()
            .map(String::as_str)
            .unwrap_or("empty selection"),
    );
    Ok(plugins)
}

/// Materializes winning loose LOD inputs for the compiler's filesystem-based VFS reads.
fn stage_mo2_lod_inputs(vfs: &Path, sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    for (relative, source) in sources {
        if crate::asset_path::is_authoring_resource(relative) {
            continue;
        }
        let relative = if extension(source, &["dds"]) {
            PathBuf::from(canonical_asset_path(relative, AssetKind::Texture, "dds")?)
        } else if relative.starts_with("lodsettings/") && extension(source, &["lod"]) {
            crate::archive::safe_relative_path(relative)?
        } else {
            continue;
        };
        let destination = vfs.join(relative);
        fs::create_dir_all(destination.parent().unwrap())?;
        link_or_copy(source, &destination)?;
    }
    Ok(())
}

/// Links the winning MO2 animation inputs into the staged VFS, where the animation pass reads
/// them beside the archive-extracted copies they override.
fn stage_mo2_animation_inputs(vfs: &Path, sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    for (relative, source) in sources {
        if crate::asset_path::is_authoring_resource(relative)
            || !crate::archive::is_animation_input(Path::new(relative))
        {
            continue;
        }
        let destination = vfs.join(crate::archive::safe_relative_path(relative)?);
        fs::create_dir_all(destination.parent().unwrap())?;
        link_or_copy(source, &destination)?;
    }
    Ok(())
}

/// Whether a path names an output this converter derives without a matching
/// source file: composed actor bodies and animation clips.
fn is_derived_output(relative: &str) -> bool {
    compose::is_composed_output(relative) || animation::is_animation_output(relative)
}

/// Removes staged mesh, texture, and script outputs absent from the current MO2 source selection.
///
/// Retains current resume work while preventing removed outputs from contributing stale references.
fn prune_removed_mo2_outputs(staging: &Path, sources: &[PathBuf]) -> Result<()> {
    let mut current = BTreeSet::new();
    for source in sources {
        let (kind, output) = if extension(source, &["nif"]) {
            (AssetKind::Mesh, "glb")
        } else if extension(source, &["dds"]) {
            (AssetKind::Texture, "ktx2")
        } else if extension(source, &["pex"]) {
            (AssetKind::Script, "luau")
        } else {
            continue;
        };
        current.insert(canonical_asset_path(
            &source.strip_prefix(staging.join("vfs"))?.to_string_lossy(),
            kind,
            output,
        )?);
    }
    for folder in ["meshes", "textures", "scripts"] {
        let directory = staging.join(folder);
        if !directory.is_dir() {
            continue;
        }
        for path in discover(&directory)? {
            if !extension(&path, &["glb", "ktx2", "luau"]) {
                continue;
            }
            let relative = path
                .strip_prefix(staging)?
                .to_string_lossy()
                .replace('\\', "/");
            if is_derived_output(&relative) {
                continue;
            }
            let key = if extension(&path, &["ktx2"]) {
                source_texture_key(&relative)?
            } else {
                relative
            };
            if !current.contains(&key) {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

/// Record installed direct-Data archive owners, including disabled and recovered-away plugins.
/// Plugins sharing a stem can share companions; any surviving plugin with that stem activates them.
pub(crate) fn installed_archive_owners(
    data_dir: &Path,
    files: &[PathBuf],
    plugins: &[PathBuf],
) -> BTreeMap<String, bool> {
    let active_stems: BTreeSet<_> = plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .map(|stem| shared::plugin_selection::normalize_plugin_name(&stem.to_string_lossy()))
        .collect();
    files
        .iter()
        .filter(|path| extension(path, &["esm", "esp", "esl"]))
        .filter(|path| {
            path.strip_prefix(data_dir)
                .is_ok_and(|relative| relative.components().count() == 1)
        })
        .filter_map(|path| path.file_stem())
        .map(|stem| {
            let stem = shared::plugin_selection::normalize_plugin_name(&stem.to_string_lossy());
            let active = active_stems.contains(&stem);
            (stem, active)
        })
        .collect()
}

/// Keep all root plugin identities in the winning MO2 VFS, including unchecked and rejected owners.
/// Physical sources can live outside Data; recovered winning source paths determine active status.
pub(crate) fn mo2_installed_archive_owners(
    files: &BTreeMap<String, PathBuf>,
    plugins: &[PathBuf],
) -> BTreeMap<String, bool> {
    let active_sources: BTreeSet<_> = plugins.iter().cloned().collect();
    let mut owners = BTreeMap::<String, bool>::new();
    for (relative, source) in files {
        let relative = Path::new(relative);
        if relative.components().count() != 1 || !extension(relative, &["esm", "esp", "esl"]) {
            continue;
        }
        let Some(stem) = relative.file_stem() else {
            continue;
        };
        let stem = shared::plugin_selection::normalize_plugin_name(&stem.to_string_lossy());
        let active = active_sources.contains(source);
        // Distinct extensions can share a companion stem. An unchecked peer must not erase
        // the activation contributed by a surviving plugin with that stem.
        owners
            .entry(stem)
            .and_modify(|survives| *survives |= active)
            .or_insert(active);
    }
    owners
}

/// Apply selection to owned companions while retaining unowned standalone and base archives.
/// The longest matching installed stem owns the archive, so an active prefix cannot activate
/// a more specifically named disabled or recovered-away plugin's companion.
pub(crate) fn non_mo2_archive_is_active(archive: &Path, owners: &BTreeMap<String, bool>) -> bool {
    let stem = shared::plugin_selection::normalize_plugin_name(
        &archive.file_stem().unwrap_or_default().to_string_lossy(),
    );
    owners
        .iter()
        .filter(|(owner, _)| archive_stem_matches_plugin(&stem, owner))
        .max_by_key(|(owner, _)| owner.len())
        .is_none_or(|(_, active)| *active)
}

/// Match an exact normalized stem or an owner followed by a space, dash or underscore.
/// Ownership, MO2 activity and priority share this companion-name boundary.
fn archive_stem_matches_plugin(stem: &str, plugin: &str) -> bool {
    stem == plugin
        || stem
            .strip_prefix(plugin)
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|separator| matches!(separator, ' ' | '-' | '_'))
}

/// Checks whether an archive stem matches an active plugin, including suffixed companion archives.
pub(crate) fn mo2_archive_is_active(archive: &Path, plugins: &[PathBuf]) -> bool {
    let stem = shared::plugin_selection::normalize_plugin_name(
        &archive.file_stem().unwrap_or_default().to_string_lossy(),
    );
    plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .any(|plugin| {
            let plugin = shared::plugin_selection::normalize_plugin_name(&plugin.to_string_lossy());
            archive_stem_matches_plugin(&stem, &plugin)
        })
}

/// Sort archives by their longest matching active stem's load order, then lowercase stem.
/// Priority retains the legacy space/dash/underscore suffixes; unmatched archives sort last
/// and same-stem plugins retain the first owner's priority.
pub(crate) fn sort_archives_by_load_order(archives: &mut [PathBuf], plugins: &[PathBuf]) {
    archives.sort_by_cached_key(|archive| {
        (
            archive_load_order_priority(archive, plugins).unwrap_or(usize::MAX),
            crate::esm::load_order::normalize_plugin_name(
                &archive.file_stem().unwrap_or_default().to_string_lossy(),
            ),
        )
    });
}

/// Resolve the most specific active owner, retaining the first slot when owner stems are equal.
pub(crate) fn archive_load_order_priority(archive: &Path, plugins: &[PathBuf]) -> Option<usize> {
    let plugin_stems = plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .map(|stem| shared::plugin_selection::normalize_plugin_name(&stem.to_string_lossy()))
        .collect::<Vec<_>>();
    let stem = shared::plugin_selection::normalize_plugin_name(
        &archive.file_stem().unwrap_or_default().to_string_lossy(),
    );
    plugin_stems
        .iter()
        .enumerate()
        .filter(|(_, plugin)| archive_stem_matches_plugin(&stem, plugin))
        .max_by_key(|(index, plugin)| (plugin.len(), std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
}

fn extension(path: &Path, expected: &[&str]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            expected
                .iter()
                .any(|expected| value.eq_ignore_ascii_case(expected))
        })
}

/// Deletes staged meshes outside the accepted set, preserving extracted VFS sources.
///
/// Paths are relative to staging with forward slashes. Conversion supplies the
/// accepted artifacts after current-source, schema/configuration and byte checks;
/// downstream scans must see exactly the meshes publication will include.
fn invalidate_staged_mesh_outputs(staging: &Path, verified: &BTreeSet<String>) -> Result<()> {
    let vfs = staging.join("vfs");
    for entry in WalkDir::new(staging)
        .into_iter()
        .filter_entry(|entry| entry.path() != vfs.as_path())
    {
        let entry = entry?;
        if !entry.file_type().is_file() || !extension(entry.path(), &["glb"]) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(staging)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if verified.contains(&relative) {
            continue;
        }
        fs::remove_file(entry.path()).wrap_err_with(|| {
            format!(
                "failed to invalidate staged mesh {}",
                entry.path().display()
            )
        })?;
    }
    Ok(())
}

fn invalidate_staged_generated_outputs(staging: &Path) -> Result<()> {
    // Rebuild from current inputs so removed sources cannot leave orphaned
    // metadata. Keep partial assets until the effective VFS is rebuilt: the
    // journal still proves which current outputs a resume can reuse.
    for relative in [
        "skyrim_world.db",
        "skyrim_world.db-wal",
        "skyrim_world.db-shm",
        "cell_cache.rkyv",
        "cell_cache_preserved.rkyv",
        "record-reader.json",
        "inhouse-reader-diagnostics.json",
        crate::esm::inhouse::DATABASE_PROFILE_FILE,
        "integration-report.json",
        "lod-manifest.json",
        "metadata-rebuild.json",
        "metadata-source-conversion-manifest.json",
    ] {
        let path = staging.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to invalidate {}", path.display()));
            }
        }
    }
    let lod_dir = staging.join("lod");
    if lod_dir.exists() {
        fs::remove_dir_all(&lod_dir)?;
    }
    // The merged terrain (#215) lives entirely under `terrain`: the arrays, the table and the
    // chunk files. It is rebuilt from the current records, so a resume with the option off
    // publishes an output without it rather than carrying a stale one.
    let terrain_dir = staging.join("terrain");
    if terrain_dir.exists() {
        fs::remove_dir_all(&terrain_dir)?;
    }
    Ok(())
}

fn remove_orphan_staged_assets(staging: &Path, sources: &[PathBuf]) -> Result<()> {
    let vfs = staging.join("vfs");
    let mut expected = BTreeSet::new();
    for source in sources {
        let (kind, output_extension) = if extension(source, &["nif"]) {
            (AssetKind::Mesh, "glb")
        } else if extension(source, &["dds"]) {
            (AssetKind::Texture, "ktx2")
        } else if extension(source, &["pex"]) {
            (AssetKind::Script, "luau")
        } else {
            continue;
        };
        expected.insert(PathBuf::from(canonical_asset_path(
            &source.strip_prefix(&vfs)?.to_string_lossy(),
            kind,
            output_extension,
        )?));
    }
    for entry in WalkDir::new(staging)
        .into_iter()
        .filter_entry(|entry| entry.path() != vfs.as_path())
    {
        let entry = entry?;
        let relative = entry.path().strip_prefix(staging)?;
        if entry.file_type().is_file()
            && extension(entry.path(), &["glb", "ktx2", "luau"])
            && !is_derived_output(&relative.to_string_lossy().replace('\\', "/"))
            && !expected.contains(relative)
        {
            fs::remove_file(entry.path()).wrap_err_with(|| {
                format!(
                    "failed to remove orphaned staged asset {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

/// Removes a staging folder after a successful publish, unless it is or holds the Skyrim Data
/// folder: a `--resume-staging` folder is named by the user, and removing it must never take the
/// game data with it. A folder that is kept, or fails to go, is reported on stderr and left behind;
/// the run has already published, so neither fails it. Returns whether the folder was removed.
fn remove_staging(staging: &Path, data_dir: &Path) -> bool {
    match (fs::canonicalize(staging), fs::canonicalize(data_dir)) {
        (Ok(staging_path), Ok(data_path)) if !data_path.starts_with(&staging_path) => {}
        (Ok(_), Ok(_)) => {
            eprintln!(
                "warning: kept the staging folder {} because it holds the Skyrim Data folder {}",
                staging.display(),
                data_dir.display()
            );
            return false;
        }
        _ => {
            eprintln!(
                "warning: kept the staging folder {}: could not compare it with the Skyrim Data folder {}",
                staging.display(),
                data_dir.display()
            );
            return false;
        }
    }
    match fs::remove_dir_all(staging) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "warning: could not remove the staging folder {}: {error}",
                staging.display()
            );
            false
        }
    }
}

fn remove_invalid_staged_output(target: &Path) -> Result<()> {
    match fs::remove_file(target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "failed to remove invalid staged output {}",
                target.display()
            )
        }),
    }
}

fn remove_partial_output_after_failure(
    target: &Path,
    conversion: Result<()>,
) -> (Result<()>, bool) {
    match conversion {
        Ok(()) => (Ok(()), false),
        Err(error) => match remove_invalid_staged_output(target) {
            Ok(()) => (Err(error), false),
            Err(remove_error) => (Err(remove_error), true),
        },
    }
}

/// Strips the leading asset kind folder (e.g., "textures", "meshes", "scripts")
/// from a relative path in a case-insensitive manner.
///
/// This avoids creating double-nested output directory structures when processing
/// assets extracted from BSA archives or loose mod folders with mixed-case naming
/// (such as `Textures\actors\dragon.dds` or `Meshes\armor\iron.nif`).
pub(crate) fn staging_path(output: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // The whole file name, not `with_extension`: a dotted output name (`Skyrim.Converted`) must
    // give `Skyrim.Converted.staging-...`, the prefix resuming and `find_resumable_staging` expect.
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!("{name}.staging-{}-{stamp}", std::process::id()))
}

/// Where the staging journal waits while its staging directory is published.
#[cfg(test)]
fn parked_journal_path(staging: &Path) -> PathBuf {
    let mut name = staging.file_name().unwrap_or_default().to_os_string();
    name.push(".journal.jsonl");
    staging.with_file_name(name)
}

/// Whether the staged `glb` holds the bytes the previous manifest recorded for it, converted
/// from the same source this run found. The previous manifest was published together with its
/// prune records, so a match means those records describe the staged mesh.
fn staged_matches_previous_entry(
    batch: &ConversionBatch<'_>,
    entries_by_output: &BTreeMap<&str, Vec<&str>>,
    glb: &str,
    staged_glb: &Path,
) -> bool {
    let Ok(staged_hash) = hash_file(staged_glb) else {
        return false;
    };
    entries_by_output.get(glb).is_some_and(|keys| {
        keys.iter().any(|key| {
            let entry = &batch.manifest.entries[*key];
            batch.previous.entries.get(*key).is_some_and(|previous| {
                previous.output == glb
                    && previous.source_hash == entry.source_hash
                    && previous.output_hash == staged_hash
            })
        })
    })
}

/// Provenance for a cache entry whose output is now complete inside staging.
fn staged_output(entry: &CacheEntry, configuration_hash: &str) -> StagedOutput {
    StagedOutput {
        schema_version: CONVERTER_SCHEMA_VERSION,
        configuration_hash: configuration_hash.to_owned(),
        source_hash: entry.source_hash.clone(),
        output_size: entry.output_size,
        output_hash: entry.output_hash.clone(),
    }
}

#[derive(Default)]
struct PublicationTimings {
    enabled: bool,
    elapsed_ms: BTreeMap<String, u128>,
}

impl PublicationTimings {
    fn measure<T>(&mut self, operation: &str, task: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.enabled {
            return task();
        }
        let started = Instant::now();
        let result = task();
        *self.elapsed_ms.entry(operation.to_owned()).or_default() += started.elapsed().as_millis();
        result
    }
}

#[cfg(test)]
thread_local! {
    /// Makes the previous pack's removal fail after the swap, as a held or read-only backup
    /// does on Windows, so tests can prove a failed cleanup never fails a published pack.
    static FAIL_PREVIOUS_PACK_CLEANUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the post-publication ingestion-cache persistence fail, as a full disk does.
    static FAIL_INGESTION_CACHE_PERSISTENCE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Names a failure that happened after the runtime pack was published. The pack on disk is
/// complete, so the message is a notice: it is printed with the run's summary, kept in the JSON
/// report, and logged, but it never makes a good pack look like a failed conversion or asks the
/// user to resume a run that is already published.
fn push_post_publication_notice(notices: &mut Vec<String>, message: String) {
    warn!("{message}");
    notices.push(message);
}

/// Publishes only runtime artifacts. Staging keeps `vfs/` and
/// `.ingestion-cache/` as build workspace; those never land in `output`.
fn publish_runtime_pack(
    staging: &Path,
    output: &Path,
    report: &mut PipelineReport,
    lock: &AssetLock,
) -> Result<BTreeMap<String, u128>> {
    let mut timings = PublicationTimings {
        enabled: true,
        ..Default::default()
    };
    // Beside staging, not inside it: a resumed staging dir keeps the
    // previous pack's linked files, so the new pack must not share inodes
    // with anything a resumed run will later unlink and rewrite.
    let pack_staging = timings.measure("runtime_pack_preparation", || {
        let path = staging.with_extension(format!(
            "pack-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(path)
    })?;
    let published = (|| -> Result<()> {
        timings.measure("runtime_pack_linking", || {
            for artifact in &report.artifacts {
                if artifact == Path::new(shared::streaming_costs::STREAMING_COST_FILE_NAME) {
                    continue; // Always regenerate; never overwrite a linked prior catalog.
                }
                let source = staging.join(artifact);
                if !source.is_file() {
                    continue;
                }
                let destination = pack_staging.join(artifact);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                link_or_copy(&source, &destination).wrap_err_with(|| {
                    format!(
                        "failed to publish {} to {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }
            ensure!(
                pack_staging.join("conversion-manifest.json").is_file(),
                "runtime pack is missing conversion-manifest.json"
            );
            Ok(())
        })?;
        // Analyze only the assembled runtime pack, after its final manifest is
        // present. Staging can contain stale resume artifacts and build inputs.
        // Generate before the directory swap so readers never see a mismatched
        // catalog/manifest pair and failures preserve the previous published pack.
        timings.measure("runtime_pack_costs", || {
            write_streaming_cost_catalog(&pack_staging)
        })?;
        publish_directory_locked_with_timings(
            &pack_staging,
            output,
            true,
            lock,
            &mut timings,
            &mut report.notices,
        )
    })();
    if published.is_err() {
        let _ = fs::remove_dir_all(&pack_staging);
    }
    published?;
    Ok(timings.elapsed_ms)
}

/// Writes the streaming cost sidecar for an assembled, immutable-until-published pack. Call only
/// after the final `conversion-manifest.json` bytes are in place: the catalog fingerprint is that
/// file's hash. A sidecar linked from another pack is unlinked first, never written through.
pub(crate) fn write_streaming_cost_catalog(pack: &Path) -> Result<()> {
    let destination = pack.join(shared::streaming_costs::STREAMING_COST_FILE_NAME);
    match fs::remove_file(&destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let catalog = crate::streaming_costs::analyze_pack(pack)?;
    fs::write(destination, serde_json::to_vec_pretty(&catalog)?)?;
    Ok(())
}

/// Publishes ingestion blobs into the persistent cache, repairing corrupt copies.
fn persist_ingestion_cache(staging_cache: &Path, cache_root: &Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_INGESTION_CACHE_PERSISTENCE.with(std::cell::Cell::get) {
        bail!("injected ingestion cache persistence failure");
    }
    let absolute = if cache_root.is_absolute() {
        cache_root.to_owned()
    } else {
        std::env::current_dir()?.join(cache_root)
    };
    let cache_root = absolute.as_path();
    let mut ancestor = cache_root.to_owned();
    while !ancestor.is_dir() {
        ancestor = ancestor.parent().unwrap().to_owned();
    }
    let mut directories = BTreeSet::new();
    for entry in WalkDir::new(staging_cache).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(staging_cache)?;
        let destination = cache_root.join(relative);
        let name = entry.file_name().to_string_lossy();
        let is_pack = relative.starts_with("batches");
        let expected = if is_pack {
            name.strip_suffix(".pack").unwrap_or("")
        } else {
            &name
        };
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let matches = fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.is_file())
            && fs::metadata(&destination)?.len() == entry.metadata()?.len()
            && hash_file(&destination)? == expected;
        if !matches {
            ensure!(
                hash_file(entry.path())? == expected,
                "staged ingestion cache object does not match its SHA-256 filename"
            );
            let parent = destination.parent().unwrap();
            fs::create_dir_all(parent)?;
            let temporary = tempfile::Builder::new()
                .prefix(".ingestion-cache-")
                .tempfile_in(parent)?
                .into_temp_path();
            link_or_copy(entry.path(), &temporary)?;
            temporary
                .persist(&destination)
                .wrap_err("failed to atomically publish ingestion cache")?;
        }
        if is_pack {
            crate::archive::sync_pack_file(&destination)?;
            let mut current = destination.parent();
            while let Some(path) = current.filter(|path| path.starts_with(&ancestor)) {
                directories.insert(path.to_owned());
                current = path.parent();
            }
        }
    }
    for directory in directories.into_iter().rev() {
        crate::archive::sync_directory(&directory)?;
    }
    Ok(())
}

/// Compiles Phase 1 terrain LOD chunks for every worldspace with a valid
/// origin, after the world database and cell cache exist and before
/// integration validation reads them.
///
/// Settings resolution per worldspace: an explicit `lod_origins` entry wins
/// (custom worlds), otherwise the `lodsettings/<WorldspaceEDID>.lod` sidecar
/// supplies its origin and Skyrim grid metadata (installed worlds). Missing or invalid
/// settings omit only that world's LOD; full-detail conversion remains usable.
///
/// The build identity is one hash over the ordered plugin bytes, every
/// compiled chunk's content hash, the configuration hash, and the converter
/// schema (BUILD-01/02). It is written to `lod_build` and to the published
/// manifest; the runtime refuses chunks whose manifest identity differs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compile_lod_chunks(
    config: &PipelineConfig,
    staging: &Path,
    reuse_root: &Path,
    plugins: &[PathBuf],
    plugin_hashes: &[String],
    progress_tx: &Sender<ProgressEvent>,
    report: &mut PipelineReport,
    ownership: Option<Arc<LodWorkerOwnership>>,
) -> Result<()> {
    compile_lod_chunks_with_cancel(
        config,
        staging,
        reuse_root,
        plugins,
        plugin_hashes,
        progress_tx,
        &Cancellation::new(),
        report,
        ownership,
    )
    .await
}

/// The blocking compiler retains its publication leases if its caller is dropped.
pub(crate) struct LodWorkerOwnership {
    pub(crate) _runtime: Vec<Arc<AssetLock>>,
    pub(crate) _repair: Option<Arc<crate::repair::OutputOwnership>>,
}

struct CancelLodWorkerOnDrop(Option<Cancellation>);

impl Drop for CancelLodWorkerOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

async fn run_owned_lod_worker<T: Send + 'static>(
    cancellation: Cancellation,
    ownership: Option<Arc<LodWorkerOwnership>>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let mut guard = CancelLodWorkerOnDrop(Some(cancellation));
    let worker = spawn_blocking(move || {
        let _ownership = ownership;
        work()
    });
    let result = worker.await.wrap_err("terrain LOD worker failed");
    guard.0 = None;
    result
}

/// Runs terrain compilation on Tokio's blocking pool with owned inputs and report state, so a
/// dropped caller cancels the worker without releasing its publication leases early.
#[allow(clippy::too_many_arguments)]
async fn compile_lod_chunks_with_cancel(
    config: &PipelineConfig,
    staging: &Path,
    reuse_root: &Path,
    plugins: &[PathBuf],
    plugin_hashes: &[String],
    progress_tx: &Sender<ProgressEvent>,
    cancellation: &Cancellation,
    report: &mut PipelineReport,
    ownership: Option<Arc<LodWorkerOwnership>>,
) -> Result<()> {
    let config = config.clone();
    let staging = staging.to_path_buf();
    let reuse_root = reuse_root.to_path_buf();
    let plugins = plugins.to_vec();
    let plugin_hashes = plugin_hashes.to_vec();
    let progress_tx = progress_tx.clone();
    let worker_cancellation = cancellation.clone();
    let mut worker_report = std::mem::take(report);
    let runtime = tokio::runtime::Handle::current();
    #[cfg(test)]
    let fail_gpu_init = FAIL_LOD_GPU_INIT.with(std::cell::Cell::get);
    #[cfg(test)]
    let wait_ready = LOD_PROGRESS_WAIT_READY.with(|ready| ready.borrow_mut().take());
    let (result, returned_report) =
        run_owned_lod_worker(cancellation.clone(), ownership, move || {
            #[cfg(test)]
            let _gpu_override = LodGpuInitOverride::new(fail_gpu_init);
            #[cfg(test)]
            LOD_PROGRESS_WAIT_READY.with(|ready| *ready.borrow_mut() = wait_ready);
            let result = runtime.block_on(async {
                // Cancellation must also release a worker waiting for a full progress
                // channel. Synchronous transactions poll the same flag between batches.
                tokio::select! {
                    biased;
                    _ = worker_cancellation.cancelled() => Err(Interrupted::new().into()),
                    result = compile_lod_chunks_inline(
                        &config, &staging, &reuse_root, &plugins, &plugin_hashes,
                        &progress_tx, &worker_cancellation, &mut worker_report,
                    ) => result,
                }
            });
            (result, worker_report)
        })
        .await?;
    *report = returned_report;
    result
}

#[cfg(test)]
struct LodGpuInitOverride(bool);

#[cfg(test)]
impl LodGpuInitOverride {
    fn new(value: bool) -> Self {
        Self(FAIL_LOD_GPU_INIT.with(|flag| flag.replace(value)))
    }
}

#[cfg(test)]
impl Drop for LodGpuInitOverride {
    fn drop(&mut self) {
        FAIL_LOD_GPU_INIT.with(|flag| flag.set(self.0));
    }
}

#[allow(clippy::too_many_arguments)]
async fn compile_lod_chunks_inline(
    config: &PipelineConfig,
    staging: &Path,
    reuse_root: &Path,
    plugins: &[PathBuf],
    plugin_hashes: &[String],
    progress_tx: &Sender<ProgressEvent>,
    cancellation: &Cancellation,
    report: &mut PipelineReport,
) -> Result<()> {
    if config.no_lod {
        interrupt(cancellation)?;
        let message = "Terrain LOD compilation disabled by --no-lod";
        report.notices.push(message.to_owned());
        #[cfg(test)]
        LOD_PROGRESS_WAIT_READY.with(|ready| {
            if let Some(ready) = ready.borrow_mut().take() {
                let _ = ready.send(());
            }
        });
        let _ = progress_tx
            .send(ProgressEvent::notice(
                ProgressStage::LodChunks,
                None,
                message,
            ))
            .await;
        return Ok(());
    }
    let db_path = staging.join("skyrim_world.db");
    if plugins.is_empty() || !db_path.is_file() {
        return Ok(());
    }
    use rayon::prelude::*;
    let connection = Connection::open(&db_path)?;
    let prior_package = reuse_root.join("conversion-manifest.json").exists()
        || reuse_root.join("lod-manifest.json").exists()
        || reuse_root.join("skyrim_world.db").exists();
    let reuse = if config.invalidate_cache || !prior_package {
        None
    } else {
        match LodReuse::open(reuse_root) {
            Ok(reuse) => Some(reuse),
            Err(error) => {
                let message = format!("Previous terrain LOD package refused: {error:#}");
                report.notices.push(message.clone());
                let _ = progress_tx
                    .send(ProgressEvent::notice(
                        ProgressStage::LodChunks,
                        None,
                        &message,
                    ))
                    .await;
                None
            }
        }
    };
    let cell_cache = TerrainCellCache::read(&staging.join("cell_cache.rkyv"))?;
    let mut texture_cache = TerrainTextureCache::default();
    // Entry 0 of the land texture table, resolved exactly as the merged-terrain stage resolves it
    // (#215), so a quadrant with no BTXT is painted the same texture at every distance. A
    // configured default that cannot be used falls back to the generated white one, as entry 0
    // does there, and the report names it instead of skipping the world.
    let (default_diffuse, default_fallback) =
        crate::terrain_pipeline::TerrainDefaultDiffuse::resolve(
            &crate::terrain_pipeline::TerrainOptions::from_config(config),
        )?;
    let (default_textures, default_error) =
        LoadedDefault::load(&default_diffuse, &staging.join("vfs"), &mut texture_cache);
    if let Some(reason) = default_fallback.or(default_error) {
        report.lod_warnings.push(format!(
            "terrain LOD default land texture falls back to the generated white one: {reason}"
        ));
    }
    let mut chunk_inputs = BTreeMap::new();
    let mut worlds: Vec<(u32, String)> = connection
        .prepare("SELECT id, editor_id FROM worldspaces ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    // Deterministic order regardless of rowid layout.
    worlds.sort();
    send(
        progress_tx,
        ProgressStage::LodChunks,
        0,
        worlds.len() as u64,
        None,
        "Compiling terrain LOD chunks",
    )
    .await;
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut world_settings = Vec::with_capacity(worlds.len());
    let mut compiled_worlds = 0u64;
    let mut terrain_sources = BTreeMap::new();
    let compiler_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.cpu_jobs)
        .build()
        .wrap_err("failed to create terrain LOD compiler pool")?;
    let mut gpu = None;
    let mut gpu_attempted = false;
    enum LodBatchItem {
        Reused(TerrainChunk, String),
        Prepared(PreparedTerrainChunk, TerrainAtlas, String, Option<String>),
    }
    'world: for (index, (worldspace_id, editor_id)) in worlds.iter().enumerate() {
        interrupt(cancellation)?;
        let settings = match resolve_lod_settings(config, &staging.join("vfs"), editor_id) {
            Ok(Some(settings)) => settings,
            Ok(None) => {
                let message = format!(
                    "worldspace {editor_id} ({worldspace_id:08X}) has no LOD origin: no lod_origins entry and no lodsettings/{editor_id}.lod; its terrain LOD is skipped"
                );
                report.lod_warnings.push(message);
                world_settings.push(serde_json::json!({
                    "worldspace_id": worldspace_id,
                    "editor_id": editor_id,
                    "status": "missing",
                }));
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
            Err(error) => {
                report.lod_warnings.push(format!(
                    "worldspace {editor_id} ({worldspace_id:08X}) has invalid LOD settings: {error:#}; its terrain LOD is skipped"
                ));
                world_settings.push(serde_json::json!({
                    "worldspace_id": worldspace_id,
                    "editor_id": editor_id,
                    "status": "invalid",
                }));
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
        };
        let origin = settings.origin;
        world_settings.push(serde_json::json!({
            "worldspace_id": worldspace_id,
            "editor_id": editor_id,
            "status": "ready",
            "origin": [origin.grid_x, origin.grid_y],
            "sidecar": settings.sidecar.map(|sidecar| serde_json::json!({
                "stride": sidecar.stride,
                "min_level": sidecar.min_level,
                "max_level": sidecar.max_level,
            })),
        }));
        connection.execute(
            "UPDATE worldspaces SET lod_origin_x = ?1, lod_origin_y = ?2 WHERE id = ?3",
            rusqlite::params![origin.grid_x, origin.grid_y, worldspace_id],
        )?;
        let cells = exterior_terrain_cells(&connection, *worldspace_id)?;
        if cells.is_empty() {
            send(
                progress_tx,
                ProgressStage::LodChunks,
                (index + 1) as u64,
                worlds.len() as u64,
                None,
                "Compiling terrain LOD chunks",
            )
            .await;
            continue;
        }
        let lookup: std::collections::HashMap<u32, (i32, i32)> = cells
            .iter()
            .map(|(grid_x, grid_y, cell_id)| (*cell_id, (*grid_x, *grid_y)))
            .collect();
        let inputs = cell_cache.cells(&lookup)?;
        let textures = match TerrainTextures::load_with_cache(
            &connection,
            &staging.join("vfs"),
            &inputs,
            &default_textures,
            &mut texture_cache,
        ) {
            Ok(textures) => textures,
            Err(error) => {
                report.lod_warnings.push(format!("worldspace {editor_id} ({worldspace_id:08X}) terrain LOD skipped: invalid material inputs: {error:#}"));
                if let Some(settings) = world_settings.last_mut() {
                    settings["status"] = serde_json::json!("invalid_material");
                    settings["material_error"] = serde_json::json!(format!("{error:#}"));
                }
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
        };
        let jobs = terrain_jobs(*worldspace_id, origin, &inputs);
        // Validate before fingerprinting: a cache hit cannot bypass source checks.
        let validation = jobs
            .iter()
            .try_for_each(|job| validate_chunk_members(job.key, &job.members));
        let mut cell_hashes = std::collections::HashMap::with_capacity(inputs.len());
        if validation.is_ok() {
            for cell in &inputs {
                cell_hashes.insert(cell.cell_id, cell.fingerprint());
            }
        }
        let (accepted, reused, refusals, content_error, gpu_chunks, fallback_chunks) = {
            let tx = connection.unchecked_transaction()?;
            let mut accepted = Vec::with_capacity(jobs.len());
            let mut reused = 0u64;
            let mut gpu_chunks = 0u64;
            let mut fallback_chunks = 0u64;
            let mut refusals = BTreeMap::<String, u64>::new();
            let mut content_error = validation.err();
            for batch in jobs.chunks(compiler_pool.current_num_threads()) {
                if content_error.is_some() {
                    break;
                }
                interrupt(cancellation)?;
                // Prepare and encode a bounded chunk batch; SQLite stays on this thread.
                let compiled: Result<Vec<_>> = compiler_pool.install(|| {
                    batch
                        .par_iter()
                        .map(|job| {
                            let input_hash = job.fingerprint_for_encoder(
                                origin,
                                &cell_hashes,
                                &textures,
                                config.lod_texture_encoder,
                            )?;
                            let refusal = if let Some(reuse) = &reuse {
                                match reuse.chunk(job, origin, &input_hash) {
                                    Ok(chunk) => {
                                        return Ok(LodBatchItem::Reused(chunk, input_hash));
                                    }
                                    Err(error) => Some(format!("{error:#}")),
                                }
                            } else {
                                None
                            };
                            let (geometry, atlas) = job.prepare(origin, &textures)?;
                            Ok(LodBatchItem::Prepared(geometry, atlas, input_hash, refusal))
                        })
                        .collect()
                });
                let items = match compiled {
                    Ok(chunks) => chunks,
                    Err(error) => {
                        content_error = Some(error);
                        break;
                    }
                };
                interrupt(cancellation)?;
                let mut chunks = (0..items.len()).map(|_| None).collect::<Vec<_>>();
                let mut prepared = Vec::new();
                let mut atlases = Vec::new();
                for (position, item) in items.into_iter().enumerate() {
                    match item {
                        LodBatchItem::Reused(chunk, input_hash) => {
                            chunks[position] = Some((chunk, input_hash, true, true, None))
                        }
                        LodBatchItem::Prepared(geometry, atlas, input_hash, refusal) => {
                            prepared.push((position, geometry, input_hash, refusal));
                            atlases.push(atlas);
                        }
                    }
                }
                if !atlases.is_empty() && !gpu_attempted {
                    gpu_attempted = true;
                    if let TextureEncoder::Gpu { quality, batch_mb } = config.lod_texture_encoder {
                        match create_lod_gpu(quality, batch_mb) {
                            Ok(encoder) => {
                                report.notices.push(format!(
                                    "Terrain atlas GPU encoder: {} (quality {quality}, batch {} MiB)",
                                    encoder.adapter_name,
                                    encoder.batch_bytes >> 20
                                ));
                                gpu = Some(encoder);
                            }
                            Err(error) => report.notices.push(format!(
                                "Terrain atlas GPU unavailable ({error:#}); using CPU encoding"
                            )),
                        }
                    }
                }
                // GPU post-processing and validation each use this many workers.
                let post_threads = (compiler_pool.current_num_threads() / 2).max(1);
                let encoded = compiler_pool
                    .install(|| TerrainAtlas::encode_batch(atlases, gpu.as_ref(), post_threads));
                let encoded = match encoded {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        content_error = Some(error);
                        break;
                    }
                };
                ensure!(
                    encoded.len() == prepared.len(),
                    "terrain atlas batch returned the wrong result count"
                );
                let finished: Result<Vec<_>> = compiler_pool.install(|| {
                    prepared
                        .into_par_iter()
                        .zip(encoded)
                        .map(|((position, geometry, input_hash, refusal), atlas)| {
                            Ok((
                                position,
                                geometry.finish(&atlas.bytes)?,
                                input_hash,
                                atlas.gpu_used,
                                atlas.fallback_reason,
                                refusal,
                            ))
                        })
                        .collect()
                });
                let finished = match finished {
                    Ok(finished) => finished,
                    Err(error) => {
                        content_error = Some(error);
                        break;
                    }
                };
                for (position, chunk, input_hash, gpu_used, fallback_reason, refusal) in finished {
                    // A CPU fallback after a GPU attempt is not GPU reuse proof, so a later
                    // healthy GPU run recomputes that chunk. This costs a retry, not a wrong reuse.
                    let reusable =
                        matches!(config.lod_texture_encoder, TextureEncoder::Cpu) || gpu_used;
                    if gpu_used {
                        gpu_chunks += 1;
                    } else if matches!(config.lod_texture_encoder, TextureEncoder::Gpu { .. }) {
                        fallback_chunks += 1;
                    }
                    if let Some(reason) = fallback_reason
                        && !report
                            .notices
                            .iter()
                            .any(|notice| notice.starts_with("Terrain atlas GPU fallback:"))
                    {
                        report
                            .notices
                            .push(format!("Terrain atlas GPU fallback: {reason}"));
                    }
                    chunks[position] = Some((chunk, input_hash, false, reusable, refusal));
                }
                if content_error.is_some() {
                    break;
                }
                interrupt(cancellation)?;
                for (chunk, input_hash, hit, reusable, refusal) in chunks.into_iter().flatten() {
                    publish_chunk(&tx, staging, &chunk)?;
                    let relative = shared::lod::chunk_payload_path(chunk.key);
                    accepted.push((relative, input_hash, hash_bytes(&chunk.glb), reusable));
                    reused += u64::from(hit);
                    if let Some(reason) = refusal {
                        *refusals.entry(reason).or_default() += 1;
                    }
                }
                // Batch updates are replaceable; the committed totals below are awaited.
                // Awaiting here would hold a non-Send SQLite transaction across suspension.
                let _ = progress_tx.try_send(ProgressEvent::new(
                    ProgressStage::LodChunks,
                    index as u64,
                    worlds.len() as u64,
                    None,
                    &format!(
                        "Terrain LOD {editor_id}: {}/{} chunks ({reused} reused)",
                        accepted.len(),
                        jobs.len()
                    ),
                ));
            }
            textures.verify_sources(&staging.join("vfs"))?;
            if content_error.is_some() {
                tx.rollback()?;
            } else {
                tx.commit()?;
            }
            (
                accepted,
                reused,
                refusals,
                content_error,
                gpu_chunks,
                fallback_chunks,
            )
        };
        if let Some(error) = content_error {
            let partial_world = staging.join(format!("lod/{worldspace_id:08x}"));
            if partial_world.exists() {
                fs::remove_dir_all(partial_world)?;
            }
            report.lod_warnings.push(format!("worldspace {editor_id} ({worldspace_id:08X}) terrain LOD skipped: invalid compiler content: {error:#}"));
            if let Some(settings) = world_settings.last_mut() {
                settings["status"] = serde_json::json!("invalid_content");
                settings["compiler_error"] = serde_json::json!(format!("{error:#}"));
            }
            send(
                progress_tx,
                ProgressStage::LodChunks,
                (index + 1) as u64,
                worlds.len() as u64,
                None,
                "Compiling terrain LOD chunks",
            )
            .await;
            continue 'world;
        }
        for (reason, count) in refusals {
            let message =
                format!("Terrain LOD {editor_id}: {count} cached chunks refused: {reason}");
            report.notices.push(message.clone());
            let _ = progress_tx
                .send(ProgressEvent::notice(
                    ProgressStage::LodChunks,
                    None,
                    &message,
                ))
                .await;
        }
        let message = format!(
            "Terrain LOD {editor_id}: {reused} reused, {} rebuilt, {} total chunks",
            accepted.len() as u64 - reused,
            accepted.len()
        );
        report.notices.push(message.clone());
        let _ = progress_tx
            .send(ProgressEvent::notice(
                ProgressStage::LodChunks,
                None,
                &message,
            ))
            .await;
        terrain_sources.extend(textures.source_hashes);
        report.lod_chunks += accepted.len() as u64;
        report.lod_cache_hits += reused;
        report.lod_gpu_chunks += gpu_chunks;
        report.lod_cpu_fallback_chunks += fallback_chunks;
        for (relative, input_hash, output_hash, reusable) in accepted {
            chunk_hashes.push(format!("{relative}:{output_hash}"));
            report.artifacts.push(PathBuf::from(&relative));
            if reusable {
                chunk_inputs.insert(relative, input_hash);
            }
        }
        compiled_worlds += 1;
        send(
            progress_tx,
            ProgressStage::LodChunks,
            (index + 1) as u64,
            worlds.len() as u64,
            None,
            "Compiling terrain LOD chunks",
        )
        .await;
    }
    for (path, hash) in &terrain_sources {
        ensure!(
            hash_file(&staging.join("vfs").join(path))? == *hash,
            "terrain input changed during bake: {path}"
        );
    }
    if report.lod_cache_hits > 0 {
        let message = format!(
            "Reused {}/{} terrain LOD chunks with verified inputs and payloads",
            report.lod_cache_hits, report.lod_chunks
        );
        report.notices.push(message);
    }
    chunk_hashes.sort();
    let current_plugin_hashes = plugins
        .iter()
        .map(|plugin| hash_file(plugin))
        .collect::<Result<Vec<_>>>()?;
    color_eyre::eyre::ensure!(
        current_plugin_hashes == plugin_hashes,
        "plugin inputs changed during conversion; refusing to publish an LOD build from a mixed input generation"
    );
    let lod_configuration = hash_bytes(&serde_json::to_vec(&serde_json::json!({
        "assets": configuration_hash(config)?,
        "atlas_encoder": match config.lod_texture_encoder {
            TextureEncoder::Cpu => "cpu-terrain-uastc-v1".to_owned(),
            TextureEncoder::Gpu { quality, .. } => format!("terrain-atlas{}", texture_gpu::cache_label(quality)),
        },
    }))?);
    let identity = build_identity(
        plugin_hashes,
        &chunk_hashes,
        &lod_configuration,
        &world_settings,
        &terrain_sources,
    )?;
    connection.execute(
        "INSERT OR REPLACE INTO lod_build(id, build_identity) VALUES (1, ?1)",
        rusqlite::params![identity],
    )?;
    if compiled_worlds > 0 {
        report.artifacts.push(PathBuf::from("lod-manifest.json"));
        let manifest = LodManifest {
            build_identity: identity,
            converter_schema: CONVERTER_SCHEMA_VERSION,
            world_database_schema: shared::WORLD_DATABASE_SCHEMA_VERSION,
            chunks: report.lod_chunks,
            land_texture_repeats_per_cell: shared::LAND_TEXTURE_REPEATS_PER_CELL,
            terrain_sources,
            compiler_version: crate::lod::TERRAIN_COMPILER_VERSION,
            chunk_inputs,
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(staging.join("lod-manifest.json"), &bytes)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_LOD_GPU_INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Fired once compilation has passed its early cancellation check and is about to wait on
    /// the progress channel, so a test can cancel a compile that is genuinely blocked there.
    static LOD_PROGRESS_WAIT_READY: std::cell::RefCell<Option<tokio::sync::oneshot::Sender<()>>> =
        const { std::cell::RefCell::new(None) };
}

fn create_lod_gpu(quality: u32, batch_mb: u64) -> Result<GpuUastc> {
    #[cfg(test)]
    if FAIL_LOD_GPU_INIT.with(std::cell::Cell::get) {
        bail!("injected GPU initialization failure");
    }
    // Chunk batches are much smaller than DDS streaming batches. Bound the
    // three reusable GPU slots to avoid reserving a gigabyte for small atlases.
    GpuUastc::new(quality, batch_mb.min(64))
}

/// Removes ingestion-cache blobs (and spill files) no longer referenced by
/// the published manifest. Without this, removed or replaced archives leave
/// orphaned blobs behind forever, since publication only adds.
fn prune_stale_ingestion_blobs(cache_root: &Path, manifest: &ConversionManifest) -> Result<()> {
    use std::collections::BTreeSet;
    if !cache_root.is_dir() {
        return Ok(());
    }
    let mut live = BTreeSet::new();
    let mut live_sources = BTreeSet::new();
    let mut live_reader_sources = BTreeMap::new();
    for entry in manifest.archives.values() {
        live_sources.insert(entry.source_hash.clone());
        if entry.reader_version > 0 {
            live_reader_sources.insert(entry.source_hash.clone(), entry.reader_version);
        }
        for file in &entry.files {
            live.insert(file.hash.clone());
        }
    }
    for entry in WalkDir::new(cache_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != "batches")
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        // Blob names are 64 hex chars; spill files append `.N`. Anything
        // else (probes, partials from crashed runs) is left alone.
        let hash = name.split('.').next().unwrap_or("");
        let is_blob = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if is_blob && !live.contains(hash) {
            fs::remove_file(entry.path())?;
        }
    }
    let packs = cache_root.join("batches");
    if packs.is_dir() {
        for entry in fs::read_dir(packs)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir()
                && name.len() == 64
                && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !live_sources.contains(&name)
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    prune_stale_reader_packs(cache_root, &live_reader_sources)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedLodSettings {
    origin: LodOrigin,
    sidecar: Option<LodSettings>,
}

/// Resolve against the effective staged VFS, after archive and loose overlays.
fn resolve_lod_settings(
    config: &PipelineConfig,
    vfs: &Path,
    editor_id: &str,
) -> Result<Option<ResolvedLodSettings>> {
    if let Some([x, y]) = config.lod_origins.get(editor_id) {
        return Ok(Some(ResolvedLodSettings {
            origin: LodOrigin::new(*x, *y),
            sidecar: None,
        }));
    }
    let path = sidecar_path(vfs, editor_id)?;
    match fs::metadata(&path) {
        Ok(_) => {
            let settings = LodSettings::read(&path)?;
            Ok(Some(ResolvedLodSettings {
                origin: settings.origin,
                sidecar: Some(settings),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .wrap_err_with(|| format!("failed to inspect LOD settings {}", path.display())),
    }
}

/// One build identity across ordered plugin bytes, compiled chunk payloads,
/// configuration, and converter schema (BUILD-01/02).
fn build_identity(
    plugin_hashes: &[String],
    chunk_hashes: &[String],
    configuration_hash: &str,
    world_settings: &[serde_json::Value],
    terrain_sources: &BTreeMap<String, String>,
) -> Result<String> {
    let canonical = serde_json::json!({
        "converter_schema": CONVERTER_SCHEMA_VERSION,
        "terrain_compiler": crate::lod::TERRAIN_COMPILER_VERSION,
        "world_database_schema": shared::WORLD_DATABASE_SCHEMA_VERSION,
        "configuration": configuration_hash,
        "land_texture_repeats_per_cell": shared::LAND_TEXTURE_REPEATS_PER_CELL,
        "plugins": plugin_hashes,
        "chunks": chunk_hashes,
        "world_settings": world_settings,
        "terrain_sources": terrain_sources,
    });
    Ok(hash_bytes(&serde_json::to_vec(&canonical)?))
}

#[cfg(test)]
fn publish_directory(staging: &Path, output: &Path) -> Result<()> {
    publish_directory_with_policy(staging, output, true)
}

pub(crate) fn publish_new_directory(staging: &Path, output: &Path, lock: &AssetLock) -> Result<()> {
    publish_directory_locked(staging, output, false, lock)
}

#[cfg(test)]
fn publish_directory_with_policy(staging: &Path, output: &Path, replace: bool) -> Result<()> {
    reject_symlink_output(output)?;
    let output = resolve_asset_path(output)?;
    let asset_lock = AssetLock::acquire_exclusive(&output).wrap_err_with(|| {
        format!(
            "failed to lock asset directory {} for publication",
            output.display()
        )
    })?;
    publish_directory_locked(staging, &output, replace, &asset_lock)
}

fn reject_symlink_output(output: &Path) -> Result<()> {
    match output.symlink_metadata() {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing symlinked asset output {}; use its real directory",
            output.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn publish_directory_locked(
    staging: &Path,
    output: &Path,
    replace: bool,
    lock: &AssetLock,
) -> Result<()> {
    // A metadata rebuild publishes into a new directory and never replaces an output, so it has
    // no previous pack to clean up; a post-publish notice from here has no report to reach and
    // only goes to the log.
    publish_directory_locked_with_timings(
        staging,
        output,
        replace,
        lock,
        &mut PublicationTimings::default(),
        &mut Vec::new(),
    )
}

fn publish_directory_locked_with_timings(
    staging: &Path,
    output: &Path,
    replace: bool,
    lock: &AssetLock,
    timings: &mut PublicationTimings,
    notices: &mut Vec<String>,
) -> Result<()> {
    timings.measure("destination_lock_validation", || {
        reject_symlink_output(output)?;
        ensure!(
            lock.mode() == AssetLockMode::Exclusive
                && lock.asset_dir() == resolve_asset_path(output)?,
            "publication requires the exclusive destination lock"
        );
        Ok(())
    })?;
    // Checked again under the publication lock before the old output is moved.
    timings.measure("destination_policy_validation", || {
        crate::config::check_output_dir(output)?;
        ensure!(
            replace
                || output
                    .symlink_metadata()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "metadata rebuild requires a new output directory: {}",
            output.display()
        );
        Ok(())
    })?;
    if !replace {
        return timings.measure("pack_swap", || {
            fs::rename(staging, output).wrap_err("failed to publish new metadata output")
        });
    }
    timings.measure("ownership_recovery", || {
        recover_interrupted_publication(output)
    })?;
    let (backup, _backup_lock, record_path) = timings.measure("backup_setup", || {
        // A previous pack this process could not delete may still hold the first name: an
        // owned leftover is stepped past, anything else is still refused.
        let prefix = publication_backup_prefix(output)?;
        let pid = std::process::id();
        let leftovers = read_publication_leftovers(output)?;
        let mut chosen = None;
        for attempt in 0..MAX_PUBLICATION_LEFTOVERS {
            let name = match attempt {
                0 => format!("{prefix}{pid}"),
                _ => format!("{prefix}{pid}-{attempt}"),
            };
            let candidate = output.with_file_name(&name);
            if !candidate.exists() {
                chosen = Some(candidate);
                break;
            }
            if !leftovers.contains(&name) {
                bail!("refusing to overwrite stale backup {}", candidate.display());
            }
        }
        let backup = chosen.ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "{MAX_PUBLICATION_LEFTOVERS} undeletable previous packs of {} remain; delete them to publish",
                output.display()
            )
        })?;
        let backup_lock = AssetLock::acquire_exclusive(&backup)?;
        let record_path = publication_record_path(output)?;
        Ok((backup, backup_lock, record_path))
    })?;
    if output.exists() {
        write_publication_record_with_timings(output, &backup, staging, timings)?;
        timings.measure("previous_pack_rename", || {
            fs::rename(output, &backup).wrap_err("failed to preserve previous asset output")
        })?;
        // The folder is checked once more under its backup name: something written into it
        // between the check above and the rename would otherwise be deleted with it below.
        if let Err(error) = timings.measure("previous_pack_policy_validation", || {
            crate::config::check_output_dir(&backup).map_err(Into::into)
        }) {
            if let Err(restore) = fs::rename(&backup, output) {
                bail!(
                    "{error}; the previous output could not be moved back from {} to {}: {restore}",
                    backup.display(),
                    output.display()
                );
            }
            fs::remove_file(&record_path)?;
            return Err(error);
        }
    }
    if let Err(error) = timings.measure("pack_swap", || {
        fs::rename(staging, output).map_err(Into::into)
    }) {
        if backup.exists()
            && let Err(restore_error) = fs::rename(&backup, output)
        {
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to publish converted assets and failed to restore last-good output from {}: {restore_error}",
                    backup.display()
                )
            });
        }
        if record_path.is_file() {
            fs::remove_file(&record_path)?;
        }
        return Err(error).wrap_err("failed to publish converted assets");
    }
    // The new pack is in place, so the only work left is deleting the previous pack. A failure
    // here must not fail a conversion that published successfully: it is reported as a notice,
    // and the backup moves to the leftover list, which every run retries. The record's job is
    // done, so it is removed: kept, a later change to the pack (a repair) would make recovery
    // refuse the remaining backup as an ambiguous publication. Only a failed list write keeps
    // the record, so recovery still deletes the backup.
    if backup.exists() {
        let cleanup: Result<()> = timings.measure("previous_pack_cleanup", || {
            #[cfg(test)]
            if FAIL_PREVIOUS_PACK_CLEANUP.with(std::cell::Cell::get) {
                bail!("injected previous pack cleanup failure");
            }
            fs::remove_dir_all(&backup).map_err(Into::into)
        });
        if let Err(error) = cleanup {
            push_post_publication_notice(
                notices,
                format!(
                    "published the new pack, but could not remove the previous pack {}: {error:#}; the next run deletes it, or delete it yourself to free the space",
                    backup.display()
                ),
            );
            let name = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| color_eyre::eyre::eyre!("previous pack backup has no name"));
            if let Err(list_error) = name.and_then(|name| add_publication_leftover(output, &name)) {
                push_post_publication_notice(
                    notices,
                    format!(
                        "could not list the previous pack {} for later removal: {list_error:#}; its ownership record is kept so the next run deletes it",
                        backup.display()
                    ),
                );
                return Ok(());
            }
        }
    }
    if record_path.is_file() {
        let cleanup: Result<()> = timings.measure("ownership_record_cleanup", || {
            fs::remove_file(&record_path).map_err(Into::into)
        });
        if let Err(error) = cleanup {
            push_post_publication_notice(
                notices,
                format!(
                    "published the new pack, but could not remove its ownership record {}: {error:#}; the next run retries removing it",
                    record_path.display()
                ),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicationRecord {
    destination: PathBuf,
    backup_name: String,
    previous_manifest_hash: Option<String>,
    next_manifest_hash: Option<String>,
    generated_artifacts: BTreeMap<String, ArtifactSeal>,
    next_generated_artifacts: BTreeMap<String, ArtifactSeal>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactSeal {
    size: u64,
    sha256: String,
}

fn publication_record_path(output: &Path) -> Result<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("output has no name"))?;
    Ok(output.with_file_name(format!(".{}.publication.json", name.to_string_lossy())))
}

fn manifest_hash(root: &Path) -> Result<Option<String>> {
    let path = root.join("conversion-manifest.json");
    if path.is_file() {
        Ok(Some(hash_file(&path)?))
    } else {
        Ok(None)
    }
}

// Seal only generated files here; converted outputs already have manifest hashes.
fn seal_generated_artifacts(output: &Path) -> Result<BTreeMap<String, ArtifactSeal>> {
    let manifest_path = output.join("conversion-manifest.json");
    let converted: BTreeSet<String> = if manifest_path.is_file() {
        let manifest: ConversionManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest
            .entries
            .values()
            .map(|entry| entry.output.clone())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut generated_artifacts = BTreeMap::new();
    for entry in WalkDir::new(output)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(entry.file_name().to_str(), Some("vfs" | ".ingestion-cache"))
        })
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "refusing symlink in publication backup: {}",
            entry.path().display()
        );
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(output)?
            .to_string_lossy()
            .replace('\\', "/");
        if converted.contains(&relative) || relative == "conversion-manifest.json" {
            continue;
        }
        generated_artifacts.insert(
            relative,
            ArtifactSeal {
                size: entry.metadata()?.len(),
                sha256: hash_file(entry.path())?,
            },
        );
    }
    Ok(generated_artifacts)
}

#[cfg(test)]
fn write_publication_record(output: &Path, backup: &Path, staging: &Path) -> Result<()> {
    write_publication_record_with_timings(
        output,
        backup,
        staging,
        &mut PublicationTimings::default(),
    )
}

fn write_publication_record_with_timings(
    output: &Path,
    backup: &Path,
    staging: &Path,
    timings: &mut PublicationTimings,
) -> Result<()> {
    use std::io::Write;
    let (destination, backup_name) = timings.measure("ownership_record_setup", || {
        Ok((
            resolve_asset_path(output)?,
            backup
                .file_name()
                .ok_or_else(|| color_eyre::eyre::eyre!("backup has no name"))?
                .to_string_lossy()
                .into_owned(),
        ))
    })?;
    let previous_manifest_hash =
        timings.measure("previous_manifest_hash", || manifest_hash(output))?;
    let next_manifest_hash = timings.measure("next_manifest_hash", || manifest_hash(staging))?;
    let generated_artifacts = timings.measure("previous_generated_artifact_sealing", || {
        seal_generated_artifacts(output)
    })?;
    let next_generated_artifacts = timings.measure("next_generated_artifact_sealing", || {
        seal_generated_artifacts(staging)
    })?;
    let record = PublicationRecord {
        destination,
        backup_name,
        previous_manifest_hash,
        next_manifest_hash,
        generated_artifacts,
        next_generated_artifacts,
    };
    let (record_path, mut file) = timings.measure("ownership_record_file_creation", || {
        let record_path = publication_record_path(output)?;
        let file = tempfile::NamedTempFile::new_in(record_path.parent().unwrap())?;
        Ok((record_path, file))
    })?;
    let bytes = timings.measure("ownership_record_serialization", || {
        serde_json::to_vec_pretty(&record).map_err(Into::into)
    })?;
    timings.measure("ownership_record_write", || {
        file.write_all(&bytes)?;
        drop(bytes);
        Ok(())
    })?;
    timings.measure("ownership_record_sync", || {
        file.as_file().sync_all().map_err(Into::into)
    })?;
    timings.measure("ownership_record_publication", || {
        file.persist_noclobber(record_path)?;
        Ok(())
    })?;
    Ok(())
}

/// The caller holds the destination's exclusive lock. Unowned backups are never adopted.
fn recover_interrupted_publication(output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // The record is settled before the leftover list is retried: an interrupted recovery can
    // list the record's own backup, which must stay until the record says it is garbage.
    let record_path = publication_record_path(output)?;
    let record = match record_path.symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "invalid publication ownership record"
            );
            serde_json::from_slice::<PublicationRecord>(&fs::read(&record_path)?)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let leftovers = retry_publication_leftovers(output, parent)?;
            if !output.exists() {
                let prefix = publication_backup_prefix(output)?;
                for entry in fs::read_dir(parent)? {
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    ensure!(
                        !(kind.is_dir() || kind.is_symlink())
                            || !name.starts_with(&prefix)
                            || (kind.is_dir() && leftovers.contains(&name)),
                        "unowned publication backup {}; manual recovery required",
                        entry.path().display()
                    );
                }
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        record.destination == resolve_asset_path(output)?,
        "publication record belongs to another destination"
    );
    ensure!(
        is_owned_backup_name(output, &record.backup_name)?,
        "invalid owned backup name"
    );
    let backup = parent.join(&record.backup_name);
    let backup_lock = AssetLock::acquire_exclusive(&backup)?;
    if output.exists() {
        let current = manifest_hash(output)?;
        if current == record.next_manifest_hash {
            // The output is the sealed new pack, so the record's backup is the previous pack
            // and holds nothing worth keeping. It is not re-verified: a delete that stopped
            // halfway leaves a backup that can never satisfy its seal, and demanding it would
            // fail every later run on a good pack. The strict seal stays only in the branch
            // below, where the backup becomes the output again.
            verify_package_seal(
                output,
                &record.next_manifest_hash,
                &record.next_generated_artifacts,
            )?;
            validate_publication_package(output)?;
            if !remove_publication_backup(&backup) {
                // The record's job is done: the output is the new pack. The backup moves to
                // the leftover list, which every run retries, so the next publication can
                // write its own record.
                add_publication_leftover(output, &record.backup_name)?;
            }
        } else {
            ensure!(
                !backup.exists() && current == record.previous_manifest_hash,
                "ambiguous interrupted publication; outputs left untouched"
            );
        }
    } else {
        verify_publication_seal(&backup, &record)?;
        validate_publication_package(&backup)?;
        fs::rename(&backup, output).wrap_err("failed to restore owned last-good assets")?;
    }
    fs::remove_file(record_path)?;
    // A listed backup may be the record's own, so its lock is released before the retry.
    drop(backup_lock);
    retry_publication_leftovers(output, parent)?;
    Ok(())
}

/// Deletes a backup left by an interrupted cleanup and returns whether it is gone. The output is
/// already the sealed new pack, so nothing in the backup is worth keeping: a delete that still
/// fails names the leftover and never fails a run that has a good pack.
fn remove_publication_backup(backup: &Path) -> bool {
    #[cfg(test)]
    let removed = if FAIL_PREVIOUS_PACK_CLEANUP.with(std::cell::Cell::get) {
        Err(std::io::Error::other(
            "injected previous pack cleanup failure",
        ))
    } else {
        fs::remove_dir_all(backup)
    };
    #[cfg(not(test))]
    let removed = fs::remove_dir_all(backup);
    match removed {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            warn!(
                "could not remove the previous pack {}: {error}; the next run retries, or delete it yourself to free the space",
                backup.display()
            );
            false
        }
    }
}

fn verify_publication_seal(backup: &Path, record: &PublicationRecord) -> Result<()> {
    verify_package_seal(
        backup,
        &record.previous_manifest_hash,
        &record.generated_artifacts,
    )
}

fn verify_package_seal(
    backup: &Path,
    expected_manifest_hash: &Option<String>,
    generated_artifacts: &BTreeMap<String, ArtifactSeal>,
) -> Result<()> {
    ensure!(
        backup.symlink_metadata()?.is_dir() && !backup.symlink_metadata()?.file_type().is_symlink(),
        "owned backup is not a real directory"
    );
    ensure!(
        manifest_hash(backup)? == *expected_manifest_hash,
        "backup manifest changed since publication began"
    );
    for (relative, seal) in generated_artifacts {
        let path = resolve_asset_uri(backup, &backup.join("conversion-manifest.json"), relative)?;
        ensure!(
            path.is_file()
                && fs::metadata(&path)?.len() == seal.size
                && hash_file(&path)? == seal.sha256,
            "generated backup artifact is missing or changed: {relative}"
        );
    }
    Ok(())
}

/// Most stale backups one destination keeps a name for; past this, publication stops and asks
/// for them to be deleted instead of growing the list without bound.
const MAX_PUBLICATION_LEFTOVERS: usize = 64;

/// Previous packs a run published over and then could not delete. They are owned (this
/// destination made them and sealed their replacement), so every recovery retries deleting
/// them, but they no longer block a new publication the way an in-flight record does.
fn publication_leftovers_path(output: &Path) -> Result<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("output has no name"))?;
    Ok(output.with_file_name(format!(
        ".{}.publication-leftovers.json",
        name.to_string_lossy()
    )))
}

/// A single path component under this destination's backup prefix.
fn is_owned_backup_name(output: &Path, name: &str) -> Result<bool> {
    Ok(Path::new(name).components().count() == 1
        && !name.contains(['/', '\\', ':'])
        && name.starts_with(&publication_backup_prefix(output)?))
}

fn read_publication_leftovers(output: &Path) -> Result<Vec<String>> {
    let path = publication_leftovers_path(output)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let names: Vec<String> = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("invalid publication leftover list {}", path.display()))?;
    for name in &names {
        ensure!(
            is_owned_backup_name(output, name)?,
            "invalid leftover backup name {name:?} in {}",
            path.display()
        );
    }
    Ok(names)
}

fn write_publication_leftovers(output: &Path, names: &[String]) -> Result<()> {
    use std::io::Write;
    let path = publication_leftovers_path(output)?;
    if names.is_empty() {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
    }
    let parent = path
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("leftover list has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(names)?)?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    Ok(())
}

fn add_publication_leftover(output: &Path, name: &str) -> Result<()> {
    let mut names = read_publication_leftovers(output)?;
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_owned());
    }
    write_publication_leftovers(output, &names)
}

/// Retries deleting every leftover backup and returns the ones still present. The caller holds
/// the destination's exclusive lock.
fn retry_publication_leftovers(output: &Path, parent: &Path) -> Result<Vec<String>> {
    let names = read_publication_leftovers(output)?;
    if names.is_empty() {
        return Ok(names);
    }
    let mut remaining = Vec::new();
    for name in names {
        let backup = parent.join(&name);
        // A reader still holding a leftover only delays its deletion; it never stops a run.
        let _backup_lock = match AssetLock::acquire_exclusive(&backup) {
            Ok(lock) => lock,
            Err(error) if error.is_held() => {
                warn!(
                    "could not remove the previous pack {}: {error}; the next run retries",
                    backup.display()
                );
                remaining.push(name);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if !remove_publication_backup(&backup) {
            remaining.push(name);
        }
    }
    write_publication_leftovers(output, &remaining)?;
    Ok(remaining)
}

fn publication_backup_prefix(output: &Path) -> Result<String> {
    let name = output
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("publication output has no directory name"))?;
    Ok(format!("{}.backup-", name.to_string_lossy()))
}

/// Checks structural integrity for record-owned recovery. A published pack may
/// be incomplete; recovery preserves that status rather than enforcing Play readiness.
fn validate_publication_package(backup: &Path) -> Result<()> {
    let root = fs::canonicalize(backup)?;
    for entry in WalkDir::new(&root).follow_links(false) {
        ensure!(
            !entry?.file_type().is_symlink(),
            "backup contains a symlink"
        );
    }
    let manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(root.join("conversion-manifest.json"))?)?;
    ensure!(
        (shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION..=CONVERTER_SCHEMA_VERSION)
            .contains(&manifest.schema_version),
        "backup has an unsupported conversion manifest"
    );
    ensure!(
        !manifest.entries.is_empty() || root.join("scripts/papyrus_runtime.luau").is_file(),
        "backup has no converted or generated runtime artifacts"
    );
    for entry in manifest.entries.values() {
        let path = resolve_asset_uri(&root, &root.join("conversion-manifest.json"), &entry.output)?;
        ensure!(
            path.is_file()
                && fs::metadata(&path)?.len() == entry.output_size
                && hash_file(&path)? == entry.output_hash,
            "backup output does not match its manifest: {}",
            entry.output
        );
    }
    let has_world = manifest.inputs_by_kind.get("esm").copied().unwrap_or(0) > 0
        || [
            "skyrim_world.db",
            "cell_cache.rkyv",
            "integration-report.json",
            "lod-manifest.json",
        ]
        .iter()
        .any(|name| root.join(name).exists());
    if has_world {
        let database = Connection::open_with_flags(
            root.join("skyrim_world.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String =
            database.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        ensure!(integrity == "ok", "backup database integrity failed");
        let version: u32 =
            database.query_row("SELECT version FROM schema_info", [], |row| row.get(0))?;
        ensure!(
            shared::supports_runtime_world_database_schema(version),
            "backup database schema {version} is unsupported"
        );
        ensure!(
            version >= shared::WORLD_DATABASE_LOD_SCHEMA_VERSION
                || !root.join("lod-manifest.json").exists(),
            "legacy backup database cannot advertise LOD"
        );
        read_cached_heights(
            &root.join("cell_cache.rkyv"),
            &std::collections::HashMap::new(),
        )?;
        let integration: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("integration-report.json"))?)?;
        ensure!(
            integration["passed"].is_boolean() && integration["schema_version"] == version,
            "backup integration report is invalid"
        );
        if version >= shared::WORLD_DATABASE_LOD_SCHEMA_VERSION {
            let count: u64 =
                database.query_row("SELECT count(*) FROM lod_chunks", [], |row| row.get(0))?;
            if count > 0 || root.join("lod-manifest.json").exists() {
                let lod: LodManifest =
                    serde_json::from_slice(&fs::read(root.join("lod-manifest.json"))?)?;
                let identity: String = database.query_row(
                    "SELECT build_identity FROM lod_build WHERE id=1",
                    [],
                    |row| row.get(0),
                )?;
                ensure!(
                    identity == lod.build_identity
                        && lod.chunks == count
                        && lod.world_database_schema == version
                        && lod.converter_schema == manifest.schema_version,
                    "backup LOD identity/count/schema mismatch"
                );
                let rows = database
                    .prepare("SELECT payload_path, content_hash FROM lod_chunks")?
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (relative, expected) in rows {
                    let path =
                        resolve_asset_uri(&root, &root.join("lod-manifest.json"), &relative)?;
                    ensure!(
                        hash_file(&path)? == expected,
                        "backup LOD payload hash mismatch: {relative}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Stops the worker pool and keeps the first error. The batch still drains
/// its channel and awaits the pool before returning it, so no worker writes
/// into a staging directory the caller is about to remove.
fn stop_batch(
    cancelled: &AtomicBool,
    first_error: &mut Option<color_eyre::eyre::Error>,
    error: color_eyre::eyre::Error,
) {
    cancelled.store(true, Ordering::Relaxed);
    first_error.get_or_insert(error);
}

async fn send(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    completed: u64,
    total: u64,
    current_file: Option<PathBuf>,
    message: &str,
) {
    let _ = tx
        .send(ProgressEvent::new(
            stage,
            completed,
            total,
            current_file,
            message,
        ))
        .await;
}

/// Sends a one-off line for the person watching: the CLI ends any open status line before it, so a
/// warning never splices into a redrawn line.
async fn send_notice(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    current_file: Option<PathBuf>,
    message: &str,
) {
    let _ = tx
        .send(ProgressEvent::notice(stage, current_file, message))
        .await;
}

/// Sends a progress event that also carries byte counts, which the status line weighs a stage by.
#[allow(clippy::too_many_arguments)]
async fn send_with_bytes(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    completed: u64,
    total: u64,
    current_file: Option<PathBuf>,
    message: &str,
    bytes: (u64, u64),
) {
    let _ = tx
        .send(
            ProgressEvent::new(stage, completed, total, current_file, message)
                .with_bytes(bytes.0, bytes.1),
        )
        .await;
}

/// Stops the run at the next safe point when the user interrupted it. The caller keeps the staging
/// folder, so the run can be resumed where it stopped.
fn interrupt(cancellation: &Cancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(Interrupted::new().into());
    }
    Ok(())
}

/// Test-only: presses Stop from inside the extractor after a given number of per-entry stop
/// checks of one output's run, so a test stops part way through an archive without racing it.
#[cfg(test)]
mod stop_hook {
    use super::Cancellation;
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    static CANCEL_AFTER: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());

    /// Arms cancellation for one output after the requested number of stop checks.
    pub(super) fn cancel_after(output: &Path, checks: usize) {
        // Match the pipeline's resolved path, including Windows' \\?\ prefix.
        let output =
            shared::asset_lock::resolve_asset_path(output).unwrap_or_else(|_| output.to_path_buf());
        CANCEL_AFTER.lock().unwrap().push((output, checks));
    }

    /// Counts one stop check for `output` and cancels the run once its armed count reaches zero.
    pub(super) fn tick(output: &Path, cancellation: &Cancellation) {
        let mut hooks = CANCEL_AFTER.lock().unwrap();
        if let Some(index) = hooks.iter().position(|(path, _)| path == output) {
            if hooks[index].1 == 0 {
                hooks.remove(index);
                cancellation.cancel();
            } else {
                hooks[index].1 -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_report_without_lod_fields_deserializes() {
        let mut legacy = serde_json::to_value(super::PipelineReport::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("lod_chunks");
        legacy.as_object_mut().unwrap().remove("lod_cache_hits");
        legacy.as_object_mut().unwrap().remove("lod_warnings");
        legacy.as_object_mut().unwrap().remove("lod_elapsed_ms");
        legacy
            .as_object_mut()
            .unwrap()
            .remove("publication_elapsed_ms");
        legacy
            .as_object_mut()
            .unwrap()
            .remove("publication_phase_elapsed_ms");
        legacy.as_object_mut().unwrap().remove("phase_elapsed_ms");
        legacy.as_object_mut().unwrap().remove("archive_timings");
        let report: super::PipelineReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(report.lod_chunks, 0);
        assert_eq!(report.lod_cache_hits, 0);
        assert!(report.lod_warnings.is_empty());
        assert!(report.phase_elapsed_ms.is_empty());
        assert!(report.archive_timings.is_empty());
        assert_eq!(report.lod_elapsed_ms, 0);
        assert_eq!(report.publication_elapsed_ms, 0);
        assert!(report.publication_phase_elapsed_ms.is_empty());
        assert!(report.skipped_plugins.is_empty());
        assert!(report.load_order.is_empty());
        assert!(report.selection_warnings.is_empty());
        let serialized = serde_json::to_value(report).unwrap();
        for field in ["skipped_plugins", "load_order", "selection_warnings"] {
            assert!(
                serialized.get(field).is_none(),
                "empty {field} must stay optional"
            );
        }
    }

    #[test]
    fn selection_report_round_trips_original_names_and_typed_warning() {
        let report = PipelineReport {
            skipped_plugins: vec!["K.esp".into(), "\u{212a}.esp".into()],
            load_order: vec!["Skyrim.esm".into(), "Good.esp".into()],
            selection_warnings: vec![CreationFallbackWarning {
                ccc_path: PathBuf::from("Game/Skyrim.ccc"),
                reason: "file missing".into(),
            }],
            ..Default::default()
        };
        let decoded: PipelineReport =
            serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
        assert_eq!(decoded.skipped_plugins, report.skipped_plugins);
        assert_eq!(decoded.load_order, report.load_order);
        assert_eq!(decoded.selection_warnings, report.selection_warnings);
        assert!(conversion_is_complete(&decoded));
    }

    use super::*;
    use crate::esm::cell_cache::write_cell_cache;

    fn cpu_lod_config(data: impl Into<PathBuf>, output: impl Into<PathBuf>) -> PipelineConfig {
        let mut config = PipelineConfig::new(data, output);
        config.lod_texture_encoder = TextureEncoder::Cpu;
        config
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lod_worker_leaves_current_thread_free_and_retains_leases_after_abort() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("modern");
        let ownership = Arc::new(LodWorkerOwnership {
            _runtime: vec![Arc::new(AssetLock::acquire_exclusive(&output).unwrap())],
            _repair: Some(Arc::new(
                crate::repair::OutputOwnership::acquire(&output).unwrap(),
            )),
        });
        let cancellation = Cancellation::new();
        let observed = cancellation.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let worker_output = output.clone();
        let task = tokio::spawn(run_owned_lod_worker(
            cancellation,
            Some(ownership),
            move || {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                if !observed.is_cancelled() {
                    fs::create_dir_all(&worker_output).unwrap();
                    fs::write(worker_output.join("unexpected-write"), b"bad").unwrap();
                }
                finished_tx.send(()).unwrap();
            },
        ));
        // The worker waits synchronously. A current-thread runtime must still
        // deliver this signal and run the abort rather than block in compilation.
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(AssetLock::acquire_shared(&output).is_err());
        assert!(crate::repair::OutputOwnership::acquire(&output).is_err());
        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if AssetLock::acquire_exclusive(&output).is_ok()
                    && crate::repair::OutputOwnership::acquire(&output).is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !output.exists(),
            "aborted worker must observe cancellation before writing"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lod_worker_cancellation_releases_a_full_progress_channel() {
        let temp = tempfile::tempdir().unwrap();
        let config = PipelineConfig {
            no_lod: true,
            ..cpu_lod_config(temp.path(), temp.path().join("out"))
        };
        // The receiver stays alive and undrained, so the next send can only complete by
        // cancellation.
        let (tx, _rx) = mpsc::channel(1);
        tx.send(ProgressEvent::notice(
            ProgressStage::LodChunks,
            None,
            "full",
        ))
        .await
        .unwrap();
        let probe = tx.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let cancellation = Cancellation::new();
        let trigger = cancellation.clone();
        let task = tokio::spawn(async move {
            // Same-thread hand-off: the current-thread runtime runs this task and the compiler here.
            LOD_PROGRESS_WAIT_READY.with(|ready| *ready.borrow_mut() = Some(ready_tx));
            compile_lod_chunks_with_cancel(
                &config,
                temp.path(),
                temp.path(),
                &[],
                &[],
                &tx,
                &cancellation,
                &mut PipelineReport::default(),
                None,
            )
            .await
        });
        // Cancel only once compilation has passed its early check and is waiting on the full channel.
        tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("compiler never reached the progress wait")
            .unwrap();
        assert!(
            probe
                .try_send(ProgressEvent::notice(
                    ProgressStage::LodChunks,
                    None,
                    "probe"
                ))
                .is_err(),
            "the progress channel must still be full while compilation waits"
        );
        trigger.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cancellation did not release the blocked progress send")
            .unwrap()
            .unwrap_err();
        assert!(error.downcast_ref::<Interrupted>().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_lod_skips_initialization_with_the_default_gpu_encoder() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let mut config = PipelineConfig::new(directory.path().join("Data"), &staging);
        assert!(matches!(
            config.lod_texture_encoder,
            TextureEncoder::Gpu { .. }
        ));
        config.no_lod = true;
        let (tx, mut rx) = mpsc::channel(1);
        let mut report = PipelineReport::default();
        FAIL_LOD_GPU_INIT.with(|fail| fail.set(true));
        let result = compile_lod_chunks(
            &config,
            &staging,
            &staging,
            &[],
            &[],
            &tx,
            &mut report,
            None,
        )
        .await;
        FAIL_LOD_GPU_INIT.with(|fail| fail.set(false));
        result.unwrap();
        assert_eq!(report.lod_chunks, 0);
        assert_eq!(report.lod_gpu_chunks, 0);
        assert_eq!(report.lod_cpu_fallback_chunks, 0);
        assert_eq!(
            report.notices,
            ["Terrain LOD compilation disabled by --no-lod"]
        );
        assert_eq!(rx.try_recv().unwrap().stage, ProgressStage::LodChunks);
        assert!(!staging.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gpu_init_fallback_does_not_claim_gpu_reuse_proof() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.cpu_jobs = 2;
        let cpu = run_without_progress(config.clone()).await;
        assert!(cpu.complete && cpu.lod_chunks > 0);
        let cpu_manifest: LodManifest =
            serde_json::from_slice(&fs::read(output.join("lod-manifest.json")).unwrap()).unwrap();
        assert_eq!(cpu_manifest.chunk_inputs.len() as u64, cpu.lod_chunks);
        config.lod_texture_encoder = TextureEncoder::Gpu {
            quality: 2,
            batch_mb: 16,
        };
        FAIL_LOD_GPU_INIT.with(|fail| fail.set(true));
        let fallback = run_without_progress(config.clone()).await;
        let retry = run_without_progress(config).await;
        FAIL_LOD_GPU_INIT.with(|fail| fail.set(false));
        for report in [fallback, retry] {
            assert!(report.complete);
            assert_eq!(report.lod_cache_hits, 0);
            assert_eq!(report.lod_gpu_chunks, 0);
            assert_eq!(report.lod_cpu_fallback_chunks, cpu.lod_chunks);
            assert_eq!(
                report.converted, 0,
                "LOD recipe changes must preserve ordinary assets"
            );
        }
        let manifest: LodManifest =
            serde_json::from_slice(&fs::read(output.join("lod-manifest.json")).unwrap()).unwrap();
        assert!(
            manifest.chunk_inputs.is_empty(),
            "fallback must be retried on a healthy GPU"
        );
    }

    /// An adapter-10 combined proof preserves DDS/PEX candidates but cannot prove NIF contracts.
    #[test]
    fn adapter10_combined_asset_proof_reuses_dds_and_pex_but_excludes_nif_without_relabeling() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = PipelineConfig::new(
            directory.path().join("Data"),
            directory.path().join("output"),
        );
        config.record_reader = crate::config::RecordReader::Inhouse;
        let mut identity = crate::esm::inhouse::reader_identity(config.record_reader);
        identity["adapter_version"] = serde_json::json!(10);
        let historical = serde_json::json!({
            "schema": CONVERTER_SCHEMA_VERSION,
            "texture_etc1s_quality": config.texture_fallback_quality,
            "texture_uastc_level": config.texture_uastc_level,
            "texture_zstd_level": config.texture_zstd_level,
            "texture_encoder": {"mode": "cpu"},
            "script_abi_version": config.script_abi_version,
            "record_reader": identity,
        });
        let recorded = hash_bytes(&serde_json::to_vec(&historical).unwrap());
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: recorded.clone(),
            entries: [
                (
                    "meshes/prior.nif".into(),
                    CacheEntry {
                        source_hash: "native-mesh-proof".into(),
                        output: "meshes/prior.glb".into(),
                        output_hash: "mesh-byte-proof".into(),
                        output_size: 137,
                    },
                ),
                (
                    "textures/prior.dds".into(),
                    CacheEntry {
                        source_hash: "native-texture-proof".into(),
                        output: "textures/prior.ktx2".into(),
                        output_hash: "texture-byte-proof".into(),
                        output_size: 251,
                    },
                ),
                (
                    "scripts/prior.pex".into(),
                    CacheEntry {
                        source_hash: "native-script-proof".into(),
                        output: "scripts/prior.luau".into(),
                        output_hash: "script-byte-proof".into(),
                        output_size: 389,
                    },
                ),
            ]
            .into(),
            complete: true,
            ..Default::default()
        };
        let reused = reusable_manifest(&config, manifest.clone()).unwrap();
        assert_eq!(
            reused.entries,
            [
                (
                    "textures/prior.dds".into(),
                    CacheEntry {
                        source_hash: "native-texture-proof".into(),
                        output: "textures/prior.ktx2".into(),
                        output_hash: "texture-byte-proof".into(),
                        output_size: 251,
                    },
                ),
                (
                    "scripts/prior.pex".into(),
                    CacheEntry {
                        source_hash: "native-script-proof".into(),
                        output: "scripts/prior.luau".into(),
                        output_hash: "script-byte-proof".into(),
                        output_size: 389,
                    },
                ),
            ]
            .into(),
            "combined metadata proof must retain DDS/PEX provenance and exclude unproven NIF"
        );
        assert_eq!(reused.schema_version, CONVERTER_SCHEMA_VERSION);
        assert_eq!(reused.configuration_hash, recorded);
        assert_eq!(reused.retained_mesh_schema_version, None);
        assert_eq!(reused.retained_asset_configuration_hash, None);
        let mut changed = config.clone();
        changed.texture_fallback_quality = 191;
        assert!(
            reusable_manifest(&changed, manifest.clone())
                .unwrap()
                .entries
                .is_empty()
        );
        let mut unsupported = manifest;
        let mut historical = historical;
        historical["record_reader"]["adapter_version"] = serde_json::json!(8);
        unsupported.configuration_hash = hash_bytes(&serde_json::to_vec(&historical).unwrap());
        assert!(
            reusable_manifest(&config, unsupported)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// Meshes an earlier converter of the same producer wrote, before a mesh output contract,
    /// rebuild from the published pack and from staging alike; textures and scripts stay reusable.
    #[test]
    fn meshes_from_before_a_mesh_output_contract_are_not_reused() {
        let directory = tempfile::tempdir().unwrap();
        let config = PipelineConfig::new(
            directory.path().join("Data"),
            directory.path().join("output"),
        );
        assert!(!crate::cache::MESH_OUTPUT_CONTRACTS.is_empty());
        let before = hash_bytes(
            &serde_json::to_vec(&serde_json::json!({"mesh_producer": CONVERTER_SCHEMA_VERSION}))
                .unwrap(),
        );
        let current = |kind| {
            asset_configuration_hash_for_schema(&config, CONVERTER_SCHEMA_VERSION, kind).unwrap()
        };
        assert_ne!(before, current("nif"));
        let entry = |output: &str| CacheEntry {
            source_hash: "source-proof".into(),
            output: output.into(),
            output_hash: "output-proof".into(),
            output_size: 1,
        };
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            entries: [
                ("meshes/prior.nif".into(), entry("meshes/prior.glb")),
                ("textures/prior.dds".into(), entry("textures/prior.ktx2")),
                ("scripts/prior.pex".into(), entry("scripts/prior.luau")),
            ]
            .into(),
            asset_configuration_hashes: [
                ("nif".to_owned(), before.clone()),
                ("dds".to_owned(), current("dds")),
                ("pex".to_owned(), current("pex")),
            ]
            .into(),
            complete: true,
            ..Default::default()
        };
        let reused = reusable_manifest(&config, manifest).unwrap();
        assert_eq!(
            reused.entries.keys().collect::<Vec<_>>(),
            ["scripts/prior.pex", "textures/prior.dds"]
        );

        let staged = directory.path().join("prior.glb");
        fs::write(&staged, b"glb").unwrap();
        let record = |configuration_hash: String| StagedOutput {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash,
            source_hash: "source-proof".into(),
            output_size: 3,
            output_hash: hash_file(&staged).unwrap(),
        };
        assert!(!record(before).is_current(&staged, "source-proof", &current("nif")));
        assert!(record(current("nif")).is_current(&staged, "source-proof", &current("nif")));
    }

    #[test]
    fn discovery_rejects_missing_roots() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let error = discover(&missing).unwrap_err();
        assert!(error.to_string().contains(&missing.display().to_string()));
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<walkdir::Error>()
                .and_then(walkdir::Error::io_error)
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        }));
    }

    #[test]
    fn a_dotted_output_name_keeps_its_whole_name_in_the_staging_folder() {
        let staging = staging_path(Path::new("converted/Skyrim.Converted"));
        let name = staging.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Skyrim.Converted.staging-"), "{name}");
        assert_eq!(staging.parent(), Some(Path::new("converted")));
        // And the folder it names is the one a resume finds.
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Skyrim.Converted");
        let staging = staging_path(&output);
        fs::create_dir_all(&staging).unwrap();
        assert_eq!(crate::find_resumable_staging(&output), Some((staging, 0)));
    }
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn lod_settings_change_lod_identity_without_invalidating_asset_cache() {
        let config = cpu_lod_config("/data", "/output");
        let cache_identity = configuration_hash(&config).unwrap();
        let mut changed = config;
        changed.lod_origins.insert("Tamriel".to_owned(), [-64, 32]);
        assert_eq!(configuration_hash(&changed).unwrap(), cache_identity);

        let first = build_identity(&[], &[], &cache_identity, &[], &BTreeMap::new()).unwrap();
        let second = build_identity(
            &[],
            &[],
            &cache_identity,
            &[serde_json::json!({
                "worldspace_id": 1,
                "editor_id": "Tamriel",
                "status": "ready",
                "origin": [-64, 32],
                "sidecar": null,
            })],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn changed_default_bytes_change_persisted_lod_identity_and_only_defaulted_chunks() {
        use crate::lod::{
            albedo::{LoadedDefault, TerrainTextures},
            terrain::{TerrainCellInput, terrain_jobs},
        };
        use std::collections::HashMap;

        struct Proof {
            identity: String,
            asset_identity: String,
            manifest: crate::lod::reuse::LodManifest,
            default_inputs: BTreeMap<String, String>,
            grass_inputs: BTreeMap<String, String>,
            default_outputs: BTreeMap<String, String>,
            grass_outputs: BTreeMap<String, String>,
            grass_source: String,
        }

        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path();
        let default_source = crate::terrain_pipeline::builtin_default_diffuse().unwrap();
        let default_path = vfs.join(&default_source);
        let grass_path = vfs.join("textures/landscape/grass.dds");
        fs::create_dir_all(default_path.parent().unwrap()).unwrap();
        let grass_bytes = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 16, 16),
            &mut dummy_content::rng::Rng::new(19),
        )
        .unwrap();
        fs::write(&grass_path, grass_bytes).unwrap();

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE texture_sets (id INTEGER PRIMARY KEY, diffuse_path TEXT);
                 CREATE TABLE landscape_textures (id INTEGER PRIMARY KEY, texture_set_id INTEGER);
                 INSERT INTO texture_sets VALUES (1, 'textures/landscape/grass.dds');
                 INSERT INTO landscape_textures VALUES (2, 1);",
            )
            .unwrap();
        let defaulted = TerrainCellInput {
            cell_id: 1,
            grid_x: 0,
            grid_y: 0,
            heights: vec![0.0; 33 * 33],
            vertex_colors: vec![255; 33 * 33 * 3],
            layers: Vec::new(),
        };
        let grass = TerrainCellInput {
            cell_id: 2,
            grid_x: 64,
            grid_y: 0,
            heights: vec![0.0; 33 * 33],
            vertex_colors: vec![255; 33 * 33 * 3],
            layers: (0..4)
                .map(|quadrant| shared::TerrainLayer {
                    texture_form_id: 2,
                    quadrant,
                    layer: 0,
                    is_base: true,
                    weights: Vec::new(),
                })
                .collect(),
        };
        let cells = [defaulted, grass];
        let cell_hashes: HashMap<_, _> = cells
            .iter()
            .map(|cell| (cell.cell_id, cell.fingerprint()))
            .collect();
        let origin = shared::lod::LodOrigin::new(0, 0);
        let asset_identity = configuration_hash(&cpu_lod_config("/data", "/output")).unwrap();

        // Exercise production chunk fingerprints and build identity with synthetic cells. The
        // persistence assertions use the same DB row and manifest field as compilation, without
        // running archive ingestion or the full AssetPipeline.
        let build = |seed| {
            let default_bytes = dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 16, 16),
                &mut dummy_content::rng::Rng::new(seed),
            )
            .unwrap();
            fs::write(&default_path, default_bytes).unwrap();
            let (default, reason) = LoadedDefault::load(
                &crate::terrain_pipeline::TerrainDefaultDiffuse::Image(default_source.clone()),
                vfs,
                &mut crate::lod::albedo::TerrainTextureCache::default(),
            );
            assert_eq!(reason, None);
            let textures = TerrainTextures::load(&connection, vfs, &cells, &default).unwrap();
            let mut chunk_inputs = BTreeMap::new();
            let mut chunk_hashes = Vec::new();
            let mut default_inputs = BTreeMap::new();
            let mut grass_inputs = BTreeMap::new();
            let mut default_outputs = BTreeMap::new();
            let mut grass_outputs = BTreeMap::new();

            for job in terrain_jobs(0x3c, origin, &cells) {
                let path = shared::lod::chunk_payload_path(job.key);
                let input_hash = job.fingerprint(origin, &cell_hashes, &textures).unwrap();
                let output_hash =
                    crate::cache::hash_bytes(&job.compile(origin, &textures).unwrap().glb);
                chunk_inputs.insert(path.clone(), input_hash.clone());
                chunk_hashes.push(format!("{path}:{output_hash}"));
                if job.members.iter().any(|cell| cell.cell_id == 1) {
                    default_inputs.insert(path.clone(), input_hash);
                    default_outputs.insert(path, output_hash);
                } else {
                    grass_inputs.insert(path.clone(), input_hash);
                    grass_outputs.insert(path, output_hash);
                }
            }
            chunk_hashes.sort();
            let terrain_sources = textures.source_hashes.clone();
            let identity =
                build_identity(&[], &chunk_hashes, &asset_identity, &[], &terrain_sources).unwrap();
            let manifest = crate::lod::reuse::LodManifest {
                build_identity: identity.clone(),
                converter_schema: CONVERTER_SCHEMA_VERSION,
                world_database_schema: shared::WORLD_DATABASE_SCHEMA_VERSION,
                chunks: chunk_hashes.len() as u64,
                land_texture_repeats_per_cell: shared::LAND_TEXTURE_REPEATS_PER_CELL,
                terrain_sources,
                compiler_version: crate::lod::TERRAIN_COMPILER_VERSION,
                chunk_inputs,
            };
            Proof {
                identity,
                asset_identity: asset_identity.clone(),
                manifest,
                default_inputs,
                grass_inputs,
                default_outputs,
                grass_outputs,
                grass_source: textures.source_hashes["textures/landscape/grass.dds"].clone(),
            }
        };

        let first = build(7);
        let second = build(8);
        assert_ne!(first.identity, second.identity);
        assert_ne!(first.default_inputs, second.default_inputs);
        assert_eq!(first.grass_inputs, second.grass_inputs);
        assert_ne!(first.default_outputs, second.default_outputs);
        assert_eq!(first.grass_outputs, second.grass_outputs);
        assert_eq!(first.grass_source, second.grass_source);
        assert_eq!(first.asset_identity, second.asset_identity);

        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE lod_build (id INTEGER PRIMARY KEY, build_identity TEXT NOT NULL);",
            )
            .unwrap();
        for proof in [&first, &second] {
            database
                .execute(
                    "INSERT OR REPLACE INTO lod_build(id, build_identity) VALUES (1, ?1)",
                    [&proof.identity],
                )
                .unwrap();
            let stored: String = database
                .query_row(
                    "SELECT build_identity FROM lod_build WHERE id=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let manifest: serde_json::Value =
                serde_json::from_slice(&serde_json::to_vec(&proof.manifest).unwrap()).unwrap();
            assert_eq!(stored, proof.identity);
            assert_eq!(manifest["build_identity"], stored);
        }
    }

    #[test]
    fn explicit_lod_origin_precedes_invalid_staged_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path().join("vfs");
        fs::create_dir_all(vfs.join("lodsettings")).unwrap();
        fs::write(vfs.join("lodsettings/tamriel.lod"), b"invalid").unwrap();
        let mut config = cpu_lod_config("/data", "/output");
        assert!(resolve_lod_settings(&config, &vfs, "Tamriel").is_err());
        config.lod_origins.insert("Tamriel".into(), [-2, 4]);
        assert_eq!(
            resolve_lod_settings(&config, &vfs, "Tamriel").unwrap(),
            Some(ResolvedLodSettings {
                origin: LodOrigin::new(-2, 4),
                sidecar: None,
            })
        );
    }

    #[test]
    fn lod_only_omissions_do_not_mark_full_detail_conversion_incomplete() {
        let report = PipelineReport {
            lod_warnings: vec!["worldspace without sidecar".to_owned()],
            ..Default::default()
        };
        assert!(conversion_is_complete(&report));
    }

    #[test]
    fn restores_last_good_output_after_interrupted_directory_swap() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("sentinel"), b"last good").unwrap();
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            entries: BTreeMap::from([(
                "sentinel".to_owned(),
                CacheEntry {
                    source_hash: "source".to_owned(),
                    output: "sentinel".to_owned(),
                    output_size: 9,
                    output_hash: hash_file(&backup.join("sentinel")).unwrap(),
                },
            )]),
            ..Default::default()
        };
        manifest
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        seal_test_backup(&output, &backup);

        recover_interrupted_publication(&output).unwrap();

        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"last good");
        assert!(!backup.exists());
    }

    fn seal_test_backup(output: &Path, backup: &Path) {
        fs::rename(backup, output).unwrap();
        write_publication_record(output, backup, output).unwrap();
        fs::rename(output, backup).unwrap();
    }

    fn write_test_runtime_pack(root: &Path, configuration: &str) {
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: configuration.to_owned(),
            complete: true,
            ..Default::default()
        }
        .save(&root.join("conversion-manifest.json"))
        .unwrap();
    }

    #[test]
    fn runtime_publication_generates_costs_for_only_published_assets() {
        use shared::streaming_costs::{STREAMING_COST_FILE_NAME, StreamingCostCatalog};
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let output = directory.path().join("assets");
        write_test_runtime_pack(&staging, "next");
        // Malformed assets must retain explicit unknown estimates. A leftover
        // resume artifact must not become part of the published catalog.
        fs::write(staging.join("published.glb"), b"invalid GLB").unwrap();
        fs::write(staging.join("stale.glb"), b"stale GLB").unwrap();
        fs::write(staging.join(STREAMING_COST_FILE_NAME), b"old sidecar").unwrap();
        let mut report = PipelineReport {
            artifacts: vec![
                "conversion-manifest.json".into(),
                "scripts/papyrus_runtime.luau".into(),
                "published.glb".into(),
                STREAMING_COST_FILE_NAME.into(),
            ],
            ..Default::default()
        };
        let lock = AssetLock::acquire_exclusive(&output).unwrap();
        publish_runtime_pack(&staging, &output, &mut report, &lock).unwrap();
        let fingerprint = hash_file(&output.join("conversion-manifest.json")).unwrap();
        let catalog =
            StreamingCostCatalog::load(&output.join(STREAMING_COST_FILE_NAME), &fingerprint)
                .unwrap();
        assert!(catalog.scenes.contains_key("published.glb"));
        assert!(!catalog.scenes.contains_key("stale.glb"));
        assert!(
            catalog
                .resolve_scene("published.glb", 64 * 1024 * 1024)
                .unwrap()
                .used_unknown_fallback
        );
        assert_eq!(
            fs::read(staging.join(STREAMING_COST_FILE_NAME)).unwrap(),
            b"old sidecar"
        );
        assert!(
            StreamingCostCatalog::load(&output.join(STREAMING_COST_FILE_NAME), "other-pack")
                .is_err()
        );
    }

    #[test]
    fn runtime_publication_cost_failure_preserves_previous_pack() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let output = directory.path().join("assets");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        fs::write(staging.join("conversion-manifest.json"), b"invalid JSON").unwrap();
        let mut report = PipelineReport {
            artifacts: vec!["conversion-manifest.json".into()],
            ..Default::default()
        };
        let lock = AssetLock::acquire_exclusive(&output).unwrap();
        assert!(publish_runtime_pack(&staging, &output, &mut report, &lock).is_err());
        assert_eq!(published_manifest(&output).configuration_hash, "previous");
        assert_eq!(
            fs::read(output.join("scripts/papyrus_runtime.luau")).unwrap(),
            b"return {}"
        );
    }

    #[test]
    fn recovery_handles_each_owned_publication_boundary() {
        for boundary in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let output = directory.path().join("assets");
            let backup = directory.path().join("assets.backup-123");
            let staging = directory.path().join("staging");
            write_test_runtime_pack(&output, "previous");
            write_test_runtime_pack(&staging, "next");
            write_publication_record(&output, &backup, &staging).unwrap();
            if boundary >= 1 {
                fs::rename(&output, &backup).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&staging, &output).unwrap();
            }
            if boundary >= 3 {
                fs::remove_dir_all(&backup).unwrap();
            }
            recover_interrupted_publication(&output).unwrap();
            assert!(!backup.exists());
            assert!(!publication_record_path(&output).unwrap().exists());
            assert_eq!(
                published_manifest(&output).configuration_hash,
                if boundary < 2 { "previous" } else { "next" }
            );
        }
    }

    /// A cleanup that died halfway leaves a backup that can never match its seal: files are
    /// already gone. The output is the sealed new pack, so recovery must delete the remains
    /// instead of refusing it, which would fail every later run on a good pack.
    #[test]
    fn recovery_discards_a_partially_deleted_backup_of_a_published_pack() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        let staging = directory.path().join("staging");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        write_publication_record(&output, &backup, &staging).unwrap();
        fs::rename(&output, &backup).unwrap();
        fs::rename(&staging, &output).unwrap();
        // The delete stopped after removing the previous pack's manifest and sealed script.
        fs::remove_file(backup.join("conversion-manifest.json")).unwrap();
        fs::remove_file(backup.join("scripts/papyrus_runtime.luau")).unwrap();

        recover_interrupted_publication(&output).unwrap();

        assert_eq!(published_manifest(&output).configuration_hash, "next");
        assert!(!backup.exists());
        assert!(!publication_record_path(&output).unwrap().exists());
    }

    /// A previous pack that cannot be deleted after the swap must not fail the run or wedge the
    /// next one: the new pack stays published, the run reports a notice, and recovery completes.
    #[test]
    fn failed_previous_pack_cleanup_never_blocks_this_or_the_next_publication() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let output = directory.path().join("assets");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        let backup = output.with_extension(format!("backup-{}", std::process::id()));
        let mut report = PipelineReport {
            artifacts: vec![
                "conversion-manifest.json".into(),
                "scripts/papyrus_runtime.luau".into(),
            ],
            ..Default::default()
        };
        let lock = AssetLock::acquire_exclusive(&output).unwrap();

        FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(true));
        let published = publish_runtime_pack(&staging, &output, &mut report, &lock);
        FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(false));

        published.unwrap();
        assert_eq!(published_manifest(&output).configuration_hash, "next");
        assert!(backup.is_dir(), "the leftover backup is kept, not lost");
        let backup_name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            read_publication_leftovers(&output).unwrap(),
            std::slice::from_ref(&backup_name),
            "the backup moves to the leftover list"
        );
        assert!(
            !publication_record_path(&output).unwrap().exists(),
            "a done record never meets a later change to the pack"
        );
        assert!(
            report.notices.iter().any(|notice| notice
                .contains("could not remove the previous pack")
                && notice.contains(&backup.display().to_string())),
            "notices: {:?}",
            report.notices
        );

        // The next publication in the same process, with deletes still failing, publishes
        // anyway: the first leftover's name stays taken and the second joins the list.
        let staging = directory.path().join("staging-2");
        write_test_runtime_pack(&staging, "third");
        FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(true));
        let republished = publish_runtime_pack(&staging, &output, &mut report, &lock);
        FAIL_PREVIOUS_PACK_CLEANUP.with(|fail| fail.set(false));
        republished.unwrap();
        assert_eq!(published_manifest(&output).configuration_hash, "third");
        let second_backup = output.with_extension(format!("backup-{}-1", std::process::id()));
        assert!(backup.is_dir() && second_backup.is_dir());
        assert_eq!(
            read_publication_leftovers(&output).unwrap(),
            [
                backup_name,
                second_backup
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
        assert!(!publication_record_path(&output).unwrap().exists());

        // Once deletes work again, one recovery clears both leftovers and the list.
        recover_interrupted_publication(&output).unwrap();
        assert!(!backup.exists() && !second_backup.exists());
        assert!(!publication_record_path(&output).unwrap().exists());
        assert!(!publication_leftovers_path(&output).unwrap().exists());
        assert_eq!(published_manifest(&output).configuration_hash, "third");
    }

    #[tokio::test]
    async fn recovery_v86_handles_incomplete_owned_packages_at_each_boundary() {
        for boundary in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let data = directory.path().join("Data");
            let output = directory.path().join("assets");
            let backup = directory.path().join("assets.backup-123");
            let staging = directory.path().join("staging");
            dummy_content::layout::prepare_directory(&data, false).unwrap();
            dummy_content::layout::generate(
                &data,
                dummy_content::layout::DEFAULT_SEED,
                dummy_content::layout::Formats::parse("dds,nif,pex,esm,lodsettings").unwrap(),
            )
            .unwrap();
            let mut config = cpu_lod_config(&data, &output);
            config.cpu_jobs = 2;
            let report = run_without_progress(config).await;
            assert!(report.complete);
            for root in [&output, &staging] {
                if root == &staging {
                    fs::create_dir_all(root).unwrap();
                    for entry in WalkDir::new(&output) {
                        let entry = entry.unwrap();
                        let destination = root.join(entry.path().strip_prefix(&output).unwrap());
                        if entry.file_type().is_dir() {
                            fs::create_dir_all(destination).unwrap();
                        } else {
                            fs::copy(entry.path(), destination).unwrap();
                        }
                    }
                }
                let mut manifest = published_manifest(root);
                manifest.configuration_hash =
                    if root == &output { "previous" } else { "next" }.into();
                manifest.complete = false;
                manifest
                    .failures
                    .insert("textures/missing.dds".into(), "missing texture".into());
                manifest
                    .save(&root.join("conversion-manifest.json"))
                    .unwrap();
                let report_path = root.join("integration-report.json");
                let mut report: serde_json::Value =
                    serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
                report["passed"] = serde_json::json!(false);
                fs::write(report_path, serde_json::to_vec(&report).unwrap()).unwrap();
            }
            write_publication_record(&output, &backup, &staging).unwrap();
            if boundary >= 1 {
                fs::rename(&output, &backup).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&staging, &output).unwrap();
            }
            if boundary >= 3 {
                fs::remove_dir_all(&backup).unwrap();
            }
            recover_interrupted_publication(&output).unwrap();
            assert!(output.is_dir());
            assert!(!published_manifest(&output).complete);
            assert_eq!(
                published_manifest(&output).configuration_hash,
                if boundary < 2 { "previous" } else { "next" }
            );
            assert_eq!(published_manifest(&output).failures.len(), 1);
            assert!(!backup.exists());
            assert!(!publication_record_path(&output).unwrap().exists());
        }
    }

    #[test]
    fn v15_wrap_aliases_resolve_to_sources_for_publication_and_restoration() {
        for mode in 0..=3 {
            for transfer in ["", ".opensky-srgb"] {
                let sampler = if mode == 3 {
                    String::new()
                } else {
                    format!(".opensky-wrap{mode}")
                };
                let alias = format!("textures/shared{transfer}{sampler}.ktx2");
                assert_eq!(source_texture_key(&alias).unwrap(), "textures/shared.ktx2");
                assert_eq!(
                    texture_reference_source_key(&alias).as_deref(),
                    Some("textures/shared.dds")
                );
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path();
                fs::create_dir_all(root.join("textures")).unwrap();
                fs::create_dir_all(root.join("meshes")).unwrap();
                fs::write(root.join("textures/shared.ktx2"), b"source").unwrap();
                let document = serde_json::json!({"asset":{"version":"2.0"},"images":[{"uri":format!("../{alias}")}],"textures":[{"source":0}],"materials":[{"normalTexture":{"index":0}}]});
                let mut json = serde_json::to_vec(&document).unwrap();
                while !json.len().is_multiple_of(4) {
                    json.push(b' ');
                }
                let mut glb = b"glTF".to_vec();
                glb.extend_from_slice(&2u32.to_le_bytes());
                glb.extend_from_slice(&(20u32 + json.len() as u32).to_le_bytes());
                glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
                glb.extend_from_slice(b"JSON");
                glb.extend_from_slice(&json);
                fs::write(root.join("meshes/alias.glb"), glb).unwrap();
                let expected = if transfer.is_empty() && mode == 3 {
                    vec![]
                } else if transfer.is_empty() || mode == 3 {
                    vec![PathBuf::from(&alias)]
                } else {
                    let linear = alias.replace(".opensky-srgb", "");
                    vec![PathBuf::from(&alias), PathBuf::from(linear)]
                };
                let published = publish_texture_aliases(root).unwrap();
                assert_eq!(published, expected);
                assert_eq!(fs::read(root.join(alias)).unwrap(), b"source");
                for linear in published
                    .iter()
                    .filter(|path| !path.to_string_lossy().contains("srgb"))
                {
                    assert_eq!(fs::read(root.join(linear)).unwrap(), b"source");
                }
                assert_eq!(
                    fs::read(root.join("textures/shared.ktx2")).unwrap(),
                    b"source"
                );
            }
        }
    }

    #[test]
    fn recovery_v85_preserves_backup_when_replacement_generated_file_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        let staging = directory.path().join("staging");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        write_publication_record(&output, &backup, &staging).unwrap();
        fs::rename(&output, &backup).unwrap();
        fs::rename(&staging, &output).unwrap();
        fs::remove_file(output.join("scripts/papyrus_runtime.luau")).unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert_eq!(published_manifest(&backup).configuration_hash, "previous");
        assert!(publication_record_path(&output).unwrap().is_file());
        fs::write(output.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(!backup.exists());
    }

    #[test]
    fn recovery_rejects_record_copied_from_another_destination() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        write_test_runtime_pack(&original, "original");
        write_test_runtime_pack(&backup, "unrelated");
        write_publication_record(&original, &backup, &original).unwrap();
        fs::rename(
            publication_record_path(&original).unwrap(),
            publication_record_path(&output).unwrap(),
        )
        .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert_eq!(published_manifest(&backup).configuration_hash, "unrelated");
        assert!(!output.exists());
    }

    #[test]
    fn complete_but_unowned_backup_is_never_adopted() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-other");
        fs::create_dir_all(backup.join("scripts")).unwrap();
        fs::write(backup.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        assert!(validate_publication_package(&backup).is_ok());
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(backup.is_dir());
        assert!(!output.exists());
    }

    #[test]
    fn recovery_refuses_an_owned_backup_held_by_a_reader() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        fs::create_dir_all(backup.join("scripts")).unwrap();
        fs::write(backup.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        seal_test_backup(&output, &backup);
        let reader = AssetLock::acquire_shared(&backup).unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(backup.is_dir());
        drop(reader);
        recover_interrupted_publication(&output).unwrap();
        assert!(output.is_dir());
    }

    /// A recovery interrupted after listing its record's backup leaves both behind; the next
    /// recovery settles the record first, so the listed backup is restored, not deleted.
    #[test]
    fn recovery_settles_the_record_before_retrying_a_listed_backup() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        fs::create_dir_all(backup.join("scripts")).unwrap();
        fs::write(backup.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        seal_test_backup(&output, &backup);
        add_publication_leftover(&output, "assets.backup-123").unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(output.join("scripts/papyrus_runtime.luau").is_file());
        assert!(!backup.exists());
        assert!(!publication_record_path(&output).unwrap().exists());
        assert!(!publication_leftovers_path(&output).unwrap().exists());
    }

    /// A reader holding a leftover delays its deletion without failing recovery.
    #[test]
    fn recovery_keeps_a_leftover_held_by_a_reader_and_continues() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let leftover = directory.path().join("assets.backup-9");
        write_test_runtime_pack(&output, "current");
        fs::create_dir_all(&leftover).unwrap();
        add_publication_leftover(&output, "assets.backup-9").unwrap();
        let reader = AssetLock::acquire_shared(&leftover).unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(leftover.is_dir());
        assert_eq!(
            read_publication_leftovers(&output).unwrap(),
            ["assets.backup-9"]
        );
        drop(reader);
        recover_interrupted_publication(&output).unwrap();
        assert!(!leftover.exists());
        assert!(!publication_leftovers_path(&output).unwrap().exists());
    }

    #[tokio::test]
    async fn recovery_verifies_generated_database_cache_manifest_and_chunk_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.cpu_jobs = 2;
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert!(report.lod_chunks > 0);
        let chunk = report
            .artifacts
            .iter()
            .find(|path| path.starts_with("lod"))
            .unwrap()
            .clone();
        let backup = directory.path().join("assets.backup-123");
        fs::rename(&output, &backup).unwrap();
        seal_test_backup(&output, &backup);
        for relative in [
            PathBuf::from("skyrim_world.db"),
            PathBuf::from("cell_cache.rkyv"),
            PathBuf::from("integration-report.json"),
            PathBuf::from("lod-manifest.json"),
            chunk,
        ] {
            let path = backup.join(&relative);
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            assert!(
                recover_interrupted_publication(&output).is_err(),
                "accepted missing {}",
                relative.display()
            );
            assert!(!output.exists());
            let mut corrupt = bytes.clone();
            corrupt[0] ^= 1;
            fs::write(&path, &corrupt).unwrap();
            assert!(
                recover_interrupted_publication(&output).is_err(),
                "accepted changed {}",
                relative.display()
            );
            assert!(!output.exists());
            fs::write(&path, bytes).unwrap();
        }
        recover_interrupted_publication(&output).unwrap();
        assert!(output.join("skyrim_world.db").is_file());
        assert!(!backup.exists());

        // Supported schema-16/world-3 packs retain full-detail recovery.
        fs::remove_dir_all(output.join("lod")).unwrap();
        fs::remove_file(output.join("lod-manifest.json")).unwrap();
        let mut legacy = published_manifest(&output);
        legacy.schema_version = 16;
        legacy.asset_configuration_hashes.clear();
        legacy
            .save(&output.join("conversion-manifest.json"))
            .unwrap();
        let database = Connection::open(output.join("skyrim_world.db")).unwrap();
        database.execute_batch("UPDATE schema_info SET version=3; DELETE FROM lod_chunks; DELETE FROM lod_chunks_spatial; DELETE FROM lod_build;").unwrap();
        drop(database);
        let integration_path = output.join("integration-report.json");
        let mut integration: serde_json::Value =
            serde_json::from_slice(&fs::read(&integration_path).unwrap()).unwrap();
        integration["schema_version"] = serde_json::json!(3);
        fs::write(&integration_path, serde_json::to_vec(&integration).unwrap()).unwrap();
        fs::rename(&output, &backup).unwrap();
        seal_test_backup(&output, &backup);
        recover_interrupted_publication(&output).unwrap();
        assert_eq!(published_manifest(&output).schema_version, 16);
        assert!(!output.join("lod-manifest.json").exists());
        // Grass-only schema5 has no LOD tables, and must remain recoverable.
        let database = Connection::open(output.join("skyrim_world.db")).unwrap();
        database.execute_batch("UPDATE schema_info SET version=5; DROP TABLE lod_chunks; DROP TABLE lod_chunks_spatial; DROP TABLE lod_build;").unwrap();
        drop(database);
        integration["schema_version"] = serde_json::json!(5);
        fs::write(&integration_path, serde_json::to_vec(&integration).unwrap()).unwrap();
        fs::rename(&output, &backup).unwrap();
        seal_test_backup(&output, &backup);
        recover_interrupted_publication(&output).unwrap();
        assert!(output.join("skyrim_world.db").is_file());
    }

    /// An unchanged conversion at the current producer reuses every mesh and its manifest entry.
    #[tokio::test]
    async fn current_producer_meshes_are_reused_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.cpu_jobs = 2;
        run_without_progress(config.clone()).await;
        let old: ConversionManifest =
            serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
                .unwrap();
        let meshes: BTreeMap<_, _> = old
            .entries
            .values()
            .filter(|entry| entry.output.ends_with(".glb"))
            .map(|entry| {
                (
                    entry.output.clone(),
                    fs::read(output.join(&entry.output)).unwrap(),
                )
            })
            .collect();
        assert!(!meshes.is_empty());
        let reused = run_without_progress(config.clone()).await;
        assert_eq!(reused.converted, 0);
        for (relative, bytes) in &meshes {
            assert_eq!(fs::read(output.join(relative)).unwrap(), *bytes);
        }
    }

    /// Producer 27 changed the GLB collision data, so the producer-24/26 mesh contract
    /// no longer grants reuse: every mesh and texture regenerates, scripts stay reused.
    #[tokio::test]
    async fn earlier_producer_meshes_regenerate_under_the_collision_contract() {
        for producer in [24, 26] {
            let directory = tempfile::tempdir().unwrap();
            let data = directory.path().join("Data");
            let output = directory.path().join("assets");
            dummy_content::layout::prepare_directory(&data, false).unwrap();
            dummy_content::layout::generate(
                &data,
                dummy_content::layout::DEFAULT_SEED,
                dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
            )
            .unwrap();
            let mut config = cpu_lod_config(&data, &output);
            config.cpu_jobs = 2;
            run_without_progress(config.clone()).await;
            let path = output.join("conversion-manifest.json");
            let mut old: ConversionManifest =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            old.schema_version = producer;
            old.asset_configuration_hashes.clear();
            old.configuration_hash = configuration_hash_for_schema(&config, producer).unwrap();
            old.save(&path).unwrap();
            let meshes = old
                .entries
                .values()
                .filter(|entry| entry.output.ends_with(".glb"))
                .count();
            assert!(meshes > 0);
            let textures = old
                .entries
                .values()
                .filter(|entry| entry.output.ends_with(".ktx2"))
                .count();
            let regenerated = run_without_progress(config).await;
            assert_eq!(
                regenerated.converted,
                (meshes + textures) as u64,
                "producer {producer} meshes must not be reused"
            );
        }
    }

    #[tokio::test]
    async fn invalid_world_compiler_content_is_omitted_without_partial_publication() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let staging = directory.path().join("staging");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,esm,lodsettings").unwrap(),
        )
        .unwrap();
        fs::create_dir_all(staging.join("vfs")).unwrap();
        overlay_loose_assets(&data, &staging.join("vfs"), &discover(&data).unwrap()).unwrap();
        let plugins = vec![data.join("Skyrim.esm")];
        let records =
            EsmParser::convert_plugins_with_records(&plugins, &staging.join("skyrim_world.db"))
                .unwrap();
        write_cell_cache(&records, &staging.join("cell_cache.rkyv")).unwrap();
        let bytes = fs::read(staging.join("cell_cache.rkyv")).unwrap();
        let mut cache = rkyv::from_bytes::<shared::CellCache, rkyv::rancor::Error>(&bytes).unwrap();
        let mut good_cell = cache.cells[0].clone();
        cache.cells[0].heights[0] = f32::NAN;
        fs::write(
            staging.join("cell_cache.rkyv"),
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, directory.path().join("output"));
        config.cpu_jobs = 2;
        let (tx, _rx) = mpsc::channel(64);
        let mut report = PipelineReport::default();
        let hashes = plugins
            .iter()
            .map(|path| hash_file(path).unwrap())
            .collect::<Vec<_>>();
        compile_lod_chunks(
            &config,
            &staging,
            &config.output_dir,
            &plugins,
            &hashes,
            &tx,
            &mut report,
            None,
        )
        .await
        .unwrap();
        assert_eq!(report.lod_chunks, 0);
        assert!(
            report
                .lod_warnings
                .iter()
                .any(|warning| warning.contains("invalid compiler content"))
        );
        assert!(!staging.join("lod-manifest.json").exists());
        assert!(!staging.join("lod").exists());
        let conn = Connection::open(staging.join("skyrim_world.db")).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM lod_chunks", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );

        let old_cell_id = good_cell.cell_id;
        good_cell.cell_id = 0x00ff0001;
        conn.execute(
            "INSERT INTO worldspaces(id,editor_id,flags) VALUES(2,'ValidWorld',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cells(id,worldspace_id,grid_x,grid_y,flags) SELECT ?1,2,grid_x,grid_y,flags FROM cells WHERE id=?2",
            rusqlite::params![good_cell.cell_id, old_cell_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO land(cell_id,heightmap,vtex,vclr,normals) SELECT ?1,heightmap,vtex,vclr,normals FROM land WHERE cell_id=?2",
            rusqlite::params![good_cell.cell_id, old_cell_id],
        ).unwrap();
        cache.cells.push(good_cell);
        fs::write(
            staging.join("cell_cache.rkyv"),
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        config.lod_origins.insert("ValidWorld".into(), [0, 0]);
        let mut mixed = PipelineReport::default();
        compile_lod_chunks(
            &config,
            &staging,
            &config.output_dir,
            &plugins,
            &hashes,
            &tx,
            &mut mixed,
            None,
        )
        .await
        .unwrap();
        assert!(mixed.lod_chunks > 0);
        assert!(
            mixed
                .lod_warnings
                .iter()
                .any(|warning| warning.contains("invalid compiler content"))
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM lod_chunks WHERE worldspace_id != 2",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert!(staging.join("lod-manifest.json").is_file());
    }

    #[tokio::test]
    async fn v121_late_batch_failure_rolls_back_entire_world() {
        use dummy_content::{esm, layout};
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let staging = directory.path().join("staging");
        layout::prepare_directory(&data, false).unwrap();
        layout::generate(
            &data,
            layout::DEFAULT_SEED,
            layout::Formats::parse("dds,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let cells = [
            esm::Cell {
                grid_x: 0,
                grid_y: 0,
            },
            esm::Cell {
                grid_x: 64,
                grid_y: 0,
            },
        ];
        fs::write(
            data.join("Skyrim.esm"),
            esm::plugin(&esm::Plugin {
                author: layout::GENERATED_AUTHOR,
                worldspace: layout::GENERATED_WORLDSPACE,
                cells: &cells,
                model_path: layout::GENERATED_MODEL_PATH,
                diffuse: layout::GENERATED_DIFFUSE_PATH,
                normal_texture: layout::GENERATED_NORMAL_PATH,
            })
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(staging.join("vfs")).unwrap();
        overlay_loose_assets(&data, &staging.join("vfs"), &discover(&data).unwrap()).unwrap();
        let plugins = vec![data.join("Skyrim.esm")];
        let records =
            EsmParser::convert_plugins_with_records(&plugins, &staging.join("skyrim_world.db"))
                .unwrap();
        write_cell_cache(&records, &staging.join("cell_cache.rkyv")).unwrap();
        let mut cache = rkyv::from_bytes::<shared::CellCache, rkyv::rancor::Error>(
            &fs::read(staging.join("cell_cache.rkyv")).unwrap(),
        )
        .unwrap();
        let last = cache
            .cells
            .iter_mut()
            .max_by_key(|cell| cell.cell_id)
            .unwrap();
        last.layers.push(shared::TerrainLayer {
            texture_form_id: last.layers[0].texture_form_id,
            quadrant: 0,
            layer: 1,
            is_base: false,
            weights: vec![shared::TerrainWeight {
                vertex: 0,
                opacity: f32::NAN,
            }],
        });
        fs::write(
            staging.join("cell_cache.rkyv"),
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, directory.path().join("output"));
        config.cpu_jobs = 1;
        let hashes = plugins
            .iter()
            .map(|path| hash_file(path).unwrap())
            .collect::<Vec<_>>();
        let (tx, mut rx) = mpsc::channel(64);
        let mut report = PipelineReport::default();
        compile_lod_chunks(
            &config,
            &staging,
            &config.output_dir,
            &plugins,
            &hashes,
            &tx,
            &mut report,
            None,
        )
        .await
        .unwrap();
        let mut first_batch_published = false;
        while let Ok(event) = rx.try_recv() {
            first_batch_published |= event.message.contains("1/6 chunks");
        }
        assert!(
            first_batch_published,
            "exercise a later failure after a successful batch"
        );
        assert_eq!(report.lod_chunks, 0);
        assert_eq!(report.lod_cache_hits, 0);
        assert_eq!(report.lod_warnings.len(), 1);
        let database = Connection::open(staging.join("skyrim_world.db")).unwrap();
        for table in ["lod_chunks", "lod_chunks_spatial"] {
            assert_eq!(
                database
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, u64>(0))
                    .unwrap(),
                0
            );
        }
        assert!(!staging.join("lod/00000001").exists());
        assert!(!staging.join("lod-manifest.json").exists());
        assert!(!report.artifacts.iter().any(|path| path.starts_with("lod")));
    }

    #[tokio::test]
    async fn conversion_fails_before_staging_when_output_has_a_reader() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        let _reader = AssetLock::acquire_shared(&output).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let error = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("before conversion"));
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn publication_rejects_symlink_output_without_changing_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        let staging = directory.path().join("staging");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(real.join("sentinel"), b"original").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(publish_directory(&staging, &alias).is_err());
        assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(real.join("sentinel")).unwrap(), b"original");
        assert!(staging.is_dir());
    }

    #[test]
    fn recovery_preserves_unrelated_and_incomplete_backups() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("sentinel"), b"unrelated").unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.join("sentinel").exists());
        ConversionManifest::default()
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.exists());
    }

    #[test]
    fn dotted_output_does_not_adopt_another_outputs_backup() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Skyrim.New");
        let backup = directory.path().join("Skyrim.Old.backup-12345");
        fs::create_dir(&backup).unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(!output.exists());
        assert!(backup.exists());
        assert_eq!(
            publication_backup_prefix(&output).unwrap(),
            "Skyrim.New.backup-"
        );
    }

    #[test]
    fn recovery_rejects_missing_or_changed_manifest_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir(&backup).unwrap();
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            entries: BTreeMap::from([(
                "asset".to_owned(),
                CacheEntry {
                    source_hash: "source".to_owned(),
                    output: "asset".to_owned(),
                    output_size: 4,
                    output_hash: hash_bytes(b"good"),
                },
            )]),
            ..Default::default()
        };
        manifest
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        fs::write(backup.join("asset"), b"evil").unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.exists());
    }

    #[test]
    fn conversion_ownership_recovers_repairs_and_reuses_publication_lock() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let staging = directory.path().join("staging");
        let repair_backup = directory.path().join("repair-backup");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        fs::create_dir(&repair_backup).unwrap();
        fs::write(output.join("sentinel"), b"interrupted repair").unwrap();
        fs::write(repair_backup.join("sentinel"), b"last good").unwrap();
        let journal = output.with_file_name("assets.repair-journal.json");
        fs::write(
            &journal,
            serde_json::to_vec(&serde_json::json!({
                "backup": repair_backup,
                "paths": [["sentinel", true]],
            }))
            .unwrap(),
        )
        .unwrap();

        let output_lock = AssetLock::acquire_exclusive(&output).unwrap();
        let ownership = crate::repair::OutputOwnership::acquire(&output).unwrap();
        recover_interrupted_publication(ownership.output()).unwrap();
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"last good");
        assert!(!journal.exists());
        assert!(crate::repair::OutputOwnership::acquire(&output).is_err());
        assert!(AssetLock::acquire_shared(&output).is_err());
        publish_directory_locked(&staging, ownership.output(), true, &output_lock).unwrap();
        assert_eq!(published_manifest(&output).configuration_hash, "next");
        assert!(crate::repair::OutputOwnership::acquire(&output).is_err());
        drop(ownership);
        drop(output_lock);
        assert!(crate::repair::OutputOwnership::acquire(&output).is_ok());
        assert!(AssetLock::acquire_exclusive(&output).is_ok());
    }

    #[test]
    fn mo2_output_pruning_handles_missing_folders_and_keeps_current_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        prune_removed_mo2_outputs(staging, &[]).unwrap();

        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::write(staging.join("meshes/current.glb"), b"current").unwrap();
        fs::write(staging.join("meshes/stale.glb"), b"stale").unwrap();
        fs::write(staging.join("meshes/unrelated.txt"), b"unrelated").unwrap();
        let sources = vec![staging.join("vfs/meshes/current.nif")];
        prune_removed_mo2_outputs(staging, &sources).unwrap();
        assert!(staging.join("meshes/current.glb").is_file());
        assert!(!staging.join("meshes/stale.glb").exists());
        assert!(staging.join("meshes/unrelated.txt").is_file());
        assert!(!staging.join("textures").exists());
        assert!(!staging.join("scripts").exists());
    }

    #[test]
    fn mo2_lod_inputs_override_archive_textures_and_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path().join("vfs");
        fs::create_dir_all(vfs.join("textures/landscape")).unwrap();
        fs::create_dir_all(vfs.join("lodsettings")).unwrap();
        fs::write(vfs.join("textures/landscape/grass.dds"), b"archive").unwrap();
        fs::write(vfs.join("lodsettings/tamriel.lod"), b"archive").unwrap();
        let texture = directory.path().join("winner.dds");
        let sidecar = directory.path().join("winner.lod");
        fs::write(&texture, b"winning texture").unwrap();
        fs::write(&sidecar, b"winning sidecar").unwrap();
        let sources = BTreeMap::from([
            ("textures/landscape/grass.dds".into(), texture.clone()),
            ("lodsettings/tamriel.lod".into(), sidecar.clone()),
        ]);
        stage_mo2_lod_inputs(&vfs, &sources).unwrap();
        assert_eq!(
            fs::read(vfs.join("textures/landscape/grass.dds")).unwrap(),
            b"winning texture"
        );
        assert_eq!(
            fs::read(vfs.join("lodsettings/tamriel.lod")).unwrap(),
            b"winning sidecar"
        );
        assert_eq!(fs::read(texture).unwrap(), b"winning texture");
        assert_eq!(fs::read(sidecar).unwrap(), b"winning sidecar");
    }

    #[test]
    fn publication_refuses_to_swap_assets_held_by_a_runtime_reader() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let staging = directory.path().join("staging");
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(output.join("sentinel"), b"last good").unwrap();
        fs::write(staging.join("sentinel"), b"new build").unwrap();
        let _reader = AssetLock::acquire_shared(&output).unwrap();

        let error = publish_directory(&staging, &output).unwrap_err();

        assert!(
            format!("{error:?}").contains("another process holds an incompatible lock"),
            "unexpected publish error: {error:?}"
        );
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"last good");
        assert_eq!(fs::read(staging.join("sentinel")).unwrap(), b"new build");
    }

    #[test]
    fn maps_srgb_runtime_aliases_back_to_their_converted_source() {
        assert_eq!(
            source_texture_key("textures/effects/fire.opensky-srgb.ktx2").unwrap(),
            "textures/effects/fire.ktx2"
        );
        assert_eq!(
            source_texture_key("textures/effects/fire.ktx2").unwrap(),
            "textures/effects/fire.ktx2"
        );
    }

    #[test]
    fn preserves_staged_meshes_and_vfs_during_generated_output_invalidation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("vfs/meshes")).unwrap();
        fs::write(staging.join("meshes/resumable.glb"), b"mesh").unwrap();
        fs::write(staging.join("vfs/meshes/source.glb"), b"source").unwrap();

        invalidate_staged_generated_outputs(staging).unwrap();

        assert!(staging.join("meshes/resumable.glb").is_file());
        assert!(staging.join("vfs/meshes/source.glb").is_file());
    }

    use dummy_content::nif::{
        BodyShape, BoxBody, LegacySkin, SkinInstanceKind, SkinNormals, StaticShape,
        legacy_skin_tint_shape, legacy_skinned_shape, legacy_skinned_shape_with_normals,
        legacy_skinned_shapes, legacy_skinned_shapes_scaled, static_shape,
        static_shape_with_bodies,
    };

    const ACTOR_BONE_TRANSFORMS: [([f32; 3], [f32; 4], f32); 2] = [
        (
            [0.0, 0.0, 0.0],
            [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
            1.0,
        ),
        ([0.0, 0.0, 2.0], [0.0, 0.0, 0.0, 1.0], 1.0),
    ];

    const ACTOR_WEIGHTS: [[f32; 4]; 4] = [
        [0.7, 0.3, 0.0, 0.0],
        [0.25, 0.75, 0.0, 0.0],
        [0.5, 0.2, 0.0, 0.0],
        [0.4, 0.6, 0.0, 0.0],
    ];

    const ACTOR_PALETTE: [u16; 2] = [1, 0];

    fn actor_assets(staging: &Path) -> PathBuf {
        staging.join("vfs/meshes/actors/character/character assets")
    }

    fn actor_part_spec<'a>(name: &'a str) -> LegacySkin<'a> {
        LegacySkin {
            name,
            instance_kind: SkinInstanceKind::Dismember,
            bone_transforms: ACTOR_BONE_TRANSFORMS,
            palette: &ACTOR_PALETTE,
            weights: &ACTOR_WEIGHTS,
            with_geometry: true,
            joint_override: None,
        }
    }

    fn write_actor_part(assets: &Path, file_name: &str) {
        fs::write(
            assets.join(file_name),
            legacy_skinned_shape(&actor_part_spec(file_name)).unwrap(),
        )
        .unwrap();
    }

    fn write_actor_part_with_normals(assets: &Path, file_name: &str, normals: SkinNormals) {
        fs::write(
            assets.join(file_name),
            legacy_skinned_shape_with_normals(&actor_part_spec(file_name), normals).unwrap(),
        )
        .unwrap();
    }

    fn write_actor_body_part(assets: &Path, file_name: &str) {
        let first = actor_part_spec("BodyShapeA");
        let second = actor_part_spec("BodyShapeB");
        fs::write(
            assets.join(file_name),
            legacy_skinned_shapes(&[first, second]).unwrap(),
        )
        .unwrap();
    }

    fn write_actor_body_part_scaled(assets: &Path, file_name: &str, scale: f32) {
        let first = actor_part_spec("BodyShapeA");
        let second = actor_part_spec("BodyShapeB");
        fs::write(
            assets.join(file_name),
            legacy_skinned_shapes_scaled(&[first, second], scale).unwrap(),
        )
        .unwrap();
    }

    fn actor_skeleton_shape(name: &str) -> StaticShape<'_> {
        StaticShape {
            name,
            positions: &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: &[[0.0, 0.0, 1.0]; 3],
            uvs: &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: &[[0, 1, 2]],
            diffuse: "textures/generated_color.dds",
            normal_texture: "textures/generated_normal.dds",
        }
    }

    fn actor_skeleton_body(node_name: &str) -> BoxBody<'_> {
        BoxBody {
            node_name,
            half_extents: [0.1, 0.1, 0.1],
            transform: None,
            collision_layer: 4,
            collision_flags: 0,
            collision_response: 1,
            inner_collision_flags: 0,
            inner_collision_response: 1,
            shape: BodyShape::Box,
            motion_system: 4,
            deactivator_type: 1,
            quality_type: 4,
            mass: 1.0,
            inertia: [0.1, 0.0, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0, 0.1],
            center_of_mass: [0.0; 3],
            linear_damping: 0.1,
            angular_damping: 0.05,
            friction: 0.5,
            restitution: 0.4,
            max_linear_velocity: 104.4,
            max_angular_velocity: 31.57,
        }
    }

    struct ActorHeaderLayout {
        block_sizes: Vec<u32>,
        block_data_start: usize,
    }

    impl ActorHeaderLayout {
        fn block_start(&self, index: usize) -> usize {
            self.block_data_start
                + self.block_sizes[..index]
                    .iter()
                    .map(|size| *size as usize)
                    .sum::<usize>()
        }
    }

    fn parse_actor_header(bytes: &[u8]) -> Option<ActorHeaderLayout> {
        fn read_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
            let end = cursor.checked_add(4)?;
            let value = u32::from_le_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
            *cursor = end;
            Some(value)
        }
        fn read_u16(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
            let end = cursor.checked_add(2)?;
            let value = u16::from_le_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
            *cursor = end;
            Some(value)
        }
        fn skip_string8(bytes: &[u8], cursor: &mut usize) -> Option<()> {
            let length = usize::from(*bytes.get(*cursor)?);
            *cursor = cursor.checked_add(1 + length)?;
            bytes.get(..*cursor)?;
            Some(())
        }
        fn read_string32(bytes: &[u8], cursor: &mut usize) -> Option<String> {
            let length = read_u32(bytes, cursor)? as usize;
            let end = cursor.checked_add(length)?;
            let value = String::from_utf8_lossy(bytes.get(*cursor..end)?).into_owned();
            *cursor = end;
            Some(value)
        }

        let newline = bytes.iter().position(|byte| *byte == b'\n')?;
        let mut cursor = newline + 1;
        read_u32(bytes, &mut cursor)?;
        cursor += 1;
        read_u32(bytes, &mut cursor)?;
        let block_count = read_u32(bytes, &mut cursor)?;
        let bethesda_version = read_u32(bytes, &mut cursor)?;
        if bethesda_version > 130 {
            read_u32(bytes, &mut cursor)?;
        }
        skip_string8(bytes, &mut cursor)?;
        if bethesda_version < 131 {
            skip_string8(bytes, &mut cursor)?;
        }
        skip_string8(bytes, &mut cursor)?;
        if bethesda_version >= 103 {
            skip_string8(bytes, &mut cursor)?;
        }
        let type_count = read_u16(bytes, &mut cursor)?;
        for _ in 0..type_count {
            read_string32(bytes, &mut cursor)?;
        }
        for _ in 0..block_count {
            read_u16(bytes, &mut cursor)?;
        }
        let mut block_sizes = Vec::with_capacity(block_count as usize);
        for _ in 0..block_count {
            block_sizes.push(read_u32(bytes, &mut cursor)?);
        }
        let string_count = read_u32(bytes, &mut cursor)?;
        read_u32(bytes, &mut cursor)?;
        for _ in 0..string_count {
            read_string32(bytes, &mut cursor)?;
        }
        let group_count = read_u32(bytes, &mut cursor)?;
        for _ in 0..group_count {
            read_u32(bytes, &mut cursor)?;
        }
        Some(ActorHeaderLayout {
            block_sizes,
            block_data_start: cursor,
        })
    }

    fn write_actor_skeleton(assets: &Path, root: &str, nodes: &[&str]) {
        let bodies = nodes
            .iter()
            .map(|name| actor_skeleton_body(name))
            .collect::<Vec<_>>();
        let mut bytes = static_shape_with_bodies(&actor_skeleton_shape(root), &bodies).unwrap();
        let header = parse_actor_header(&bytes).expect("fixture header must parse");
        bytes[header.block_start(0)..header.block_start(0) + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        fs::write(assets.join("skeleton.nif"), bytes).unwrap();
    }

    fn stage_default_actor_parts(staging: &Path) {
        let assets = actor_assets(staging);
        fs::create_dir_all(&assets).unwrap();
        write_actor_skeleton(&assets, "SkeletonRoot", &["Bone0", "Bone1"]);
        for sex in ["male", "female"] {
            write_actor_body_part(&assets, &format!("{sex}body_1.nif"));
            write_actor_part(&assets, &format!("{sex}hands_1.nif"));
            write_actor_part(&assets, &format!("{sex}feet_1.nif"));
            write_actor_body_part_scaled(&assets, &format!("{sex}body_0.nif"), 0.8);
            write_actor_part(&assets, &format!("{sex}hands_0.nif"));
            write_actor_part(&assets, &format!("{sex}feet_0.nif"));
        }
    }

    fn staged_sources(staging: &Path) -> BTreeMap<PathBuf, PathBuf> {
        discover(&staging.join("vfs"))
            .unwrap()
            .into_iter()
            .map(|path| (path.clone(), path))
            .collect()
    }

    fn compose_staged_bodies(
        staging: &Path,
        fail_fast: bool,
    ) -> (PipelineReport, ConversionManifest) {
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        compose_default_bodies(
            staging,
            &staged_sources(staging),
            fail_fast,
            &mut report,
            &mut manifest,
        )
        .unwrap();
        (report, manifest)
    }

    fn glb_skin_joints(path: &Path) -> Vec<Vec<usize>> {
        let bytes = fs::read(path).unwrap();
        let gltf = gltf::Gltf::from_slice(&bytes).unwrap();
        gltf.skins()
            .map(|skin| skin.joints().map(|node| node.index()).collect())
            .collect()
    }

    #[test]
    fn recognises_only_composed_actor_body_outputs() {
        assert!(crate::compose::is_composed_output(
            "meshes/actors/character/composed/default/male/body.glb"
        ));
        assert!(!crate::compose::is_composed_output(
            "meshes/actors/character/malebody_1.glb"
        ));
        assert!(!crate::compose::is_composed_output(
            "meshes/actors/character/composed/default/male/body.nif"
        ));
        assert!(!crate::compose::is_composed_output(
            "meshes/actors/character/composed-body.glb"
        ));
    }

    /// Both bodies record raw winning inputs with logical paths and final output bytes.
    #[test]
    fn composed_body_proofs_follow_winners_and_clear_stale_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let assets = actor_assets(staging);
        let mods = staging.join("mods");
        fs::create_dir_all(&mods).unwrap();
        write_actor_skeleton(&mods, "WinningRoot", &["Bone0", "Bone1"]);
        write_actor_part(&mods, "WinningHands.nif");
        let sources = BTreeMap::from([
            (assets.join("skeleton.nif"), mods.join("skeleton.nif")),
            (
                assets.join("malehands_1.nif"),
                mods.join("WinningHands.nif"),
            ),
        ]);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        assert_eq!(manifest.derived_outputs.len(), 2);
        assert!(
            manifest.entries.is_empty(),
            "derived outputs are not synthetic sources"
        );
        for sex in ["male", "female"] {
            let key = format!("meshes/actors/character/composed/default/{sex}/body.glb");
            let proof = &manifest.derived_outputs[&key];
            assert_eq!(proof.recipe, "body-compose-v2");
            let mut expected = BTreeMap::new();
            for suffix in ["body_1.nif", "hands_1.nif", "feet_1.nif"] {
                let filename = format!("{sex}{suffix}");
                let logical = assets.join(&filename);
                let path = sources.get(&logical).unwrap_or(&logical);
                expected.insert(
                    format!("meshes/actors/character/character assets/{filename}"),
                    hash_bytes(&fs::read(path).unwrap()),
                );
            }
            expected.insert(
                "meshes/actors/character/character assets/skeleton.nif".into(),
                hash_bytes(&fs::read(mods.join("skeleton.nif")).unwrap()),
            );
            let thin: Vec<_> = ["body", "hands", "feet"]
                .into_iter()
                .map(|slot| {
                    serde_json::json!([
                        slot,
                        hash_bytes(&fs::read(assets.join(format!("{sex}{slot}_0.nif"))).unwrap()),
                        null
                    ])
                })
                .collect();
            let context =
                serde_json::json!([[["body", null], ["hands", null], ["feet", null]], thin]);
            expected.insert(
                "recipes/body-compose-v2/context.json".into(),
                hash_bytes(&serde_json::to_vec(&context).unwrap()),
            );
            assert_eq!(proof.inputs, expected);
            assert!(crate::metadata::supported_derived_proof(&key, proof));
            assert_ne!(
                proof.inputs["meshes/actors/character/character assets/skeleton.nif"],
                hash_file(&assets.join("skeleton.nif")).unwrap()
            );
            assert_eq!(
                proof.output_hash,
                hash_bytes(&fs::read(staging.join(&key)).unwrap())
            );
            assert_eq!(
                proof.output_size,
                fs::metadata(staging.join(&key)).unwrap().len()
            );
        }
        let first = manifest.derived_outputs.clone();
        write_actor_skeleton(&mods, "ChangedWinningRoot", &["Bone0", "Bone1"]);
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        for (key, old) in first {
            assert_ne!(manifest.derived_outputs[&key].inputs, old.inputs);
        }
        fs::remove_file(assets.join("femalehands_1.nif")).unwrap();
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        assert_eq!(manifest.derived_outputs.len(), 1);
        assert!(
            !staging
                .join("meshes/actors/character/composed/default/female/body.glb")
                .exists()
        );
        fs::write(mods.join("WinningHands.nif"), b"broken winning NIF").unwrap();
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        assert!(manifest.derived_outputs.is_empty());
        assert!(
            !staging
                .join("meshes/actors/character/composed/default/male/body.glb")
                .exists()
        );
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("failed to compose default male"))
        );
        assert!(report.warnings.is_empty());
    }

    /// The real prune pass changes bodies before their published hashes are finalized.
    #[test]
    fn derived_body_hashes_describe_post_prune_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let (mut report, mut manifest) = compose_staged_bodies(staging, false);
        let before = manifest.derived_outputs.clone();
        let pruned =
            MeshConverter::prune_dangling_texture_uris_with_sources(staging, &BTreeSet::new())
                .unwrap();
        assert!(pruned.iter().any(|file| !file.removed_uris.is_empty()));
        finalize_derived_outputs(staging, &mut report, &mut manifest).unwrap();
        for (key, proof) in &manifest.derived_outputs {
            assert_ne!(
                before[key].output_hash, proof.output_hash,
                "body must actually be pruned"
            );
            assert_eq!(before[key].inputs, proof.inputs);
            let bytes = fs::read(staging.join(key)).unwrap();
            assert_eq!(proof.output_hash, hash_bytes(&bytes));
            assert_eq!(proof.output_size, bytes.len() as u64);
        }
        assert_eq!(manifest.derived_outputs.len(), 2);
    }

    /// Publication preserves the valid sibling when a procedural actor part is corrupt.
    #[tokio::test]
    async fn derived_body_proofs_publish_after_pruning_and_optional_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("fixture");
        stage_default_actor_parts(&fixture);
        let data = fixture.join("vfs");
        let output = directory.path().join("output");
        let mut config = cpu_lod_config(&data, &output);
        config.no_lod = true;
        let report = run_without_progress(config.clone()).await;
        assert!(report.complete, "{:?}", report.warnings);
        let manifest = published_manifest(&output);
        assert_eq!(manifest.derived_outputs.len(), 2);
        for (key, proof) in &manifest.derived_outputs {
            assert_eq!(proof.output_hash, hash_file(&output.join(key)).unwrap());
            assert_eq!(
                proof.output_size,
                fs::metadata(output.join(key)).unwrap().len()
            );
            assert!(manifest.pruned_texture_references.contains_key(key));
        }
        fs::write(
            actor_assets(&fixture).join("femalehands_1.nif"),
            b"broken neighbor",
        )
        .unwrap();
        let report = run_without_progress(config).await;
        let manifest = published_manifest(&output);
        assert!(
            !report.complete,
            "the ordinary source conversion must report the broken NIF"
        );
        assert_eq!(manifest.derived_outputs.len(), 1);
        let male = "meshes/actors/character/composed/default/male/body.glb";
        assert_eq!(
            manifest.derived_outputs[male].output_hash,
            hash_file(&output.join(male)).unwrap()
        );
        assert!(
            !output
                .join("meshes/actors/character/composed/default/female/body.glb")
                .exists()
        );
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("failed to compose default female"))
        );
    }

    #[test]
    fn compose_default_bodies_writes_both_sexes_sharing_one_skeleton() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);

        compose_staged_bodies(staging, false);

        for sex in ["male", "female"] {
            let output = staging.join(crate::compose::composed_body_path("default", sex));
            let joints = glb_skin_joints(&output);
            assert_eq!(joints.len(), 4, "{sex} body must compose four skins");
            assert!(
                joints.windows(2).all(|pair| pair[0] == pair[1]),
                "{sex} composed skins must share one skeleton"
            );
        }
    }

    /// Both recipes' producer proofs satisfy reuse's exact input-set contract for each sex.
    #[test]
    fn producer_recipe_inputs_match_reuse_consumer_for_both_sexes() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let (_, mut manifest) = compose_staged_bodies(staging, false);
        fs::write(actor_assets(staging).join("skeleton.hkx"), b"skeleton").unwrap();
        for sex in ["male", "female"] {
            for stem in ["mt_idle", "mt_walkforward", "mt_runforward"] {
                let key = format!("meshes/actors/character/animations/{sex}/{stem}.glb");
                let source = staging
                    .join("vfs/meshes/actors/character/animations")
                    .join(sex)
                    .join(format!("{stem}.hkx"));
                fs::create_dir_all(source.parent().unwrap()).unwrap();
                fs::write(source, format!("{sex} {stem}")).unwrap();
                let inputs =
                    default_animation_input_hashes(staging, &BTreeMap::new(), sex, stem).unwrap();
                // Output encoding is exercised by the gait-writer test below; this
                // fixture isolates the raw dependency contract from HKX decoding.
                let output = staging.join(&key);
                fs::create_dir_all(output.parent().unwrap()).unwrap();
                fs::write(&output, b"proof contract fixture").unwrap();
                let proof = derived_output_proof(DEFAULT_GAIT_RECIPE, inputs, &output).unwrap();
                manifest.derived_outputs.insert(key, proof);
            }
        }
        assert_eq!(manifest.derived_outputs.len(), 8);
        for (key, proof) in &manifest.derived_outputs {
            assert!(
                crate::metadata::supported_derived_proof(key, proof),
                "{key}"
            );
            for input in proof.inputs.keys() {
                let mut missing = proof.clone();
                missing.inputs.remove(input);
                assert!(!crate::metadata::supported_derived_proof(key, &missing));
            }
            let mut extra = proof.clone();
            extra
                .inputs
                .insert("meshes/unrelated.nif".into(), "0".repeat(64));
            assert!(!crate::metadata::supported_derived_proof(key, &extra));
        }
    }

    /// Synthetic gait proof capture uses raw logical winners and a real GLB writer.
    /// HKX decoding success is covered separately by the ignored retail test.
    #[test]
    fn derived_gait_proof_capture_tracks_actual_dependencies_and_winners() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let assets = actor_assets(staging);
        fs::write(assets.join("skeleton.hkx"), b"raw synthetic skeleton HKX").unwrap();
        let mods = staging.join("mods");
        fs::create_dir_all(&mods).unwrap();
        fs::write(mods.join("clip.hkx"), b"winning clip bytes").unwrap();
        let logical_clip = staging.join("vfs/meshes/actors/character/animations/male/mt_idle.hkx");
        let sources = BTreeMap::from([(logical_clip, mods.join("clip.hkx"))]);
        let (mut report, mut manifest) = compose_staged_bodies(staging, false);
        let body_proofs = manifest.derived_outputs.clone();
        for sex in ["male", "female"] {
            let animations = staging
                .join("vfs/meshes/actors/character/animations")
                .join(sex);
            fs::create_dir_all(&animations).unwrap();
            let body = crate::mesh::build_skinned_model(
                &assets.join(format!("{sex}body_1.nif")),
                &assets.join("skeleton.nif"),
            )
            .unwrap();
            let bones = &body.skeletal_meshes.first().unwrap().bones;
            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                fs::write(
                    animations.join(format!("{stem}.hkx")),
                    format!("raw {sex} {stem}"),
                )
                .unwrap();
                let inputs = default_animation_input_hashes(staging, &sources, sex, stem).unwrap();
                let mut expected = BTreeMap::new();
                for filename in [
                    format!("{sex}body_1.nif"),
                    "skeleton.nif".into(),
                    "skeleton.hkx".into(),
                ] {
                    expected.insert(
                        format!("meshes/actors/character/character assets/{filename}"),
                        hash_bytes(&fs::read(assets.join(filename)).unwrap()),
                    );
                }
                let expected_clip = if sex == "male" && stem == "mt_idle" {
                    b"winning clip bytes".to_vec()
                } else {
                    format!("raw {sex} {stem}").into_bytes()
                };
                expected.insert(
                    format!("meshes/actors/character/animations/{sex}/{stem}.hkx"),
                    hash_bytes(&expected_clip),
                );
                assert_eq!(inputs, expected);
                let mut clip = animation::AnimationClip {
                    name: String::new(),
                    original_skeleton_name: "synthetic".into(),
                    duration_seconds: 1.0 / 30.0,
                    frame_count: 2,
                    frames_per_second: 30.0,
                    tracks: vec![animation::TransformTrack {
                        bone_index: 0,
                        bone_name: Some("Bone0".into()),
                        samples: vec![
                            animation::TransformSample {
                                translation: [0.0; 3],
                                rotation: [0.0, 0.0, 0.0, 1.0],
                                scale: [1.0; 3],
                            };
                            2
                        ],
                    }],
                };
                let relative = default_animation_output(sex, stem);
                let output = staging.join(&relative);
                animation::write_clip_glb(&mut clip, bones, stem, &output).unwrap();
                let proof = derived_output_proof("default-gait-v1", inputs, &output).unwrap();
                let key = relative;
                assert!(crate::metadata::supported_derived_proof(&key, &proof));
                assert_eq!(proof.output_hash, hash_bytes(&fs::read(&output).unwrap()));
                assert_eq!(proof.output_size, fs::metadata(&output).unwrap().len());
                manifest.derived_outputs.insert(key, proof);
            }
        }
        let before = default_animation_input_hashes(staging, &sources, "male", "mt_idle").unwrap();
        for filename in ["malehands_1.nif", "malefeet_1.nif"] {
            fs::write(assets.join(filename), b"not a gait dependency").unwrap();
        }
        assert_eq!(
            before,
            default_animation_input_hashes(staging, &sources, "male", "mt_idle").unwrap()
        );
        for path in [
            assets.join("malebody_1.nif"),
            assets.join("skeleton.nif"),
            assets.join("skeleton.hkx"),
            mods.join("clip.hkx"),
        ] {
            let original = fs::read(&path).unwrap();
            fs::write(&path, b"changed actual dependency").unwrap();
            let after =
                default_animation_input_hashes(staging, &sources, "male", "mt_idle").unwrap();
            assert_eq!(
                before
                    .iter()
                    .filter(|(key, value)| after.get(*key) != Some(*value))
                    .count(),
                1
            );
            fs::write(path, original).unwrap();
        }
        let stale_gait_proofs: BTreeMap<_, _> = manifest
            .derived_outputs
            .iter()
            .filter(|(key, _)| key.starts_with("meshes/actors/character/animations/"))
            .map(|(key, proof)| (key.clone(), proof.clone()))
            .collect();
        // Invalid HKX bytes fail locally through the real producer and remove all
        // stale gait proofs/files while leaving the composed-body neighbor intact.
        convert_default_animations(staging, &sources, false, &mut report, &mut manifest).unwrap();
        assert_eq!(manifest.derived_outputs, body_proofs);
        assert!(report.warnings.is_empty());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("failed to convert"))
        );
        for sex in ["male", "female"] {
            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                assert!(!staging.join(default_animation_output(sex, stem)).exists());
            }
        }
        // A later missing dependency also clears proofs, including proof-only remnants.
        manifest.derived_outputs.extend(stale_gait_proofs);
        fs::remove_file(assets.join("skeleton.hkx")).unwrap();
        convert_default_animations(staging, &sources, false, &mut report, &mut manifest).unwrap();
        assert_eq!(manifest.derived_outputs, body_proofs);
    }

    /// Missing final outputs lose proofs locally while a readable sibling survives.
    #[test]
    fn derived_finalization_drops_missing_output_proof_and_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let (mut report, mut manifest) = compose_staged_bodies(staging, false);
        let missing = "meshes/actors/character/composed/default/female/body.glb";
        report.artifacts = manifest.derived_outputs.keys().map(PathBuf::from).collect();
        fs::remove_file(staging.join(missing)).unwrap();
        finalize_derived_outputs(staging, &mut report, &mut manifest).unwrap();
        assert_eq!(manifest.derived_outputs.len(), 1);
        assert!(!manifest.derived_outputs.contains_key(missing));
        assert!(!report.artifacts.contains(&PathBuf::from(missing)));
        assert_eq!(report.notices.len(), 1);
        assert!(report.notices[0].contains(missing));
        assert!(report.warnings.is_empty());
    }

    fn glb_node_names(path: &Path) -> BTreeSet<String> {
        let bytes = fs::read(path).unwrap();
        let gltf = gltf::Gltf::from_slice(&bytes).unwrap();
        gltf.nodes()
            .filter_map(|node| node.name().map(str::to_owned))
            .collect()
    }

    #[test]
    fn compose_default_bodies_uses_the_winning_source_map() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);

        let mods = directory.path().join("mods");
        fs::create_dir_all(&mods).unwrap();
        write_actor_skeleton(&mods, "ModRoot", &["Bone0", "Bone1"]);
        write_actor_body_part(&mods, "malebody_1.nif");

        let assets = actor_assets(staging);
        let staged_body = assets.join("malebody_1.nif");
        fs::remove_file(&staged_body).unwrap();
        let mut sources = staged_sources(staging);
        sources.insert(assets.join("skeleton.nif"), mods.join("skeleton.nif"));
        sources.insert(staged_body, mods.join("malebody_1.nif"));

        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();

        for sex in ["male", "female"] {
            let output = staging.join(crate::compose::composed_body_path("default", sex));
            let names = glb_node_names(&output);
            assert!(
                names.contains("ModRoot"),
                "{sex} body did not use the winning skeleton: {names:?}"
            );
            assert!(
                !names.contains("SkeletonRoot"),
                "{sex} body used the losing staged skeleton: {names:?}"
            );
        }
    }

    #[test]
    fn compose_default_bodies_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let outputs = ["male", "female"]
            .map(|sex| staging.join(crate::compose::composed_body_path("default", sex)));

        compose_staged_bodies(staging, false);
        let first = outputs.each_ref().map(|path| fs::read(path).unwrap());
        let untouched_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567_890);
        for output in &outputs {
            fs::File::options()
                .write(true)
                .open(output)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(untouched_at))
                .unwrap();
        }
        let modified = outputs
            .each_ref()
            .map(|path| fs::metadata(path).unwrap().modified().unwrap());
        compose_staged_bodies(staging, false);
        let second = outputs.each_ref().map(|path| fs::read(path).unwrap());

        assert_eq!(
            first, second,
            "a second composition must not rewrite the bodies"
        );
        assert_eq!(
            modified,
            outputs
                .each_ref()
                .map(|path| fs::metadata(path).unwrap().modified().unwrap()),
            "unchanged bodies must keep their modification times"
        );
    }

    /// Every morph target in `glb`, as the mesh name and its POSITION deltas.
    fn glb_thin_morphs(glb: &Path) -> Vec<(String, Vec<[f32; 3]>)> {
        let bytes = fs::read(glb).unwrap();
        let gltf = gltf::Gltf::from_slice(&bytes).unwrap();
        let blob = gltf.blob.as_deref().unwrap();
        let mut morphs = Vec::new();
        for mesh in gltf.document.meshes() {
            let name = mesh.name().unwrap_or_default().to_owned();
            for primitive in mesh.primitives() {
                let reader = primitive.reader(|buffer| (buffer.index() == 0).then_some(blob));
                for (positions, _, _) in reader.read_morph_targets() {
                    morphs.push((name.clone(), positions.unwrap().collect()));
                }
            }
        }
        morphs
    }

    #[test]
    fn compose_default_bodies_attaches_a_thin_morph_target() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);

        compose_staged_bodies(staging, false);

        for sex in ["male", "female"] {
            let output = staging.join(crate::compose::composed_body_path("default", sex));
            let bytes = fs::read(&output).unwrap();
            let json = crate::mesh::glb_json_from_bytes(&bytes).unwrap();
            for mesh in json["meshes"].as_array().unwrap() {
                assert_eq!(mesh["weights"], serde_json::json!([0.0]), "{sex}: {mesh}");
                assert_eq!(
                    mesh["extras"]["targetNames"],
                    serde_json::json!(["thin"]),
                    "{sex}: {mesh}"
                );
            }

            let morphs = glb_thin_morphs(&output);
            assert_eq!(morphs.len(), 4, "{sex} must morph four shapes: {morphs:?}");
            let body = morphs
                .iter()
                .find(|(name, _)| name.as_str() == "BodyShapeA")
                .unwrap();
            assert_eq!(body.1[0], [0.0, 0.0, 0.0]);
            assert!((body.1[1][0] + 0.2).abs() < 1e-5, "{:?}", body.1);
            assert!(body.1[1][1].abs() < 1e-5, "{:?}", body.1);
        }
    }

    /// Thin winners and catalog skin choices must change the recorded body provenance.
    #[test]
    fn composed_body_proof_tracks_thin_winners_and_skin_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_skin_tint_hands(staging);
        let key = "meshes/actors/character/composed/default/female/body.glb";
        let (_, baseline) = compose_staged_bodies(staging, false);

        let mods = staging.join("thin-mod");
        fs::create_dir_all(&mods).unwrap();
        write_actor_body_part_scaled(&mods, "female-thin.nif", 0.4);
        let sources = BTreeMap::from([(
            actor_assets(staging).join("femalebody_0.nif"),
            mods.join("female-thin.nif"),
        )]);
        let mut report = PipelineReport::default();
        let mut thin = ConversionManifest::default();
        compose_default_bodies(staging, &sources, false, &mut report, &mut thin).unwrap();
        assert_ne!(
            baseline.derived_outputs[key].output_hash,
            thin.derived_outputs[key].output_hash
        );
        let thin_changed = baseline.derived_outputs[key].inputs != thin.derived_outputs[key].inputs;

        write_skin_catalog(staging, true);
        let mut skin = ConversionManifest::default();
        compose_default_bodies(staging, &sources, false, &mut report, &mut skin).unwrap();
        assert_ne!(
            thin.derived_outputs[key].output_hash,
            skin.derived_outputs[key].output_hash
        );
        let skin_changed = thin.derived_outputs[key].inputs != skin.derived_outputs[key].inputs;
        assert_eq!(
            (thin_changed, skin_changed),
            (true, true),
            "thin winners and catalog skin choices must each change provenance"
        );
        assert!(crate::metadata::supported_derived_proof(
            key,
            &skin.derived_outputs[key]
        ));
    }

    #[test]
    fn compose_default_bodies_resolves_thin_sources_from_logical_paths() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let heavy_mod = staging.join("heavy-mod");
        let thin_mod = staging.join("thin-mod");
        fs::create_dir_all(&heavy_mod).unwrap();
        fs::create_dir_all(&thin_mod).unwrap();
        write_actor_body_part_scaled(&heavy_mod, "malebody_1.nif", 0.7);
        write_actor_body_part_scaled(&heavy_mod, "malebody_0.nif", 0.5);
        write_actor_body_part_scaled(&thin_mod, "malebody_0.nif", 0.3);

        let assets = actor_assets(staging);
        let mut sources = staged_sources(staging);
        sources.insert(
            assets.join("malebody_1.nif"),
            heavy_mod.join("malebody_1.nif"),
        );
        sources.insert(
            assets.join("malebody_0.nif"),
            thin_mod.join("malebody_0.nif"),
        );
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        let output = staging.join(crate::compose::composed_body_path("default", "male"));
        let body = glb_thin_morphs(&output)
            .into_iter()
            .find(|(name, _)| name == "BodyShapeA")
            .unwrap();
        assert!(
            (body.1[1][0] + 0.4).abs() < 1e-5,
            "thin source delta: {:?}",
            body.1
        );

        // The heavy winner's physical sibling must not beat staged thin fallback.
        sources.remove(&assets.join("malebody_0.nif"));
        compose_default_bodies(staging, &sources, false, &mut report, &mut manifest).unwrap();
        let body = glb_thin_morphs(&output)
            .into_iter()
            .find(|(name, _)| name == "BodyShapeA")
            .unwrap();
        assert!(
            (body.1[1][0] - 0.1).abs() < 1e-5,
            "staged thin delta: {:?}",
            body.1
        );
    }

    #[test]
    fn compose_default_bodies_without_thin_parts_keeps_the_heavy_body() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        fs::remove_file(actor_assets(staging).join("malebody_0.nif")).unwrap();

        let (report, _) = compose_staged_bodies(staging, false);

        let male = staging.join(crate::compose::composed_body_path("default", "male"));
        assert!(glb_thin_morphs(&male).is_empty());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("actor_body_weight_unavailable")
                    && notice.contains("thin part missing")),
            "notices: {:?}",
            report.notices
        );
        let female = staging.join(crate::compose::composed_body_path("default", "female"));
        assert_eq!(glb_thin_morphs(&female).len(), 4);
    }

    #[test]
    fn compose_default_bodies_with_mismatched_thin_parts_keeps_the_heavy_body() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        fs::write(
            actor_assets(staging).join("malebody_0.nif"),
            legacy_skinned_shape(&actor_part_spec("BodyShapeA")).unwrap(),
        )
        .unwrap();

        let (report, _) = compose_staged_bodies(staging, false);

        let male = staging.join(crate::compose::composed_body_path("default", "male"));
        assert!(glb_thin_morphs(&male).is_empty());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("actor_body_weight_unavailable")
                    && notice.contains("skeletal mesh counts differ")),
            "notices: {:?}",
            report.notices
        );
    }

    #[test]
    fn compose_default_bodies_with_thin_parts_missing_normals_keeps_the_heavy_body() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        write_actor_part_with_normals(
            &actor_assets(staging),
            "malehands_0.nif",
            SkinNormals::Omitted,
        );

        let (report, _) = compose_staged_bodies(staging, false);

        let male = staging.join(crate::compose::composed_body_path("default", "male"));
        assert!(male.is_file(), "heavy body must remain available");
        assert!(glb_thin_morphs(&male).is_empty());
        assert!(report.notices.iter().any(|notice| {
            notice.contains("actor_body_weight_unavailable")
                && notice.contains("per-vertex normals are missing or incomplete")
        }));
    }

    /// The `KHR_materials_specular` of every material in `glb` whose name is `shape`.
    fn glb_specular(glb: &Path, shape: &str) -> Vec<(f64, Option<String>)> {
        let bytes = fs::read(glb).unwrap();
        let json = crate::mesh::glb_json_from_bytes(&bytes).unwrap();
        json["materials"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|material| material["name"] == shape)
            .map(|material| {
                let specular = &material["extensions"]["KHR_materials_specular"];
                let uri = specular["specularColorTexture"]["index"]
                    .as_u64()
                    .map(|texture| {
                        let image = json["textures"][texture as usize]["source"]
                            .as_u64()
                            .unwrap();
                        json["images"][image as usize]["uri"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    });
                (specular["specularFactor"].as_f64().unwrap(), uri)
            })
            .collect()
    }

    /// A world database holding one female Nord whose `DefaultRace` hands armature carries
    /// `SkinHandFemale_1`, written with the catalog columns or, when `with_skin_columns` is
    /// false, as a database from before them.
    fn write_skin_catalog(staging: &Path, with_skin_columns: bool) {
        let connection = rusqlite::Connection::open(staging.join("skyrim_world.db")).unwrap();
        let skin_columns = if with_skin_columns {
            ", armature_race_id INTEGER, skin_texture_set_id INTEGER"
        } else {
            ""
        };
        connection
            .execute_batch(&format!(
                "CREATE TABLE actor_models (id INTEGER PRIMARY KEY, race_id INTEGER NOT NULL, \
                 sex TEXT NOT NULL, skin_armo_id INTEGER, head_model TEXT, head_source TEXT, \
                 resolution TEXT NOT NULL);
                 CREATE TABLE actor_model_parts (npc_id INTEGER NOT NULL, slot TEXT NOT NULL, \
                 model_path TEXT NOT NULL{skin_columns}, PRIMARY KEY (npc_id, slot));
                 CREATE TABLE texture_sets (id INTEGER PRIMARY KEY, editor_id TEXT, \
                 diffuse_path TEXT, normal_path TEXT, glow_path TEXT, height_path TEXT, \
                 environment_path TEXT, mask_path TEXT, specular_path TEXT, detail_path TEXT);
                 INSERT INTO actor_models VALUES (256, 79686, 'female', 3428, NULL, NULL, 'body');
                 INSERT INTO texture_sets VALUES (257001, 'SkinHandFemale_1', \
                 'Actors\\Character\\Female\\FemaleHands_1.dds', \
                 'Actors\\Character\\Female\\FemaleHands_1_msn.dds', \
                 'Actors\\Character\\Female\\FemaleHands_1_sk.dds', NULL, NULL, NULL, \
                 'Actors\\Character\\Female\\FemaleHands_1_S.dds', NULL);"
            ))
            .unwrap();
        if with_skin_columns {
            connection
                .execute(
                    "INSERT INTO actor_model_parts VALUES (256, 'hands', \
                     'Actors\\Character\\Character Assets\\FemaleHands_1.nif', 25, 257001)",
                    [],
                )
                .unwrap();
        } else {
            connection
                .execute(
                    "INSERT INTO actor_model_parts VALUES (256, 'hands', \
                     'Actors\\Character\\Character Assets\\FemaleHands_1.nif')",
                    [],
                )
                .unwrap();
        }
    }

    fn stage_skin_tint_hands(staging: &Path) {
        stage_default_actor_parts(staging);
        let assets = actor_assets(staging);
        for sex in ["male", "female"] {
            let file_name = format!("{sex}hands_1.nif");
            fs::write(
                assets.join(&file_name),
                legacy_skin_tint_shape(&actor_part_spec(&file_name)).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn compose_default_bodies_applies_the_default_armature_skin_texture_set() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_skin_tint_hands(staging);
        write_skin_catalog(staging, true);

        compose_staged_bodies(staging, true);

        let female = staging.join(crate::compose::composed_body_path("default", "female"));
        let hands = glb_specular(&female, "femalehands_1.nif");
        assert_eq!(hands.len(), 1, "{hands:?}");
        let (factor, uri) = &hands[0];
        assert_eq!(*factor, 1.0);
        // The specular colour texture is the sRGB alias; the engine loads its linear twin.
        assert!(
            uri.as_deref().is_some_and(|uri| uri
                .contains("/textures/actors/character/female/femalehands_1_s.opensky-srgb.")),
            "{uri:?}"
        );
        // No male row: the male hands keep their NIF textures, and without `_s` no specular.
        let male = staging.join(crate::compose::composed_body_path("default", "male"));
        assert_eq!(glb_specular(&male, "malehands_1.nif"), [(0.0, None)]);
    }

    #[test]
    fn compose_default_bodies_keeps_nif_textures_with_an_older_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_skin_tint_hands(staging);
        write_skin_catalog(staging, false);

        compose_staged_bodies(staging, true);

        let female = staging.join(crate::compose::composed_body_path("default", "female"));
        assert_eq!(glb_specular(&female, "femalehands_1.nif"), [(0.0, None)]);
    }

    #[test]
    fn compose_default_bodies_skips_a_sex_without_its_parts() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let assets = actor_assets(staging);
        fs::create_dir_all(&assets).unwrap();
        write_actor_skeleton(&assets, "SkeletonRoot", &["Bone0", "Bone1"]);
        write_actor_body_part(&assets, "malebody_1.nif");
        write_actor_part(&assets, "malehands_1.nif");
        write_actor_part(&assets, "malefeet_1.nif");

        compose_staged_bodies(staging, false);

        assert!(
            staging
                .join(crate::compose::composed_body_path("default", "male"))
                .is_file()
        );
        assert!(
            !staging
                .join(crate::compose::composed_body_path("default", "female"))
                .exists()
        );
    }

    #[test]
    fn compose_default_bodies_removes_a_body_whose_part_went_missing() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let female = staging.join(crate::compose::composed_body_path("default", "female"));
        compose_staged_bodies(staging, false);
        assert!(female.is_file());

        fs::remove_file(actor_assets(staging).join("femalehands_1.nif")).unwrap();
        compose_staged_bodies(staging, false);

        assert!(
            !female.exists(),
            "a skipped sex must not keep last run's body"
        );
        assert!(
            staging
                .join(crate::compose::composed_body_path("default", "male"))
                .is_file()
        );
    }

    fn stage_a_failing_male_hands(staging: &Path) {
        let assets = actor_assets(staging);
        fs::write(
            assets.join("malehands_1.nif"),
            static_shape(&actor_skeleton_shape("StaticHands")).unwrap(),
        )
        .unwrap();
    }

    fn stage_a_malformed_male_thin_body(staging: &Path) {
        fs::write(actor_assets(staging).join("malebody_0.nif"), []).unwrap();
    }

    /// Keeps optional composition failures under the canonical derived asset key.
    #[test]
    fn compose_default_bodies_reports_optional_failure_as_a_notice() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        compose_staged_bodies(staging, false);
        let male = staging.join(crate::compose::composed_body_path("default", "male"));
        assert!(
            male.is_file(),
            "the stale body must exist before poisoning a part"
        );
        stage_a_failing_male_hands(staging);

        let (report, manifest) = compose_staged_bodies(staging, false);

        let female = staging.join(crate::compose::composed_body_path("default", "female"));
        assert!(!male.exists(), "a failed body must not leave stale output");
        assert!(female.is_file(), "the other sex must still compose");
        let key = "meshes/actors/character/composed/default/male/body.glb";
        assert!(!manifest.failures.contains_key(key));
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.starts_with(&format!("{key}: failed to compose default male"))),
            "notices: {:?}",
            report.notices
        );
    }

    fn block_male_composed_output(staging: &Path) {
        let output = staging.join(crate::compose::composed_body_path("default", "male"));
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        let partial = output.with_extension(format!("glb.{}.partial", std::process::id()));
        fs::write(partial, b"stale partial output").unwrap();
    }

    #[test]
    fn compose_default_bodies_handles_output_write_failure_without_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        compose_staged_bodies(staging, false);
        write_actor_skeleton(&actor_assets(staging), "ChangedRoot", &["Bone0", "Bone1"]);
        block_male_composed_output(staging);
        let key = "meshes/actors/character/composed/default/male/body.glb";
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        manifest
            .failures
            .insert(key.into(), "old derived failure".into());

        compose_default_bodies(
            staging,
            &staged_sources(staging),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();

        assert!(
            !staging.join(key).is_file(),
            "failed output must not be advertised"
        );
        assert!(!manifest.failures.contains_key(key));
        assert!(report.notices.iter().any(|notice| {
            notice.contains("failed to compose default male")
                && notice.contains("stale temporary GLB")
        }));
        assert!(
            staging
                .join(crate::compose::composed_body_path("default", "female"))
                .is_file(),
            "the other sex must still compose"
        );
    }

    #[test]
    fn compose_default_bodies_cleanup_failure_is_fatal_without_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let output = staging.join(crate::compose::composed_body_path("default", "male"));
        fs::create_dir_all(&output).unwrap();
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();

        let error = compose_default_bodies(
            staging,
            &staged_sources(staging),
            false,
            &mut report,
            &mut manifest,
        )
        .expect_err("failure to clear an old output must stop composition");

        let error = format!("{error:#}");
        assert!(error.contains("failed to remove"), "{error}");
        assert!(report.notices.is_empty(), "notices: {:?}", report.notices);
    }

    #[test]
    fn compose_default_bodies_contextualizes_output_failure_with_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        compose_staged_bodies(staging, false);
        write_actor_skeleton(&actor_assets(staging), "ChangedRoot", &["Bone0", "Bone1"]);
        block_male_composed_output(staging);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();

        let error = compose_default_bodies(
            staging,
            &staged_sources(staging),
            true,
            &mut report,
            &mut manifest,
        )
        .unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("failed to compose default male"), "{error}");
        assert!(error.contains("stale temporary GLB"), "{error}");
        assert!(
            !staging
                .join(crate::compose::composed_body_path("default", "male"))
                .is_file()
        );
    }

    #[test]
    fn compose_default_bodies_clears_old_derived_failure_after_success_and_skip() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        let male_key = "meshes/actors/character/composed/default/male/body.glb";
        let female_key = "meshes/actors/character/composed/default/female/body.glb";
        let mut manifest = ConversionManifest::default();
        manifest
            .failures
            .insert(male_key.into(), "old male failure".into());
        manifest
            .failures
            .insert(female_key.into(), "old female failure".into());
        manifest
            .failures
            .insert("meshes/source.nif".into(), "source failure".into());
        let mut report = PipelineReport::default();

        compose_default_bodies(
            staging,
            &staged_sources(staging),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();
        fs::remove_file(actor_assets(staging).join("femalehands_1.nif")).unwrap();
        manifest
            .failures
            .insert(female_key.into(), "old female skip failure".into());
        compose_default_bodies(
            staging,
            &staged_sources(staging),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();

        assert!(!manifest.failures.contains_key(male_key));
        assert!(!manifest.failures.contains_key(female_key));
        assert_eq!(
            manifest.failures.get("meshes/source.nif").unwrap(),
            "source failure"
        );
        assert!(
            !staging.join(female_key).exists(),
            "skipped output must be absent"
        );
    }

    #[test]
    fn compose_default_bodies_propagates_a_failure_when_fail_fast_is_set() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        stage_a_failing_male_hands(staging);

        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        let error = compose_default_bodies(
            staging,
            &staged_sources(staging),
            true,
            &mut report,
            &mut manifest,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("failed to compose default male"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn malformed_thin_preparation_reaches_fail_fast_and_clears_stale_output() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        compose_staged_bodies(staging, false);
        let key = "meshes/actors/character/composed/default/male/body.glb";
        let output = staging.join(key);
        assert!(output.is_file());
        stage_a_malformed_male_thin_body(staging);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        manifest
            .failures
            .insert(key.into(), "old derived failure".into());

        let error = compose_default_bodies(
            staging,
            &staged_sources(staging),
            true,
            &mut report,
            &mut manifest,
        )
        .expect_err("malformed thin NIF preparation must fail fast");

        assert!(format!("{error:#}").contains("failed to compose default male"));
        assert!(
            !output.exists(),
            "stale heavy body must be removed on failure"
        );
        assert!(!manifest.failures.contains_key(key));
    }

    #[test]
    fn malformed_thin_preparation_is_a_derived_notice_without_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_default_actor_parts(staging);
        compose_staged_bodies(staging, false);
        let key = "meshes/actors/character/composed/default/male/body.glb";
        let output = staging.join(key);
        assert!(output.is_file());
        stage_a_malformed_male_thin_body(staging);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        manifest
            .failures
            .insert(key.into(), "old derived failure".into());

        compose_default_bodies(
            staging,
            &staged_sources(staging),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();

        assert!(!output.exists(), "failed body must not keep stale output");
        assert!(!manifest.failures.contains_key(key));
        assert!(report.notices.iter().any(|notice| {
            notice.contains("failed to compose default male")
                && !notice.contains("actor_body_weight_unavailable")
        }));
        assert!(
            staging
                .join(crate::compose::composed_body_path("default", "female"))
                .is_file()
        );
    }

    #[test]
    fn composed_actor_bodies_survive_orphan_removal() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let male = staging.join("meshes/actors/character/composed/default/male/body.glb");
        let female = staging.join("meshes/actors/character/composed/default/female/body.glb");
        let orphan = staging.join("meshes/actors/character/stale.glb");
        for path in [&male, &female, &orphan] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"mesh").unwrap();
        }

        remove_orphan_staged_assets(staging, &[]).unwrap();

        assert!(male.is_file());
        assert!(female.is_file());
        assert!(!orphan.exists());
    }

    #[test]
    fn composed_actor_bodies_survive_mo2_pruning() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let male = staging.join("meshes/actors/character/composed/default/male/body.glb");
        let female = staging.join("meshes/actors/character/composed/default/female/body.glb");
        let orphan = staging.join("meshes/actors/character/stale.glb");
        for path in [&male, &female, &orphan] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"mesh").unwrap();
        }

        prune_removed_mo2_outputs(staging, &[]).unwrap();

        assert!(male.is_file());
        assert!(female.is_file());
        assert!(!orphan.exists());
    }

    #[test]
    fn recognises_only_composed_and_animation_derived_outputs() {
        assert!(is_derived_output(
            "meshes/actors/character/composed/default/male/body.glb"
        ));
        assert!(is_derived_output(
            "meshes/actors/character/animations/male/mt_idle.glb"
        ));
        assert!(is_derived_output(
            "meshes/actors/character/animations/female/mt_idle.glb"
        ));
        assert!(is_derived_output(
            "meshes/actors/character/animations/male/mt_walkforward.glb"
        ));
        assert!(is_derived_output(
            "meshes/actors/character/animations/female/mt_runforward.glb"
        ));
        assert!(!is_derived_output("meshes/actors/character/malebody_1.glb"));
        assert!(!is_derived_output(
            "meshes/actors/character/animations/male/mt_idle.hkx"
        ));
        assert!(!is_derived_output(
            "textures/actors/character/animations/male/mt_idle.ktx2"
        ));
        assert!(!is_derived_output(
            "meshes/actors/character/animation/male/mt_idle.glb"
        ));
    }

    #[test]
    fn animation_clips_survive_orphan_removal() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let clips = write_staged_gait_clips(staging);
        let orphan = staging.join("meshes/actors/character/stale.glb");
        fs::write(&orphan, b"clip").unwrap();

        remove_orphan_staged_assets(staging, &[]).unwrap();

        for clip in clips {
            assert!(clip.is_file(), "{} must survive", clip.display());
        }
        assert!(!orphan.exists());
    }

    #[test]
    fn animation_clips_survive_mo2_pruning() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let clips = write_staged_gait_clips(staging);
        let orphan = staging.join("meshes/actors/character/stale.glb");
        fs::write(&orphan, b"clip").unwrap();

        prune_removed_mo2_outputs(staging, &[]).unwrap();

        for clip in clips {
            assert!(clip.is_file(), "{} must survive", clip.display());
        }
        assert!(!orphan.exists());
    }

    #[test]
    fn all_gait_clips_survive_a_second_cleanup_pass() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let clips = write_staged_gait_clips(staging);

        for _ in 0..2 {
            remove_orphan_staged_assets(staging, &[]).unwrap();
            prune_removed_mo2_outputs(staging, &[]).unwrap();
        }

        for clip in clips {
            assert!(clip.is_file(), "{} must survive", clip.display());
        }
    }

    /// Keeps the six gait keys ordered and portable through report serialization.
    #[test]
    fn default_animation_clip_table_is_ordered() {
        assert_eq!(
            DEFAULT_ANIMATION_CLIPS,
            [
                ("idle", "mt_idle"),
                ("walkforward", "mt_walkforward"),
                ("runforward", "mt_runforward"),
            ]
        );
        for sex in ["male", "female"] {
            let paths: Vec<String> = DEFAULT_ANIMATION_CLIPS
                .iter()
                .map(|(_, stem)| default_animation_output(sex, stem))
                .collect();
            assert_eq!(
                paths,
                [
                    format!("meshes/actors/character/animations/{sex}/mt_idle.glb"),
                    format!("meshes/actors/character/animations/{sex}/mt_walkforward.glb"),
                    format!("meshes/actors/character/animations/{sex}/mt_runforward.glb"),
                ]
            );
            let report = PipelineReport {
                artifacts: paths.iter().map(PathBuf::from).collect(),
                ..PipelineReport::default()
            };
            let serialized = serde_json::to_value(report).unwrap();
            for (artifact, expected) in serialized["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .zip(paths)
            {
                let key = artifact.as_str().unwrap();
                assert!(!key.contains('\\'), "noncanonical gait artifact: {key}");
                assert_eq!(key, expected);
            }
        }
    }

    /// Exercises Windows-style key text even when CI runs on a Unix host.
    #[test]
    fn default_animation_output_canonicalizes_windows_asset_keys() {
        let relative = default_animation_output("FEMALE", r"Nested\MT_IDLE");
        assert_eq!(
            Path::new(&relative).to_str(),
            Some("meshes/actors/character/animations/female/nested/mt_idle.glb")
        );
    }

    #[test]
    fn convert_default_animations_is_a_no_op_without_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();

        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        convert_default_animations(staging, &BTreeMap::new(), false, &mut report, &mut manifest)
            .unwrap();

        assert!(!staging.join("meshes").exists());
        assert!(report.notices.is_empty());
    }

    #[test]
    fn missing_animation_inputs_remove_stale_outputs_and_failure_markers() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        let outputs = write_staged_gait_clips(staging);
        for output in &outputs {
            let key = output
                .strip_prefix(staging)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            manifest
                .failures
                .insert(key, "stale derived failure".into());
        }

        convert_default_animations(staging, &BTreeMap::new(), false, &mut report, &mut manifest)
            .unwrap();

        assert!(outputs.iter().all(|output| !output.exists()));
        assert!(manifest.failures.is_empty());
        assert_eq!(report.notices.len(), outputs.len());
    }

    /// Keeps missing-input notices for both sexes when only old gait markers remain.
    #[test]
    fn marker_only_gait_work_keeps_missing_actor_notices_for_both_sexes() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        for sex in ["male", "female"] {
            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                let key = default_animation_output(sex, stem);
                manifest.failures.insert(key, "stale gait failure".into());
            }
        }

        convert_default_animations(staging, &BTreeMap::new(), false, &mut report, &mut manifest)
            .unwrap();

        assert_eq!(report.notices.len(), 6);
        for sex in ["male", "female"] {
            assert_eq!(
                report
                    .notices
                    .iter()
                    .filter(|notice| notice.contains(&format!("default {sex} ")))
                    .count(),
                3,
                "notices: {:?}",
                report.notices
            );
        }
        assert!(manifest.failures.is_empty());
    }

    #[test]
    fn gait_output_directory_collision_is_fatal_and_preserved_with_or_without_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_invalid_default_animation(staging);
        let output = staging.join(default_animation_output("male", "mt_runforward"));
        fs::create_dir_all(&output).unwrap();
        let contents = output.join("blocker");
        fs::write(&contents, b"staged generated assets").unwrap();

        for fail_fast in [false, true] {
            let mut report = PipelineReport::default();
            let mut manifest = ConversionManifest::default();
            let error = convert_default_animations(
                staging,
                &BTreeMap::new(),
                fail_fast,
                &mut report,
                &mut manifest,
            )
            .expect_err("directory collision must abort preflight");
            assert!(format!("{error:#}").contains("refusing to remove directory"));
            assert_eq!(fs::read(&contents).unwrap(), b"staged generated assets");
            assert!(report.notices.is_empty(), "notices: {:?}", report.notices);
        }
    }

    /// Rejects immediate and higher file ancestors before changing manifests or notices.
    #[test]
    fn gait_output_metadata_errors_are_fatal_without_notices() {
        for relative in [
            "meshes",
            "meshes/actors/character/animations",
            "meshes/actors/character/animations/male",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path();
            stage_invalid_default_animation(staging);
            let blocker = staging.join(relative);
            fs::create_dir_all(blocker.parent().unwrap()).unwrap();
            fs::write(&blocker, b"not a directory").unwrap();

            for fail_fast in [false, true] {
                let mut report = PipelineReport::default();
                let mut manifest = ConversionManifest::default();
                manifest
                    .failures
                    .insert(MALE_IDLE_GLB.into(), "old failure".into());
                let error = convert_default_animations(
                    staging,
                    &BTreeMap::new(),
                    fail_fast,
                    &mut report,
                    &mut manifest,
                )
                .expect_err("metadata error must abort preflight with either fail-fast setting");

                assert!(format!("{error:#}").contains("failed to inspect"));
                assert!(report.notices.is_empty(), "notices: {:?}", report.notices);
                assert_eq!(manifest.failures.get(MALE_IDLE_GLB).unwrap(), "old failure");
                assert_eq!(fs::read(&blocker).unwrap(), b"not a directory");
            }
        }
    }

    /// A rejected gait preflight leaves the complete failure and proof maps for both sexes intact.
    #[test]
    fn gait_preflight_rejection_preserves_complete_manifest_maps() {
        // File ancestors are rejected by the early parent check; directories at exact outputs are
        // rejected by the output cleanup that runs before any map entry is removed.
        let mut blockers: Vec<(String, bool)> = [
            "meshes",
            "meshes/actors/character/animations",
            "meshes/actors/character/animations/male",
            "meshes/actors/character/animations/female",
        ]
        .into_iter()
        .map(|blocked| (blocked.to_owned(), false))
        .collect();
        for sex in ["male", "female"] {
            for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                blockers.push((default_animation_output(sex, stem), true));
            }
        }
        for (blocked, is_directory) in blockers {
            for fail_fast in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let staging = directory.path();
                stage_invalid_default_animation(staging);
                let blocker = staging.join(&blocked);
                let contents = blocker.join("blocker");
                fs::create_dir_all(blocker.parent().unwrap()).unwrap();
                if is_directory {
                    fs::create_dir_all(&blocker).unwrap();
                    fs::write(&contents, b"staged generated assets").unwrap();
                } else {
                    fs::write(&blocker, b"not a directory").unwrap();
                }

                let mut manifest = ConversionManifest::default();
                for sex in ["male", "female"] {
                    for (_, stem) in DEFAULT_ANIMATION_CLIPS {
                        let key = default_animation_output(sex, stem);
                        manifest
                            .failures
                            .insert(key.clone(), format!("old {sex} failure"));
                        manifest.derived_outputs.insert(
                            key,
                            DerivedOutput {
                                recipe: DEFAULT_GAIT_RECIPE.into(),
                                inputs: BTreeMap::new(),
                                output_size: 1,
                                output_hash: hash_bytes(b"old gait"),
                            },
                        );
                    }
                }
                let incoming = manifest.clone();
                let mut report = PipelineReport::default();
                let error = convert_default_animations(
                    staging,
                    &BTreeMap::new(),
                    fail_fast,
                    &mut report,
                    &mut manifest,
                )
                .expect_err("blocked gait path must abort before any map mutation");

                let expected = if is_directory {
                    "refusing to remove directory"
                } else {
                    "failed to inspect"
                };
                assert!(
                    format!("{error:#}").contains(expected),
                    "{blocked}: {error:#}"
                );
                assert!(report.notices.is_empty(), "{blocked}: {:?}", report.notices);
                assert_eq!(manifest, incoming, "{blocked} fail_fast={fail_fast}");
                if is_directory {
                    assert!(blocker.is_dir(), "{blocked} fail_fast={fail_fast}");
                    assert_eq!(
                        fs::read(&contents).unwrap(),
                        b"staged generated assets",
                        "{blocked} fail_fast={fail_fast}"
                    );
                    assert_eq!(
                        fs::read_dir(&blocker).unwrap().count(),
                        1,
                        "{blocked} fail_fast={fail_fast}"
                    );
                } else {
                    assert_eq!(fs::read(&blocker).unwrap(), b"not a directory");
                }
            }
        }
    }

    #[test]
    fn gait_output_preflight_preserves_regular_outputs_and_removes_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let regular = staging.join(default_animation_output("female", "mt_idle"));
        let symlink = staging.join(default_animation_output("male", "mt_walkforward"));
        let target = directory.path().join("external-target.glb");
        fs::create_dir_all(regular.parent().unwrap()).unwrap();
        fs::create_dir_all(symlink.parent().unwrap()).unwrap();
        fs::write(&regular, b"unchanged clip").unwrap();
        fs::write(&target, b"external target bytes").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &symlink).unwrap();

        clear_nonregular_animation_outputs(staging).unwrap();

        assert_eq!(fs::read(regular).unwrap(), b"unchanged clip");
        #[cfg(unix)]
        {
            assert_eq!(
                fs::symlink_metadata(&symlink).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "symlink must be removed"
            );
            assert_eq!(fs::read(&target).unwrap(), b"external target bytes");
        }
    }

    /// Reports a canonical asset prefix while preserving unrelated source repairs.
    #[test]
    fn animation_preparation_failure_is_a_notice_and_keeps_source_repairs() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_invalid_default_animation(staging);
        let output = staging.join(MALE_IDLE_GLB);
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, b"stale clip").unwrap();
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        manifest
            .failures
            .insert(MALE_IDLE_GLB.into(), "old derived failure".into());
        manifest
            .failures
            .insert("meshes/source.nif".into(), "source repair failure".into());

        convert_default_animations(staging, &BTreeMap::new(), false, &mut report, &mut manifest)
            .unwrap();

        assert!(!output.exists(), "failed conversion must remove stale clip");
        assert!(!manifest.failures.contains_key(MALE_IDLE_GLB));
        assert_eq!(
            manifest
                .failures
                .get("meshes/source.nif")
                .map(String::as_str),
            Some("source repair failure")
        );
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );
        assert!(report.notices.iter().any(|notice| {
            notice.contains(MALE_IDLE_GLB) && notice.contains("failed to prepare")
        }));
        for notice in &report.notices {
            let (key, _) = notice.split_once(": ").unwrap();
            assert!(!key.contains('\\'), "noncanonical gait notice: {key}");
            assert_eq!(key, key.to_ascii_lowercase());
        }
    }

    #[test]
    fn animation_preparation_failure_propagates_with_fail_fast() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        stage_invalid_default_animation(staging);
        let outputs = write_staged_gait_clips(staging);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        for output in &outputs {
            let key = output
                .strip_prefix(staging)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            manifest.failures.insert(key, "stale gait failure".into());
        }

        let error =
            convert_default_animations(staging, &BTreeMap::new(), true, &mut report, &mut manifest)
                .expect_err("fail-fast must propagate an optional-output preparation error");

        assert!(format!("{error:#}").contains("failed to build the default male actor body"));
        assert!(outputs.iter().all(|output| !output.exists()));
        assert!(manifest.failures.is_empty());
        assert!(report.notices.is_empty(), "notices: {:?}", report.notices);
    }

    #[test]
    fn animation_conversion_failure_propagates_with_fail_fast_and_cleans_all_gaits() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let assets = actor_assets(staging);
        let animations = staging.join("vfs/meshes/actors/character/animations/male");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir_all(&animations).unwrap();
        write_actor_skeleton(&assets, "SkeletonRoot", &["Bone0", "Bone1"]);
        write_actor_body_part(&assets, "malebody_1.nif");
        fs::write(assets.join("skeleton.hkx"), b"synthetic skeleton").unwrap();
        fs::write(animations.join("mt_idle.hkx"), b"invalid clip").unwrap();
        let outputs = write_staged_gait_clips(staging);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        for output in &outputs {
            let key = output
                .strip_prefix(staging)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            manifest.failures.insert(key, "stale gait failure".into());
        }

        let error =
            convert_default_animations(staging, &BTreeMap::new(), true, &mut report, &mut manifest)
                .expect_err("fail-fast must propagate a default clip conversion error");

        assert!(format!("{error:#}").contains("failed to convert the default male idle animation"));
        assert!(outputs.iter().all(|output| !output.exists()));
        assert!(manifest.failures.is_empty());
        assert!(report.notices.is_empty(), "notices: {:?}", report.notices);
    }

    fn retail_path(variable: &str) -> PathBuf {
        std::env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{variable} must point at an extracted character tree"))
    }

    fn write_staged_gait_clips(staging: &Path) -> Vec<PathBuf> {
        DEFAULT_ANIMATION_CLIPS
            .iter()
            .flat_map(|(_, stem)| {
                ["male", "female"].map(|sex| staging.join(default_animation_output(sex, stem)))
            })
            .inspect(|path| {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"clip").unwrap();
            })
            .collect()
    }

    const MALE_IDLE_GLB: &str = "meshes/actors/character/animations/male/mt_idle.glb";

    fn stage_invalid_default_animation(staging: &Path) {
        let assets = staging.join("vfs/meshes/actors/character/character assets");
        let animations = staging.join("vfs/meshes/actors/character/animations/male");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir_all(&animations).unwrap();
        fs::write(assets.join("skeleton.hkx"), b"invalid skeleton").unwrap();
        fs::write(assets.join("skeleton.nif"), b"invalid skeleton").unwrap();
        fs::write(assets.join("malebody_1.nif"), b"invalid body").unwrap();
        fs::write(animations.join("mt_idle.hkx"), b"invalid clip").unwrap();
    }

    fn read_glb(path: &Path) -> (serde_json::Value, Vec<u8>) {
        let bytes = fs::read(path).unwrap();
        assert_eq!(&bytes[..4], b"glTF", "GLB magic");
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        assert_eq!(&bytes[16..20], b"JSON", "JSON chunk type");
        let json = serde_json::from_slice(&bytes[20..20 + json_len]).unwrap();
        let bin_len =
            u32::from_le_bytes(bytes[20 + json_len..24 + json_len].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len + 8;
        (json, bytes[bin_start..bin_start + bin_len].to_vec())
    }

    fn accessor_values(json: &serde_json::Value, bin: &[u8], index: usize) -> Vec<f32> {
        let accessor = &json["accessors"][index];
        let view = &json["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize
            + accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
        let components = match accessor["type"].as_str().unwrap() {
            "SCALAR" => 1,
            "VEC2" => 2,
            "VEC3" => 3,
            "VEC4" => 4,
            other => panic!("unexpected accessor type {other}"),
        };
        let count = accessor["count"].as_u64().unwrap() as usize;
        bin[offset..offset + count * components * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect()
    }

    fn clip_duration(json: &serde_json::Value) -> (f32, usize) {
        let animation = &json["animations"][0];
        let input = animation["samplers"][0]["input"].as_u64().unwrap() as usize;
        let accessor = &json["accessors"][input];
        (
            accessor["max"][0].as_f64().unwrap() as f32,
            accessor["count"].as_u64().unwrap() as usize,
        )
    }

    fn rotation_samples(json: &serde_json::Value, bin: &[u8], node_name: &str) -> Vec<[f32; 4]> {
        let nodes = json["nodes"].as_array().unwrap();
        let node = nodes
            .iter()
            .position(|node| node["name"].as_str() == Some(node_name))
            .unwrap_or_else(|| panic!("node {node_name} is missing"));
        let animation = &json["animations"][0];
        let channel = animation["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| {
                channel["target"]["node"].as_u64() == Some(node as u64)
                    && channel["target"]["path"].as_str() == Some("rotation")
            })
            .unwrap_or_else(|| panic!("{node_name} has no rotation channel"));
        let sampler = &animation["samplers"][channel["sampler"].as_u64().unwrap() as usize];
        accessor_values(json, bin, sampler["output"].as_u64().unwrap() as usize)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
            .collect()
    }

    fn quaternion_angle_degrees(a: [f32; 4], b: [f32; 4]) -> f32 {
        let dot = a
            .iter()
            .zip(b)
            .map(|(left, right)| left * right)
            .sum::<f32>()
            .abs()
            .min(1.0);
        2.0 * dot.acos().to_degrees()
    }

    fn stage_retail_animation_tree(
        staging: &Path,
        animation_root: &Path,
        actor_root: &Path,
        skipped: Option<(&str, &str)>,
    ) {
        let assets = staging.join("vfs/meshes/actors/character/character assets");
        let animations = staging.join("vfs/meshes/actors/character/animations");
        fs::create_dir_all(&assets).unwrap();
        fs::copy(
            animation_root.join("character assets/skeleton.hkx"),
            assets.join("skeleton.hkx"),
        )
        .unwrap();
        fs::copy(
            actor_root.join("character/character assets/skeleton.nif"),
            assets.join("skeleton.nif"),
        )
        .unwrap();
        for sex in ["male", "female"] {
            fs::create_dir_all(animations.join(sex)).unwrap();
            fs::copy(
                actor_root.join(format!("character/character assets/{sex}body_1.nif")),
                assets.join(format!("{sex}body_1.nif")),
            )
            .unwrap();
            for (clip, stem) in DEFAULT_ANIMATION_CLIPS {
                if skipped == Some((sex, clip)) {
                    continue;
                }
                fs::copy(
                    animation_root.join(format!("animations/{sex}/{stem}.hkx")),
                    animations.join(sex).join(format!("{stem}.hkx")),
                )
                .unwrap();
            }
        }
    }

    fn assert_written_clip(path: &Path, label: &str, expected_duration: Option<f32>) {
        let (json, _) = read_glb(path);
        let animation = &json["animations"][0];
        let channels = animation["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 288, "{label} channels");
        let (duration, frames) = clip_duration(&json);
        println!(
            "{label}: {} channels, {frames} frames, {duration:.6}s",
            channels.len()
        );
        if let Some(expected) = expected_duration {
            assert!(
                (duration - expected).abs() <= 1e-4,
                "{label} duration {duration}"
            );
        }
    }

    /// Real HKX decoding records every successful gait's four raw inputs and bytes.
    #[test]
    #[ignore = "requires MUDCRAB_ANIMATION_DIR and MUDCRAB_ACTOR_NIF_DIR"]
    fn retail_default_animations_emit_all_gait_clips() {
        let animation_root = retail_path("MUDCRAB_ANIMATION_DIR");
        let actor_root = retail_path("MUDCRAB_ACTOR_NIF_DIR");

        let full = tempfile::tempdir().unwrap();
        stage_retail_animation_tree(full.path(), &animation_root, &actor_root, None);
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        convert_default_animations(
            full.path(),
            &BTreeMap::new(),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();

        let mut written = Vec::new();
        assert_eq!(manifest.derived_outputs.len(), 6);
        for sex in ["male", "female"] {
            for (clip, stem) in DEFAULT_ANIMATION_CLIPS {
                let output = full.path().join(default_animation_output(sex, stem));
                assert!(output.is_file(), "{sex} {clip} clip must exist");
                let key = format!("meshes/actors/character/animations/{sex}/{stem}.glb");
                let proof = &manifest.derived_outputs[&key];
                assert_eq!(proof.recipe, "default-gait-v1");
                let expected_paths = [
                    format!("meshes/actors/character/character assets/{sex}body_1.nif"),
                    "meshes/actors/character/character assets/skeleton.nif".into(),
                    "meshes/actors/character/character assets/skeleton.hkx".into(),
                    format!("meshes/actors/character/animations/{sex}/{stem}.hkx"),
                ];
                let expected: BTreeMap<_, _> = expected_paths
                    .into_iter()
                    .map(|path| {
                        let bytes = fs::read(full.path().join("vfs").join(&path)).unwrap();
                        (path, hash_bytes(&bytes))
                    })
                    .collect();
                assert_eq!(proof.inputs, expected);
                assert_eq!(proof.output_hash, hash_bytes(&fs::read(&output).unwrap()));
                assert_eq!(proof.output_size, fs::metadata(&output).unwrap().len());
                let expected = match clip {
                    "idle" => Some(9.133_333),
                    "walkforward" => Some(1.133_333),
                    _ => None,
                };
                assert_written_clip(&output, &format!("{sex} {clip}"), expected);
                let bytes = fs::read(&output).unwrap();
                written.push((output, bytes));
            }
        }

        let first_proofs = manifest.derived_outputs.clone();
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        convert_default_animations(
            full.path(),
            &BTreeMap::new(),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();
        assert_eq!(manifest.derived_outputs, first_proofs);
        for (output, first) in &written {
            assert_eq!(
                fs::read(output).unwrap(),
                *first,
                "a second run must not rewrite {}",
                output.display()
            );
        }

        let partial = tempfile::tempdir().unwrap();
        stage_retail_animation_tree(
            partial.path(),
            &animation_root,
            &actor_root,
            Some(("male", "runforward")),
        );
        let mut report = PipelineReport::default();
        let mut manifest = ConversionManifest::default();
        convert_default_animations(
            partial.path(),
            &BTreeMap::new(),
            false,
            &mut report,
            &mut manifest,
        )
        .unwrap();
        assert_eq!(manifest.derived_outputs.len(), 5);
        assert!(
            !manifest
                .derived_outputs
                .contains_key("meshes/actors/character/animations/male/mt_runforward.glb")
        );
        for sex in ["male", "female"] {
            for (clip, stem) in DEFAULT_ANIMATION_CLIPS {
                let output = partial.path().join(default_animation_output(sex, stem));
                if (sex, clip) == ("male", "runforward") {
                    assert!(
                        !output.exists(),
                        "the missing male run clip must be skipped"
                    );
                } else {
                    assert!(
                        output.is_file(),
                        "{sex} {clip} must still be written without the male run clip"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires MUDCRAB_ANIMATION_DIR and MUDCRAB_ACTOR_NIF_DIR"]
    fn retail_default_animations_decode_all_gait_clips() {
        let animation_root = retail_path("MUDCRAB_ANIMATION_DIR");
        let actor_root = retail_path("MUDCRAB_ACTOR_NIF_DIR");
        let skeleton_bytes = fs::read(animation_root.join("character assets/skeleton.hkx"))
            .expect("read skeleton.hkx");

        for sex in ["male", "female"] {
            for (clip, stem) in DEFAULT_ANIMATION_CLIPS {
                let bytes = fs::read(animation_root.join(format!("animations/{sex}/{stem}.hkx")))
                    .unwrap_or_else(|error| panic!("read {sex} {clip}: {error}"));
                let decoded = animation::decode_clip_with_skeleton(&bytes, &skeleton_bytes)
                    .unwrap_or_else(|error| panic!("decode {sex} {clip}: {error}"));
                println!(
                    "{sex} {clip}: duration {:.6}s, {} frames, {} tracks, {:.3} fps",
                    decoded.duration_seconds,
                    decoded.frame_count,
                    decoded.tracks.len(),
                    decoded.frames_per_second
                );
                assert_eq!(decoded.tracks.len(), 99, "{sex} {clip} tracks");
                match clip {
                    "idle" => {
                        assert_eq!(decoded.frame_count, 275, "{sex} idle frames");
                        assert!(
                            (decoded.duration_seconds - 9.133_333).abs() <= 1e-4,
                            "{sex} idle duration {}",
                            decoded.duration_seconds
                        );
                    }
                    "walkforward" => {
                        assert_eq!(decoded.frame_count, 35, "{sex} walk frames");
                        assert!(
                            (decoded.duration_seconds - 1.133_333).abs() <= 1e-4,
                            "{sex} walk duration {}",
                            decoded.duration_seconds
                        );
                    }
                    "runforward" => assert!(decoded.frame_count > 0, "{sex} run must carry frames"),
                    other => panic!("unexpected clip {other}"),
                }
            }
        }

        let skeleton_nif = actor_root.join("character/character assets/skeleton.nif");
        let body = crate::mesh::build_skinned_model(
            &actor_root.join("character/character assets/malebody_1.nif"),
            &skeleton_nif,
        )
        .expect("build the male body");
        let bones = body
            .skeletal_meshes
            .first()
            .expect("male body has a skeletal mesh")
            .bones
            .clone();
        let walk_bytes =
            fs::read(animation_root.join("animations/male/mt_walkforward.hkx")).unwrap();
        let mut walk = animation::decode_clip_with_skeleton(&walk_bytes, &skeleton_bytes)
            .expect("decode male walk");
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("mt_walkforward.glb");
        animation::write_clip_glb(&mut walk, &bones, "mt_walkforward", &output)
            .expect("write male walk");

        let (json, bin) = read_glb(&output);
        assert_eq!(json["nodes"].as_array().unwrap().len(), 100);
        let animation = &json["animations"][0];
        assert_eq!(animation["channels"].as_array().unwrap().len(), 288);
        let (duration, frames) = clip_duration(&json);
        println!("male walk GLB: {frames} frames, {duration:.6}s");
        assert!(
            (duration - 1.133_333).abs() <= 1e-4,
            "male walk GLB duration {duration}"
        );

        for bone in ["NPC L Thigh [LThg]", "NPC R Thigh [RThg]"] {
            let samples = rotation_samples(&json, &bin, bone);
            assert_eq!(samples.len(), frames, "{bone} rotation samples");
            let first = samples[0];
            let (peak_frame, peak_angle) = samples
                .iter()
                .enumerate()
                .map(|(frame, sample)| (frame, quaternion_angle_degrees(first, *sample)))
                .max_by(|left, right| left.1.total_cmp(&right.1))
                .unwrap();
            let final_angle = quaternion_angle_degrees(first, *samples.last().unwrap());
            println!(
                "{bone}: peak {peak_angle:.3} deg at frame {peak_frame}, final {final_angle:.3} deg"
            );
            assert!(
                peak_angle > 5.0,
                "{bone} must move more than 5 degrees, peak {peak_angle}"
            );
            assert!(
                final_angle <= 1.0,
                "{bone} walk cycle must close, final {final_angle}"
            );
        }
    }

    #[test]
    fn removes_staged_assets_without_current_sources() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("vfs/meshes")).unwrap();
        fs::write(staging.join("meshes/orphan.glb"), b"mesh").unwrap();
        fs::write(staging.join("vfs/meshes/source.glb"), b"source").unwrap();

        remove_orphan_staged_assets(staging, &[]).unwrap();

        assert!(!staging.join("meshes/orphan.glb").exists());
        assert!(staging.join("vfs/meshes/source.glb").is_file());
    }

    #[test]
    fn keeps_a_provenance_verified_staged_mesh() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::write(staging.join("meshes/verified.glb"), b"mesh").unwrap();
        fs::write(staging.join("meshes/stale.glb"), b"mesh").unwrap();

        invalidate_staged_mesh_outputs(
            staging,
            &BTreeSet::from(["meshes/verified.glb".to_owned()]),
        )
        .unwrap();

        assert!(staging.join("meshes/verified.glb").is_file());
        assert!(!staging.join("meshes/stale.glb").exists());
    }

    #[test]
    fn removes_partial_asset_output_after_conversion_failure() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("scripts/partial.luau");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"partial output").unwrap();

        let (result, fatal) = remove_partial_output_after_failure(
            &target,
            Err(color_eyre::eyre::eyre!("conversion failed after writing")),
        );

        assert!(result.is_err());
        assert!(!fatal);
        assert!(!target.exists());
    }

    #[test]
    fn publishes_a_distinct_asset_path_for_srgb_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("textures/effects")).unwrap();
        fs::write(staging.join("textures/effects/fire.ktx2"), b"texture").unwrap();
        let mut json = serde_json::to_vec(&serde_json::json!({
            "asset": { "version": "2.0" },
            "images": [{ "uri": "../textures/effects/fire.opensky-srgb.ktx2" }],
            "textures": [{ "source": 0 }],
            "materials": [{
                "pbrMetallicRoughness": { "baseColorTexture": { "index": 0 } }
            }]
        }))
        .unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&u32::try_from(20 + json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(&u32::try_from(json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        fs::write(staging.join("meshes/fire.glb"), glb).unwrap();

        let aliases = publish_texture_aliases(staging).unwrap();

        assert_eq!(
            aliases,
            vec![PathBuf::from("textures/effects/fire.opensky-srgb.ktx2")]
        );
        assert_eq!(fs::read(staging.join(&aliases[0])).unwrap(), b"texture");
    }

    #[test]
    fn loose_character_animation_inputs_are_overlaid_and_other_hkx_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let skeleton = data.join("Meshes/Actors/Character/Character Assets/skeleton.hkx");
        let behavior = data.join("Meshes/Actors/Character/Behaviors/0_master.hkx");
        fs::create_dir_all(skeleton.parent().unwrap()).unwrap();
        fs::create_dir_all(behavior.parent().unwrap()).unwrap();
        fs::write(&skeleton, b"skeleton").unwrap();
        fs::write(&behavior, b"behaviour").unwrap();
        let vfs = directory.path().join("vfs");
        fs::create_dir_all(&vfs).unwrap();

        overlay_loose_assets(&data, &vfs, &discover(&data).unwrap()).unwrap();

        assert_eq!(
            fs::read(vfs.join("meshes/actors/character/character assets/skeleton.hkx")).unwrap(),
            b"skeleton"
        );
        assert!(
            !vfs.join("meshes/actors/character/behaviors/0_master.hkx")
                .exists()
        );
    }

    #[test]
    fn mo2_animation_inputs_stage_the_winning_clips() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path().join("vfs");
        fs::create_dir_all(&vfs).unwrap();
        let clip = directory.path().join("mod/animations/male/mt_idle.hkx");
        let behavior = directory.path().join("mod/behaviors/0_master.hkx");
        fs::create_dir_all(clip.parent().unwrap()).unwrap();
        fs::create_dir_all(behavior.parent().unwrap()).unwrap();
        fs::write(&clip, b"clip").unwrap();
        fs::write(&behavior, b"behaviour").unwrap();
        let sources = BTreeMap::from([
            (
                "meshes/actors/character/animations/male/mt_idle.hkx".to_owned(),
                clip,
            ),
            (
                "meshes/actors/character/behaviors/0_master.hkx".to_owned(),
                behavior,
            ),
        ]);

        stage_mo2_animation_inputs(&vfs, &sources).unwrap();

        assert_eq!(
            fs::read(vfs.join("meshes/actors/character/animations/male/mt_idle.hkx")).unwrap(),
            b"clip"
        );
        assert!(
            !vfs.join("meshes/actors/character/behaviors/0_master.hkx")
                .exists()
        );
    }

    #[test]
    fn loose_asset_override_does_not_write_through_a_linked_vfs_entry() {
        let directory = tempfile::tempdir().unwrap();
        let previous = directory.path().join("previous");
        let previous_vfs = previous.join("vfs/textures/rock.dds");
        fs::create_dir_all(previous_vfs.parent().unwrap()).unwrap();
        fs::write(&previous_vfs, b"archive bytes").unwrap();

        // What a first conversion publishes: the extracted `vfs` file and its content-addressed
        // blob share one file. What a reconversion stages for the same entry is one file with
        // that published blob again.
        let hash = "ab".repeat(32);
        let previous_blob = previous.join(".ingestion-cache/sha256/ab").join(&hash);
        fs::create_dir_all(previous_blob.parent().unwrap()).unwrap();
        link_or_copy(&previous_vfs, &previous_blob).unwrap();
        let staging_vfs = directory.path().join("staging/vfs");
        let staged = staging_vfs.join("textures/rock.dds");
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        link_or_copy(&previous_blob, &staged).unwrap();

        let data = directory.path().join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        let loose = data.join("textures/rock.dds");
        fs::write(&loose, b"loose override").unwrap();

        overlay_loose_assets(&data, &staging_vfs, &[loose]).unwrap();

        assert_eq!(fs::read(&staged).unwrap(), b"loose override");
        assert_eq!(
            fs::read(&previous_vfs).unwrap(),
            b"archive bytes",
            "the override wrote through the link into the previous output"
        );
        assert_eq!(fs::read(&previous_blob).unwrap(), b"archive bytes");
    }

    /// MO2 activity accepts the same exact/space/dash/underscore boundary as installed ownership.
    #[test]
    fn mo2_archives_use_shared_owner_separator_rule() {
        let plugins = vec![PathBuf::from("Example.esp")];
        for name in [
            "EXAMPLE.bsa",
            "Example - Textures.bsa",
            "Example - Meshes.ba2",
            "Example Other.bsa",
            "Example-Other.bsa",
            "Example_Other.bsa",
            "Example -Other.bsa",
        ] {
            assert!(mo2_archive_is_active(Path::new(name), &plugins), "{name}");
        }
        for name in ["Example2.bsa", "ExampleOther.bsa", "Other.bsa"] {
            assert!(!mo2_archive_is_active(Path::new(name), &plugins), "{name}");
        }
    }

    /// MO2 activation and final archive attachment share accented plugin-name keys.
    #[test]
    fn archive_attachment_and_mo2_activation_match_unicode_plugin_names() {
        let plugins = vec![PathBuf::from("CAFÉ.esp")];
        for name in ["café.bsa", "café - Textures.bsa"] {
            assert!(mo2_archive_is_active(Path::new(name), &plugins));
            assert_eq!(
                archive_load_order_priority(Path::new(name), &plugins),
                Some(0)
            );
        }
        assert!(!mo2_archive_is_active(Path::new("caféElse.bsa"), &plugins));
        assert_eq!(
            archive_load_order_priority(Path::new("caféElse.bsa"), &plugins),
            None
        );
    }

    #[test]
    fn orders_archives_by_plugin_load_order() {
        let plugins = vec![
            PathBuf::from("Skyrim.esm"),
            PathBuf::from("Update.esm"),
            PathBuf::from("Example.esp"),
        ];
        let mut archives = vec![
            PathBuf::from("Example - Textures.bsa"),
            PathBuf::from("Skyrim - Textures.bsa"),
            PathBuf::from("Update.bsa"),
            PathBuf::from("Skyrim - Meshes.bsa"),
        ];
        sort_archives_by_load_order(&mut archives, &plugins);
        assert_eq!(
            archives,
            vec![
                PathBuf::from("Skyrim - Meshes.bsa"),
                PathBuf::from("Skyrim - Textures.bsa"),
                PathBuf::from("Update.bsa"),
                PathBuf::from("Example - Textures.bsa"),
            ]
        );
    }

    /// Unicode-normalized owner names agree across ownership, activity and priority for every separator.
    #[test]
    fn unicode_plugin_archive_names_match_activation_and_priority() {
        let plugins = [PathBuf::from("Skyrim.esm"), PathBuf::from("CAF\u{c9}.esp")];
        let owners = BTreeMap::from([("caf\u{e9}".to_owned(), true)]);
        for suffix in [" Textures", "-Textures", "_Textures", " - Textures"] {
            let archive = PathBuf::from(format!("caf\u{e9}{suffix}.bsa"));
            assert!(mo2_archive_is_active(&archive, &plugins));
            assert!(non_mo2_archive_is_active(&archive, &owners));
            assert_eq!(archive_load_order_priority(&archive, &plugins), Some(1));
        }
        assert!(!mo2_archive_is_active(
            Path::new("caf\u{e9}Other.bsa"),
            &plugins
        ));
    }

    /// Expanded companion separators still defer to the longest installed owner and unowned policy.
    #[test]
    fn shared_archive_matcher_preserves_longest_owner_and_unowned_policies() {
        let plugins = [PathBuf::from("Mod.esp")];
        let owners = BTreeMap::from([
            ("mod".to_owned(), true),
            ("mod - expansion".to_owned(), false),
        ]);
        for suffix in [" Textures", "-Textures", "_Textures", " - Textures"] {
            let archive = PathBuf::from(format!("Mod - Expansion{suffix}.bsa"));
            assert!(!non_mo2_archive_is_active(&archive, &owners));
        }
        let standalone = Path::new("Standalone.bsa");
        assert!(non_mo2_archive_is_active(standalone, &owners));
        assert!(!mo2_archive_is_active(standalone, &plugins));
        assert_eq!(archive_load_order_priority(standalone, &plugins), None);
    }

    /// Overlapping active names use the longest owner, while equal stems keep their first slot.
    #[test]
    fn archive_priority_preserves_specific_owner_and_same_stem_first_slot() {
        let plugins = [
            PathBuf::from("Good.esp"),
            PathBuf::from("Middle.esp"),
            PathBuf::from("Good - Expansion.esp"),
            PathBuf::from("Good.esl"),
        ];
        assert_eq!(
            archive_load_order_priority(Path::new("Good.bsa"), &plugins),
            Some(0)
        );
        assert_eq!(
            archive_load_order_priority(Path::new("Good - Expansion - Textures.bsa"), &plugins),
            Some(2)
        );
        assert_eq!(
            archive_load_order_priority(Path::new("Standalone.bsa"), &plugins),
            None
        );
    }

    const FOREIGN_PAYLOAD: &str = "optional-payload.bin";

    /// Converts one script, then declares a foreign capability in the published manifest.
    async fn pack_with_foreign_capability(
        data: &Path,
        output: &Path,
    ) -> shared::extensions::ExtensionEntry {
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        AssetPipeline::run_async(cpu_lod_config(data, output), tx)
            .await
            .unwrap();
        drain.await.unwrap();
        let payload = b"foreign optional payload";
        fs::write(output.join(FOREIGN_PAYLOAD), payload).unwrap();
        let mut hasher = shared::extensions::ExtensionHasher::new("example.optional", 1);
        hasher.add_file(FOREIGN_PAYLOAD, payload);
        let entry = shared::extensions::ExtensionEntry {
            id: "example.optional".into(),
            version: 1,
            owns: shared::extensions::ExtensionOwns {
                files: vec![FOREIGN_PAYLOAD.into()],
                ..Default::default()
            },
            sha256: hasher.finish(),
            built_from: "earlier-core".into(),
            stats: Default::default(),
        };
        let path = output.join("conversion-manifest.json");
        let mut manifest = ConversionManifest::load(&path).unwrap();
        manifest.extensions.push(entry.clone());
        manifest.save(&path).unwrap();
        entry
    }

    async fn convert_again(config: PipelineConfig) -> PipelineReport {
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let report = AssetPipeline::run_async(config, tx).await.unwrap();
        drain.await.unwrap();
        report
    }

    /// A new pack does not inherit another pack's capability payloads, but losing one must be
    /// reported rather than silent, and the report must not claim a complete conversion.
    #[tokio::test]
    async fn conversion_reports_capabilities_the_new_pack_drops() {
        for invalidate_cache in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("Data");
            let output = temp.path().join("modern");
            pack_with_foreign_capability(&data, &output).await;
            let mut config = cpu_lod_config(&data, &output);
            config.invalidate_cache = invalidate_cache;
            let report = convert_again(config).await;
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("dropped capability example.optional")),
                "invalidate_cache={invalidate_cache}: {:?}",
                report.warnings
            );
            assert!(!report.complete, "invalidate_cache={invalidate_cache}");
            let published =
                ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
            assert!(!published.complete);
            assert!(published.extensions.is_empty());
        }
    }

    /// A staging tree that already holds a declared payload keeps the declaration, and the pack
    /// is published with the payload files, never the declaration alone.
    #[tokio::test]
    async fn resumed_conversion_publishes_retained_capability_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        let entry = pack_with_foreign_capability(&data, &output).await;
        let staging = temp.path().join("modern.staging-resume");
        fs::create_dir_all(&staging).unwrap();
        fs::copy(output.join(FOREIGN_PAYLOAD), staging.join(FOREIGN_PAYLOAD)).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let report = convert_again(config).await;
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.complete);
        let published = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert_eq!(published.extensions, vec![entry]);
        assert_eq!(
            fs::read(output.join(FOREIGN_PAYLOAD)).unwrap(),
            b"foreign optional payload"
        );
    }

    #[tokio::test]
    async fn converts_and_reuses_assets_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("scripts/two.pex"),
            dummy_content::pex::minimal("Two").unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.cpu_jobs = 2;
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let first = AssetPipeline::run_async(config.clone(), tx).await.unwrap();
        drain.await.unwrap();
        assert_eq!(first.converted, 2);
        assert!(output.join("scripts/one.luau").is_file());
        assert!(output.join("scripts/papyrus_runtime.luau").is_file());

        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let second = AssetPipeline::run_async(config, tx).await.unwrap();
        drain.await.unwrap();
        assert_eq!(second.cache_hits, 2);
        assert_eq!(second.skipped, 0);
        assert!(
            ConversionManifest::load(&output.join("conversion-manifest.json"))
                .unwrap()
                .complete
        );
        for report in [&first, &second] {
            assert!(
                report
                    .publication_phase_elapsed_ms
                    .contains_key("runtime_pack_linking")
            );
            assert!(
                report
                    .publication_phase_elapsed_ms
                    .contains_key("pack_swap")
            );
            assert_eq!(
                report.phase_elapsed_ms["runtime_publication"],
                report.publication_elapsed_ms
            );
            assert!(
                report.publication_phase_elapsed_ms.values().sum::<u128>()
                    <= report.publication_elapsed_ms
            );
            assert!(
                report
                    .publication_phase_elapsed_ms
                    .keys()
                    .all(|operation| { !report.phase_elapsed_ms.contains_key(operation) })
            );
        }
        assert!(
            !first
                .publication_phase_elapsed_ms
                .contains_key("ownership_record_sync")
        );
        for operation in [
            "previous_generated_artifact_sealing",
            "next_generated_artifact_sealing",
            "ownership_record_write",
            "ownership_record_sync",
            "ownership_record_publication",
            "previous_pack_cleanup",
        ] {
            assert!(second.publication_phase_elapsed_ms.contains_key(operation));
        }
    }

    #[tokio::test]
    async fn resume_does_not_publish_unverified_staged_meshes_for_any_manifest_schema() {
        let manifests = std::iter::once(("absent".to_owned(), None)).chain(
            (14..=22).map(|schema| (
                format!("schema-{schema}"),
                Some(format!(r#"{{"schema_version":{schema},"complete":true,"configuration_hash":"","entries":{{}}}}"#)),
            )),
        );

        for (name, manifest) in manifests {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("Data");
            let output = temp.path().join("modern");
            let staging = temp.path().join("modern.staging-resume");
            fs::create_dir_all(&data).unwrap();
            fs::create_dir_all(&output).unwrap();
            fs::create_dir_all(staging.join("meshes")).unwrap();
            fs::write(staging.join("meshes/stale.glb"), b"unverified mesh").unwrap();
            if let Some(manifest) = manifest {
                fs::write(output.join("conversion-manifest.json"), manifest).unwrap();
            }

            let mut config = cpu_lod_config(&data, &output);
            config.resume_staging = Some(staging);
            let report = run_without_progress(config).await;

            assert!(report.complete, "resume failed with {name} manifest");
            assert!(
                !output.join("meshes/stale.glb").exists(),
                "stale mesh published with {name} manifest"
            );
            assert_eq!(
                ConversionManifest::load(&output.join("conversion-manifest.json"))
                    .unwrap()
                    .schema_version,
                crate::cache::CONVERTER_SCHEMA_VERSION
            );
        }
    }

    #[tokio::test]
    async fn reuses_and_invalidates_archive_ingestion_cache_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new(
                    "strings/readme.strings",
                    b"cached asset",
                )],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.extract_all_archive_files = true;

        let cache_root = config.ingestion_cache_dir();
        let first = run_without_progress(config.clone()).await;
        assert_eq!(first.converted, 1);
        assert_eq!(first.cache_hits, 0);
        assert!(!output.join("vfs").exists());
        assert!(!output.join(".ingestion-cache").exists());
        let blobs: Vec<_> = WalkDir::new(cache_root.join(".ingestion-cache/sha256"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .collect();
        assert_eq!(blobs.len(), 1);
        assert_eq!(fs::read(blobs[0].path()).unwrap(), b"cached asset");

        let second = run_without_progress(config.clone()).await;
        assert_eq!(second.converted, 0);
        assert_eq!(second.cache_hits, 1);

        // Configuration changes must not bypass verification of retained archive blobs.
        fs::write(blobs[0].path(), b"broken asset").unwrap();
        let mut reader_changed = config.clone();
        reader_changed.record_reader = crate::config::RecordReader::Inhouse;
        let corrupted = run_without_progress(reader_changed.clone()).await;
        assert_eq!(corrupted.converted, 0);
        assert_eq!(
            corrupted.cache_hits, 1,
            "verified packs recover corrupt derived blobs"
        );
        let manifest = published_manifest(&output);
        assert_eq!(
            manifest.archives.values().next().unwrap().files[0].hash,
            hash_bytes(b"cached asset")
        );

        // Repair the fixture's persistent blob, then prove a reader switch can reuse it.
        fs::write(blobs[0].path(), b"cached asset").unwrap();
        let repaired = run_without_progress(config.clone()).await;
        assert_eq!(repaired.converted, 0);
        assert_eq!(repaired.cache_hits, 1);

        let mut unsupported = published_manifest(&output);
        unsupported.schema_version = CONVERTER_SCHEMA_VERSION + 1;
        unsupported
            .save(&output.join("conversion-manifest.json"))
            .unwrap();
        let rejected = run_without_progress(reader_changed.clone()).await;
        assert_eq!(rejected.converted, 0);
        assert_eq!(
            rejected.cache_hits, 1,
            "independently versioned raw packs remain valid; unsupported derived schemas are not reused"
        );

        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new(
                    "docs/readme.txt",
                    b"changed asset",
                )],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        let source_changed = run_without_progress(reader_changed).await;
        assert_eq!(source_changed.converted, 1);
        assert_eq!(source_changed.cache_hits, 0);

        let mut invalidated = config;
        let mut texture_changed = invalidated.clone();
        texture_changed.texture_fallback_quality += 1;
        let retained_archive = run_without_progress(texture_changed).await;
        assert_eq!(
            retained_archive.cache_hits, 1,
            "texture recipes do not change ingestion bytes"
        );
        assert_eq!(retained_archive.converted, 0);
        invalidated.invalidate_cache = true;
        let third = run_without_progress(invalidated).await;
        assert_eq!(third.converted, 1);
        assert_eq!(third.cache_hits, 0);
    }

    fn write_archived_script(data: &Path) {
        fs::create_dir_all(data).unwrap();
        let script = dummy_content::pex::minimal("One").unwrap();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("scripts/one.pex", &script)],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
    }

    fn replace_script_with_verified_stale_output(output: &Path, manifest: &mut ConversionManifest) {
        let stale = b"return { stale = true }";
        fs::write(output.join("scripts/one.luau"), stale).unwrap();
        let entry = manifest
            .entries
            .values_mut()
            .find(|entry| entry.output == "scripts/one.luau")
            .unwrap();
        entry.output_size = stale.len() as u64;
        entry.output_hash = hash_bytes(stale);
    }

    #[tokio::test]
    async fn reader_changes_reuse_archive_ingestion_and_rebuild_derived_assets() {
        use crate::config::RecordReader::{Inhouse, Legacy};

        for schema in [24, CONVERTER_SCHEMA_VERSION] {
            for (previous_reader, next_reader) in [(Legacy, Inhouse), (Inhouse, Legacy)] {
                let temp = tempfile::tempdir().unwrap();
                let data = temp.path().join("Data");
                let output = temp.path().join("modern");
                write_archived_script(&data);
                let mut config = cpu_lod_config(&data, &output);
                config.record_reader = previous_reader;
                let first = run_without_progress(config.clone()).await;
                assert!(first.complete);
                assert_eq!(first.converted, 2);
                let expected = fs::read(output.join("scripts/one.luau")).unwrap();
                let mut manifest = published_manifest(&output);
                manifest.schema_version = schema;
                manifest.configuration_hash =
                    configuration_hash_for_schema(&config, schema).unwrap();
                manifest
                    .save(&output.join("conversion-manifest.json"))
                    .unwrap();

                config.record_reader = next_reader;
                let report = run_without_progress(config).await;

                // Scripts are reader-independent: the switch reuses the archive file and the
                // verified script, and only the database and cell caches are rebuilt.
                assert!(report.complete);
                assert_eq!(
                    report.cache_hits, 2,
                    "archive ingestion and script must be reused"
                );
                assert_eq!(
                    report.converted, 0,
                    "reader identity must not re-encode scripts"
                );
                assert_eq!(fs::read(output.join("scripts/one.luau")).unwrap(), expected);
                assert_eq!(published_manifest(&output).archives, manifest.archives);
            }
        }
    }

    #[tokio::test]
    async fn retained_configuration_changes_reuse_archives_and_rebuild_derived_assets() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_archived_script(&data);
        let config = cpu_lod_config(&data, &output);
        assert!(run_without_progress(config.clone()).await.complete);
        let expected = fs::read(output.join("scripts/one.luau")).unwrap();
        let mut manifest = published_manifest(&output);
        // A legacy manifest carries only the combined retained proof, not per-kind hashes.
        manifest.asset_configuration_hashes.clear();
        let mut retained_config = config.clone();
        retained_config.script_abi_version += 1;
        manifest.retained_mesh_schema_version = Some(CONVERTER_SCHEMA_VERSION);
        manifest.retained_asset_configuration_hash =
            Some(configuration_hash(&retained_config).unwrap());
        replace_script_with_verified_stale_output(&output, &mut manifest);
        manifest
            .save(&output.join("conversion-manifest.json"))
            .unwrap();

        let report = run_without_progress(config).await;

        assert!(report.complete);
        assert_eq!(report.cache_hits, 1, "archive ingestion must be reused");
        assert_eq!(report.converted, 1, "derived scripts must regenerate");
        assert_eq!(fs::read(output.join("scripts/one.luau")).unwrap(), expected);
        assert_eq!(published_manifest(&output).archives, manifest.archives);
    }

    #[tokio::test]
    async fn a_failed_batch_stops_and_keeps_staging_for_a_resume() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/bad.dds"), b"not a DDS").unwrap();
        fs::write(data.join("textures/also-bad.dds"), b"also not a DDS").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let mut config = cpu_lod_config(&data, &output);
        config.fail_fast = true;
        let failure = AssetPipeline::run_async(config, tx).await.unwrap_err();
        let events = collect.await.unwrap();

        assert!(failure.to_string().contains("failed to convert"));
        assert!(!failure.cancelled);
        assert!(!output.exists());
        let staging = failure
            .staging
            .expect("a failed run keeps the work it has done");
        assert!(staging.is_dir());
        let last = events.last().unwrap();
        assert_eq!(last.stage, ProgressStage::Textures);
        assert_eq!(last.outcome, Some(AssetOutcome::Failed));
        assert_eq!(last.message, "Asset conversion failed");
        assert!(last.current_file.as_ref().is_some_and(|path| {
            path == Path::new("textures/bad.dds") || path == Path::new("textures/also-bad.dds")
        }));
    }

    fn staging_entries(parent: &Path) -> Vec<PathBuf> {
        staging_entries_named(parent, "modern")
    }

    fn staging_entries_named(parent: &Path, output_name: &str) -> Vec<PathBuf> {
        let prefix = format!("{output_name}.staging-");
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_journal_write_failure_stops_the_batch_before_removing_staging() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        // Enough work that workers are still converting when the first
        // journal write fails.
        for index in 0..400 {
            let name = format!("Script{index}");
            fs::write(
                data.join(format!("scripts/{name}.pex")),
                dummy_content::pex::minimal(&name).unwrap(),
            )
            .unwrap();
        }

        crate::cache::FAIL_JOURNAL_WRITES.with(|fail| fail.set(true));
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx).await;
        crate::cache::FAIL_JOURNAL_WRITES.with(|fail| fail.set(false));

        let error = result.unwrap_err();
        assert!(format!("{error:?}").contains("injected journal write failure"));
        let staging = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&staging).unwrap();
        // A worker left running would write into staging after it was removed.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!output.exists());
        assert_eq!(staging_entries(temp.path()), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn a_folder_that_is_not_an_earlier_output_is_refused_and_kept() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("Games");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/One.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        fs::create_dir_all(output.join("Skyrim")).unwrap();
        fs::write(output.join("Skyrim/save.ess"), b"keep me").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("conversion-manifest.json"));
        assert_eq!(
            fs::read(output.join("Skyrim/save.ess")).unwrap(),
            b"keep me"
        );
        assert_eq!(
            staging_entries_named(temp.path(), "Games"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn publishing_refuses_to_replace_a_folder_that_is_not_an_earlier_output() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("pack");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("conversion-manifest.json"), b"{}").unwrap();
        let output = temp.path().join("Games");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("keep.txt"), b"keep me").unwrap();

        assert!(publish_directory(&staging, &output).is_err());
        assert_eq!(fs::read(output.join("keep.txt")).unwrap(), b"keep me");
        assert!(staging.join("conversion-manifest.json").is_file());
    }

    #[tokio::test]
    async fn a_failed_publish_keeps_the_journal_in_staging() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/One.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        // A stale backup alongside a live output makes `publish_directory`
        // refuse to publish: without the output dir, a lone backup reads as
        // an interrupted swap and is restored instead.
        fs::create_dir_all(&output).unwrap();
        let backup = output.with_extension(format!("backup-{}", std::process::id()));
        fs::create_dir_all(&backup).unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("stale backup"));
        let staging = staging_entries(temp.path());
        assert_eq!(
            staging.len(),
            1,
            "expected only the staging directory: {staging:?}"
        );
        assert!(staging[0].is_dir());
        assert!(StagingJournal::path_in(&staging[0]).is_file());
        assert!(!parked_journal_path(&staging[0]).exists());

        // Once the backup is gone the same staging directory publishes, and
        // neither the output nor its parent keeps the journal.
        fs::remove_dir_all(&backup).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging[0].clone());
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        AssetPipeline::run_async(config, tx).await.unwrap();
        assert!(output.join("conversion-manifest.json").is_file());
        assert!(!StagingJournal::path_in(&output).exists());
        assert_eq!(staging_entries(temp.path()), Vec::<PathBuf>::new());
    }

    /// The pack is already published when the ingestion cache is written, so a cache write that
    /// fails must leave a complete run, not a failure that sends the user back to resume it.
    #[tokio::test]
    async fn a_post_publication_cache_failure_keeps_the_published_pack_complete() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new(
                    "strings/readme.strings",
                    b"cached asset",
                )],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.extract_all_archive_files = true;

        FAIL_INGESTION_CACHE_PERSISTENCE.with(|fail| fail.set(true));
        let report = run_without_progress(config.clone()).await;
        FAIL_INGESTION_CACHE_PERSISTENCE.with(|fail| fail.set(false));

        assert!(report.complete, "warnings: {:?}", report.warnings);
        assert_eq!(published_manifest(&output).complete, report.complete);
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("could not save the ingestion cache")),
            "notices: {:?}",
            report.notices
        );
        let cache = config.ingestion_cache_dir().join(".ingestion-cache/sha256");
        assert!(
            !cache.exists(),
            "the injected failure skipped cache persistence"
        );

        // The next run publishes normally from the same inputs.
        let rerun = run_without_progress(config).await;
        assert!(rerun.complete);
    }

    #[tokio::test]
    async fn an_interrupted_run_keeps_staging_and_resumes_from_it() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();

        let cancellation = Cancellation::new();
        cancellation.cancel();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let failure =
            AssetPipeline::run_async_with_cancel(cpu_lod_config(&data, &output), tx, cancellation)
                .await
                .unwrap_err();
        drain.await.unwrap();

        assert!(failure.cancelled);
        assert!(failure.to_string().contains("interrupted"));
        assert!(!output.exists(), "an interrupted run does not publish");
        let staging = failure
            .staging
            .expect("an interrupted run keeps its staging folder");
        assert!(staging.is_dir());

        // The kept folder is a working resume point: the run finishes from it.
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert!(output.join("scripts/one.luau").is_file());
        let costs = PathBuf::from(shared::streaming_costs::STREAMING_COST_FILE_NAME);
        assert!(report.artifacts.contains(&costs));
        let fingerprint = hash_file(&output.join("conversion-manifest.json")).unwrap();
        shared::streaming_costs::StreamingCostCatalog::load(&output.join(costs), &fingerprint)
            .unwrap();
    }

    #[tokio::test]
    async fn skips_failed_assets_and_records_redo_list() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/bad.dds"), b"not a DDS").unwrap();
        fs::write(data.join("textures/also-bad.dds"), b"also not a DDS").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let report = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap();
        let events = collect.await.unwrap();

        assert_eq!(report.skipped, 2);
        assert_eq!(report.warnings.len(), 2);
        assert!(!report.complete);
        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(!manifest.complete);
        assert_eq!(manifest.failures.len(), 2);
        assert!(manifest.failures.contains_key("textures/bad.dds"));
        assert!(manifest.failures.contains_key("textures/also-bad.dds"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.outcome == Some(AssetOutcome::Skipped))
                .count(),
            2
        );
    }

    #[test]
    fn pruned_texture_references_do_not_make_a_run_incomplete() {
        let pruned_only = PipelineReport {
            pruned_texture_references: 182,
            ..PipelineReport::default()
        };
        assert!(
            conversion_is_complete(&pruned_only),
            "a texture the game data never contained must not block a release"
        );

        let skipped = PipelineReport {
            skipped: 1,
            ..PipelineReport::default()
        };
        assert!(!conversion_is_complete(&skipped));

        let warned = PipelineReport {
            warnings: vec!["asset integration failed: 1 missing models".to_owned()],
            ..PipelineReport::default()
        };
        assert!(!conversion_is_complete(&warned));
    }

    /// A lost light row makes the run incomplete; an unresolved emittance target is a notice.
    #[test]
    fn projection_omissions_warn_on_data_loss_and_note_fallbacks() {
        use crate::esm::projection_omissions::{OmissionCategory, ProjectionOmissions};
        let mut fallback_only = ProjectionOmissions::default();
        fallback_only.record(OmissionCategory::EmittanceTargetAbsent, 0xDEAD, "absent");
        let mut report = PipelineReport::default();
        report_projection_omissions(&mut report, &fallback_only);
        assert!(report.warnings.is_empty());
        assert_eq!(report.notices.len(), 1);
        assert!(
            report.notices[0].contains("emittance_target_absent: 1 record (fallback)"),
            "{:?}",
            report.notices
        );
        assert!(conversion_is_complete(&report));

        let mut with_loss = fallback_only;
        with_loss.record(OmissionCategory::LightShortData, 0x802, "short");
        with_loss.record(OmissionCategory::LightShortData, 0x803, "short");
        let mut report = PipelineReport::default();
        report_projection_omissions(&mut report, &with_loss);
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("light_short_data: 2 records (data_loss), e.g. 00000802"),
            "{:?}",
            report.warnings
        );
        assert!(!conversion_is_complete(&report));
    }

    /// Notices never hide failures or affect completeness, regardless of insertion order.
    #[test]
    fn notices_do_not_affect_completeness() {
        let mut report = PipelineReport::default();
        report.notices.push("nested plugins ignored".into());
        assert!(conversion_is_complete(&report));
        report.warnings.push("conversion failed".into());
        assert!(!conversion_is_complete(&report));
        report.notices.push("another advisory".into());
        assert!(!conversion_is_complete(&report));
        report.warnings.clear();
        report.skipped = 1;
        assert!(!conversion_is_complete(&report));
    }

    /// Both volcanic models recover failed fallback publication and follow loose mod changes.
    #[tokio::test]
    async fn volcanic_normal_pipeline_publishes_target_and_tracks_mod_override() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("meshes/landscape/grass")).unwrap();
        fs::create_dir_all(data.join("textures/dlc02/landscape/grass")).unwrap();
        fs::create_dir_all(data.join("textures/landscape/grass")).unwrap();
        let positions = [[-2.0, -1.0, 0.0], [3.0, -1.0, 0.0], [0.0, 4.0, 0.0]];
        let normals = [[0.0, 0.0, 1.0]; 3];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [0.0, 0.0]];
        let indices = [[0, 1, 2]];
        for model in [
            "dlc02volcanicashgrass01",
            "dlc02volcanicashgrass02",
            "control",
        ] {
            let shape = dummy_content::nif::StaticShape {
                name: model,
                positions: &positions,
                normals: &normals,
                uvs: &uvs,
                indices: &indices,
                diffuse: "textures/present.dds",
                normal_texture: if model == "control" {
                    ""
                } else {
                    "textures/landscape/grass/volcanicashgrass02_n.dds"
                },
            };
            let mut nif = dummy_content::nif::static_shape(&shape).unwrap();
            // This generated four-block fixture ends with a nine-slot texture set and the
            // 100-byte lighting shader directly before it. Enable its authored specular flag.
            let texture_set_size = 4 + 9 * 4 + shape.diffuse.len() + shape.normal_texture.len();
            let shader = nif.len() - texture_set_size - 100;
            assert_eq!(&nif[shader + 16..shader + 20], &0u32.to_le_bytes());
            nif[shader + 16..shader + 20].copy_from_slice(&1u32.to_le_bytes());
            fs::write(
                data.join(format!("meshes/landscape/grass/{model}.nif")),
                nif,
            )
            .unwrap();
        }
        let dds = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc7Unorm, 8, 8),
            &mut dummy_content::rng::Rng::new(71),
        )
        .unwrap();
        fs::write(data.join("textures/present.dds"), &dds).unwrap();
        fs::write(
            data.join("textures/dlc02/landscape/grass/volcanicashgrass02_n.dds"),
            &dds,
        )
        .unwrap();
        let override_dds = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc7Unorm, 8, 8),
            &mut dummy_content::rng::Rng::new(93),
        )
        .unwrap();
        assert_ne!(dds, override_dds);
        let target_source = data.join("textures/dlc02/landscape/grass/volcanicashgrass02_n.dds");
        fs::write(&target_source, b"invalid DDS, still present in the VFS").unwrap();
        let failed = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(
            !failed.complete,
            "invalid target texture cannot complete publication"
        );
        for model in ["dlc02volcanicashgrass01", "dlc02volcanicashgrass02"] {
            let glb = output.join(format!("meshes/landscape/grass/{model}.glb"));
            assert_eq!(MeshConverter::glb_texture_uris(&glb).unwrap().len(), 1);
        }
        // Repair the DDS bytes without changing either source-presence bit or any NIF bytes.
        fs::write(&target_source, &dds).unwrap();
        let mut control_bytes = None;
        for (run, original) in [false, true, false, false].into_iter().enumerate() {
            let source = data.join("textures/landscape/grass/volcanicashgrass02_n.dds");
            if original {
                fs::write(&source, &override_dds).unwrap();
            } else if source.exists() {
                fs::remove_file(&source).unwrap();
            }
            let report = run_without_progress(cpu_lod_config(&data, &output)).await;
            assert!(report.complete, "{:?}", report.warnings);
            let control = fs::read(output.join("meshes/landscape/grass/control.glb")).unwrap();
            if let Some(previous) = &control_bytes {
                assert_eq!(&control, previous);
                assert!(
                    report.cache_hits >= 2,
                    "unchanged control and diffuse must be reused"
                );
            } else {
                control_bytes = Some(control);
            }
            if run == 3 {
                assert_eq!(
                    report.converted, 0,
                    "unchanged repaired meshes must remain cache hits"
                );
            }
            for model in ["dlc02volcanicashgrass01", "dlc02volcanicashgrass02"] {
                let glb = output.join(format!("meshes/landscape/grass/{model}.glb"));
                let uris = MeshConverter::glb_texture_uris(&glb).unwrap();
                let expected = if original {
                    "../../../textures/landscape/grass/volcanicashgrass02_n.opensky-wrap0.ktx2"
                } else {
                    "../../../textures/dlc02/landscape/grass/volcanicashgrass02_n.opensky-wrap0.ktx2"
                };
                assert!(
                    uris.iter().any(|uri| uri == expected),
                    "expected {expected}, got {uris:?}"
                );
                assert!(
                    resolve_asset_uri(&output, &glb, expected)
                        .unwrap()
                        .is_file()
                );
                if original {
                    assert_ne!(
                        fs::read(resolve_asset_uri(&output, &glb, expected).unwrap()).unwrap(),
                        fs::read(
                            output.join("textures/dlc02/landscape/grass/volcanicashgrass02_n.ktx2")
                        )
                        .unwrap()
                    );
                }
                let bytes = fs::read(&glb).unwrap();
                let length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
                let json: serde_json::Value =
                    serde_json::from_slice(&bytes[20..20 + length]).unwrap();
                assert_eq!(
                    json["materials"][0]["normalTexture"]["index"],
                    json["materials"][0]["extensions"]["KHR_materials_specular"]["specularTexture"]
                        ["index"]
                );
                assert!(json["materials"][0]["normalTexture"]["index"].is_number());
            }
        }
    }

    #[tokio::test]
    async fn publishes_meshes_with_missing_textures_and_stays_complete() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        // One mesh drops an auxiliary map, the other the mandatory base color.
        let shapes = [
            (
                "meshes/missing_normal.nif",
                dummy_content::nif::StaticShape {
                    name: "MissingNormalQuad",
                    positions: &positions,
                    normals: &normals,
                    uvs: &uvs,
                    indices: &indices,
                    diffuse: "textures/present.dds",
                    normal_texture: "textures/absent_n.dds",
                },
            ),
            (
                "meshes/missing_diffuse.nif",
                dummy_content::nif::StaticShape {
                    name: "MissingDiffuseQuad",
                    positions: &positions,
                    normals: &normals,
                    uvs: &uvs,
                    indices: &indices,
                    diffuse: "textures/absent.dds",
                    normal_texture: "textures/present_n.dds",
                },
            ),
        ];
        for (path, shape) in shapes {
            fs::write(
                data.join(path),
                dummy_content::nif::static_shape(&shape).unwrap(),
            )
            .unwrap();
        }
        for texture in ["textures/present.dds", "textures/present_n.dds"] {
            fs::write(
                data.join(texture),
                dummy_content::dds::generate(
                    &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                    &mut dummy_content::rng::Rng::new(7),
                )
                .unwrap(),
            )
            .unwrap();
        }

        let report = run_without_progress(cpu_lod_config(&data, &output)).await;

        assert_eq!(report.skipped, 0);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(
            report.complete,
            "a texture the game data does not contain is not an incomplete conversion"
        );
        assert_eq!(report.pruned_texture_references, 2);

        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(manifest.complete);
        assert!(
            manifest.failures.is_empty(),
            "a pruned reference is not a failure: {:?}",
            manifest.failures
        );
        // Base color is published through an sRGB alias, so that is the URI the
        // mesh dropped.
        assert_eq!(
            manifest
                .pruned_texture_references
                .get("meshes/missing_diffuse.glb"),
            Some(&BTreeSet::from([
                "textures/absent.opensky-srgb.opensky-wrap0.ktx2".to_owned()
            ]))
        );
        assert_eq!(
            manifest
                .pruned_texture_references
                .get("meshes/missing_normal.glb"),
            Some(&BTreeSet::from([
                "textures/absent_n.opensky-wrap0.ktx2".to_owned()
            ]))
        );
        for (glb, kept) in [
            ("meshes/missing_diffuse.glb", "present_n"),
            ("meshes/missing_normal.glb", "present.opensky-srgb"),
        ] {
            let source_key = canonical_asset_path(glb, AssetKind::Mesh, "nif").unwrap();
            let entry = &manifest.entries[&source_key];
            assert_eq!(entry.output_hash, hash_file(&output.join(glb)).unwrap());
            assert_eq!(
                entry.output_size,
                fs::metadata(output.join(glb)).unwrap().len()
            );
            let uris = MeshConverter::glb_texture_uris(&output.join(glb)).unwrap();
            assert!(
                !uris.iter().any(|uri| uri.contains("absent")),
                "the dangling reference is still in {glb}: {uris:?}"
            );
            assert!(
                uris.iter().any(|uri| uri.contains(kept)),
                "{glb} lost the texture that does exist: {uris:?}"
            );
        }
    }

    #[tokio::test]
    async fn failed_archives_still_skip_and_warn() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("broken.bsa"), b"not a BSA archive").unwrap();

        let report = run_without_progress(cpu_lod_config(&data, &output)).await;

        assert_eq!(report.skipped, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("broken.bsa"));
        assert_eq!(report.pruned_texture_references, 0);
        assert!(!report.complete);

        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(!manifest.complete);
        assert_eq!(manifest.failures.len(), 1);
        assert!(manifest.pruned_texture_references.is_empty());
    }

    const PRUNED_MESH: &str = "meshes/dangling_normal.glb";
    const PRUNED_REFERENCE: &str = "textures/absent_n.opensky-wrap0.ktx2";

    /// Writes one NIF whose normal map is absent from the game data, next to the
    /// base-color DDS the game data does contain.
    fn write_mesh_with_absent_normal(data: &Path) {
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let shape = dummy_content::nif::StaticShape {
            name: "DanglingNormalQuad",
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            indices: &indices,
            diffuse: "textures/present.dds",
            normal_texture: "textures/absent_n.dds",
        };
        fs::write(
            data.join("meshes/dangling_normal.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("textures/present.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(7),
            )
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn schema23_distant_containers_and_textures_require_regeneration() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        write_mesh_with_absent_normal(&data);
        let nif = data.join("meshes/dangling_normal.nif");
        let mut bytes = fs::read(&nif).unwrap();
        let root = bytes
            .windows(b"BSFadeNode".len())
            .position(|w| w == b"BSFadeNode")
            .unwrap();
        let last_type = bytes
            .windows(b"BSShaderTextureSet".len())
            .position(|w| w == b"BSShaderTextureSet")
            .unwrap();
        let size_offset = last_type + b"BSShaderTextureSet".len() + 4 * 2;
        let root_size = u32::from_le_bytes(bytes[size_offset..size_offset + 4].try_into().unwrap());
        bytes[size_offset..size_offset + 4].copy_from_slice(&(root_size + 4).to_le_bytes());
        bytes.splice(
            root - 4..root + b"BSFadeNode".len(),
            [
                (b"BSMultiBoundNode".len() as u32).to_le_bytes().as_slice(),
                b"BSMultiBoundNode",
            ]
            .concat(),
        );
        let (body, _) = crate::mesh::parse_skyrim_header(&bytes, &nif).unwrap();
        let root_end = bytes.len() - body.len() + root_size as usize;
        bytes.splice(root_end..root_end, u32::MAX.to_le_bytes());
        fs::write(&nif, bytes).unwrap();
        let config = cpu_lod_config(&data, &output);
        assert!(run_without_progress(config.clone()).await.complete);
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();
        let texture = output.join("textures/present.ktx2");
        let expected_texture = fs::read(&texture).unwrap();
        let mut manifest = published_manifest(&output);
        manifest.schema_version = 23;
        manifest.asset_configuration_hashes.clear();
        manifest.configuration_hash = configuration_hash_for_schema(&config, 23).unwrap();
        let stale = b"pre-LOD23 omitted distant container";
        fs::write(output.join(PRUNED_MESH), stale).unwrap();
        let entry = manifest
            .entries
            .values_mut()
            .find(|entry| entry.output == PRUNED_MESH)
            .unwrap();
        entry.output_size = stale.len() as u64;
        entry.output_hash = hash_bytes(stale);
        manifest
            .save(&output.join("conversion-manifest.json"))
            .unwrap();
        let report = run_without_progress(config).await;
        assert_eq!(report.converted, 2);
        assert_eq!(fs::read(output.join(PRUNED_MESH)).unwrap(), expected);
        assert_eq!(fs::read(texture).unwrap(), expected_texture);
    }

    #[tokio::test]
    async fn legacy_producers_rebuild_meshes_and_textures_and_reuse_scripts() {
        for old_schema in [16, 17, 18, 19, 20, 21, 22, 23] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("Data");
            let output = temp.path().join("modern");
            write_mesh_with_absent_normal(&data);
            fs::create_dir_all(data.join("scripts")).unwrap();
            fs::write(
                data.join("scripts/one.pex"),
                dummy_content::pex::minimal("One").unwrap(),
            )
            .unwrap();
            let config = cpu_lod_config(&data, &output);
            assert!(run_without_progress(config.clone()).await.complete);
            let expected_mesh = fs::read(output.join(PRUNED_MESH)).unwrap();
            let texture_path = output.join("textures/present.ktx2");
            let expected_texture = fs::read(&texture_path).unwrap();
            let manifest_path = output.join("conversion-manifest.json");
            let mut manifest = published_manifest(&output);
            manifest.schema_version = old_schema;
            manifest.asset_configuration_hashes.clear();
            manifest.configuration_hash =
                configuration_hash_for_schema(&config, old_schema).unwrap();
            // A verified old GLB must still be rebuilt: its publication semantics changed.
            let old_bytes = b"old material publication";
            fs::write(output.join(PRUNED_MESH), old_bytes).unwrap();
            let entry = manifest
                .entries
                .values_mut()
                .find(|entry| entry.output == PRUNED_MESH)
                .unwrap();
            entry.output_size = old_bytes.len() as u64;
            entry.output_hash = crate::cache::hash_bytes(old_bytes);
            // Valid old cache proof cannot establish the new texture contract.
            let old_texture = b"old texture encoding contract";
            fs::write(&texture_path, old_texture).unwrap();
            let entry = manifest
                .entries
                .values_mut()
                .find(|entry| entry.output == "textures/present.ktx2")
                .unwrap();
            entry.output_size = old_texture.len() as u64;
            entry.output_hash = crate::cache::hash_bytes(old_texture);
            manifest.save(&manifest_path).unwrap();

            let report = run_without_progress(config.clone()).await;
            assert!(report.complete);
            assert_eq!(report.converted, 2);
            assert_eq!(report.cache_hits, 1); // Only unchanged PEX.
            assert_eq!(fs::read(output.join(PRUNED_MESH)).unwrap(), expected_mesh);
            assert_eq!(fs::read(&texture_path).unwrap(), expected_texture);
            assert_eq!(
                published_manifest(&output).schema_version,
                CONVERTER_SCHEMA_VERSION
            );

            // Schema migration must not erase a real configuration change.
            let mut manifest = published_manifest(&output);
            manifest.schema_version = old_schema;
            manifest.asset_configuration_hashes.clear();
            manifest.configuration_hash =
                configuration_hash_for_schema(&config, old_schema).unwrap();
            manifest.save(&manifest_path).unwrap();
            let mut changed_config = config;
            changed_config.texture_zstd_level = 0;
            let report = run_without_progress(changed_config).await;
            assert!(report.complete);
            assert_eq!(report.cache_hits, 0);
            assert_eq!(report.converted, 3);
        }
    }

    #[tokio::test]
    async fn resumed_runs_carry_reused_mesh_prunes_forward() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()]))
        );

        // Resume from a staging directory holding exactly what the first run
        // published: the mesh is reused instead of converted again, so the prune
        // pass has nothing left to remove from it.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);

        let (resumed, events) = run_collecting_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.message == "Texture reference pruned")
                .count(),
            0,
            "the resumed run reused the pruned mesh instead of pruning it again"
        );
        assert!(resumed.cache_hits > 0);
        assert_eq!(
            resumed.pruned_texture_references, 1,
            "the prune record of the reused mesh is carried forward"
        );
        let manifest = published_manifest(&output);
        assert!(manifest.complete);
        let entry = &manifest.entries["meshes/dangling_normal.nif"];
        assert_eq!(
            entry.output_hash,
            hash_file(&output.join(PRUNED_MESH)).unwrap()
        );
        assert_eq!(
            manifest.pruned_texture_references.get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()]))
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            !uris.iter().any(|uri| uri.contains("absent")),
            "the record describes the published mesh: {uris:?}"
        );
    }

    /// A run that prunes a mesh and stops before publishing must not lose the prune record when
    /// it is resumed: the staged mesh no longer holds the removed reference, so only converting
    /// it again lets the prune pass find and record it.
    #[tokio::test]
    async fn a_resumed_run_keeps_the_prune_record_of_a_mesh_pruned_before_the_stop() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        // A stale backup beside a live output makes the first run fail at publishing, after the
        // prune pass, and keep its staging directory and journal. Without the output, a lone
        // backup reads as an interrupted swap and is recovered instead.
        fs::create_dir_all(&output).unwrap();
        let backup = output.with_extension(format!("backup-{}", std::process::id()));
        fs::create_dir_all(&backup).unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let staging = error.staging.expect("staging directory was kept");
        let staged_uris = MeshConverter::glb_texture_uris(&staging.join(PRUNED_MESH)).unwrap();
        assert!(
            !staged_uris.iter().any(|uri| uri.contains("absent")),
            "the first run pruned the staged mesh before it stopped: {staged_uris:?}"
        );

        fs::remove_dir_all(&backup).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(resumed.pruned_texture_references, 1);
        let manifest = published_manifest(&output);
        assert_eq!(
            manifest.pruned_texture_references.get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the published manifest records the reference the published mesh omits"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");

        // A normal rerun still reuses the pruned mesh and keeps its record.
        let rerun = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(rerun.complete);
        assert_eq!(rerun.converted, 0);
        assert_eq!(rerun.pruned_texture_references, 1);
    }

    #[tokio::test]
    async fn a_tampered_staged_pruned_mesh_is_not_reused() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        run_without_progress(cpu_lod_config(&data, &output)).await;
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        // The staged copy of the pruned mesh keeps its length but no longer matches the hash the
        // manifest recorded. The published copy is gone, so the cache-hit path cannot replace it
        // and only the reuse gate stands between the tampered bytes and the next publish.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut tampered = expected.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_ne!(tampered, expected);
        fs::write(staging.join(PRUNED_MESH), tampered).unwrap();
        fs::remove_file(output.join(PRUNED_MESH)).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the tampered staged mesh was converted again, not published"
        );
    }

    /// A resume whose staged pruned mesh still holds the bytes the previous manifest recorded
    /// keeps the mesh's prune record even when the published copy is gone: the staged mesh
    /// already omits the reference, so the prune pass cannot record it again.
    #[tokio::test]
    async fn a_verified_staged_pruned_mesh_keeps_its_record_without_a_published_copy() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        fs::remove_file(output.join(PRUNED_MESH)).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the verified staged mesh was published"
        );
        assert_eq!(resumed.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the published manifest records the reference the published mesh omits"
        );

        // The record is what brings the reference back once its texture exists.
        fs::write(
            data.join("textures/absent_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(9),
            )
            .unwrap(),
        )
        .unwrap();
        let restored = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(restored.complete);
        assert_eq!(restored.pruned_texture_references, 0);
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("absent_n")),
            "the restored texture is not referenced: {uris:?}"
        );
    }

    /// The published copy of the pruned mesh is not the only evidence for keeping its record:
    /// when it was replaced by different bytes but the staged mesh still matches the hash the
    /// previous manifest recorded for the same source, the record still describes what is
    /// about to be published again.
    #[tokio::test]
    async fn a_staged_mesh_matching_the_previous_entry_keeps_its_record_over_a_changed_published_copy()
     {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        // The resume copies the published output into staging, then another build republishes
        // different bytes at the same path. The published copy no longer matches the staged
        // mesh, but the staged bytes still match the previous manifest's entry, so the record
        // describes them and must survive.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut rebuilt = expected.clone();
        rebuilt.push(0);
        fs::write(output.join(PRUNED_MESH), rebuilt).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the staged bytes the record describes were published again"
        );
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the staged mesh matched the previous entry, so its record was kept"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");
    }

    /// Stopping and resuming twice in a row must leave the prune record intact each time: the
    /// second stop happens after the first resume published the mesh and its record.
    #[tokio::test]
    async fn repeated_stops_and_resumes_keep_the_prune_record() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        // A stale backup beside a live output makes the run fail at publishing, after the prune
        // pass, and keep its staging directory and journal. Without the output, a lone backup
        // reads as an interrupted swap and is recovered instead.
        let backup = output.with_extension(format!("backup-{}", std::process::id()));

        // First cycle: stop before the first publish, then resume to completion.
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&backup).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(cpu_lod_config(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let staging = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&backup).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let first_resume = run_without_progress(config).await;
        assert!(first_resume.complete);
        assert_eq!(first_resume.pruned_texture_references, 1);

        // Second cycle: stop once more, resuming this time from the published pack, then resume
        // again. The published manifest holds the record through both stops.
        let staging = temp.path().join("modern.staging-again");
        copy_tree(&output, &staging);
        fs::create_dir_all(&backup).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let error = AssetPipeline::run_async(config, tx).await.unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let stopped = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&backup).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(stopped);
        let second_resume = run_without_progress(config).await;

        assert!(second_resume.complete);
        assert_eq!(second_resume.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the record survived two stop-and-resume cycles"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");
    }

    #[tokio::test]
    async fn changed_meshes_do_not_keep_stale_prune_records() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        run_without_progress(cpu_lod_config(&data, &output)).await;

        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        // The mesh's source changed and now names a normal map the game data holds: the stored
        // record describes the old mesh, not the one this run converts and publishes.
        let positions = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0]];
        let shape = dummy_content::nif::StaticShape {
            name: "DanglingNormalQuad",
            positions: &positions,
            normals: &[[0.0, 0.0, 1.0]; 3],
            uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
            indices: &[[0, 1, 2]],
            diffuse: "textures/present.dds",
            normal_texture: "textures/present_n.dds",
        };
        fs::write(
            data.join("meshes/dangling_normal.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("textures/present_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(11),
            )
            .unwrap(),
        )
        .unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(resumed.pruned_texture_references, 0);
        assert!(
            published_manifest(&output)
                .pruned_texture_references
                .is_empty()
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("present_n")),
            "the changed mesh was converted again: {uris:?}"
        );
    }

    /// Pruning rewrites a mesh after its cache entry is written; the refreshed entry keeps the
    /// mesh a cache hit on the next run instead of converting it again.
    #[tokio::test]
    async fn pruned_meshes_remain_cache_hits() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        let second = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(
            second.converted, 0,
            "the pruned mesh was converted again instead of reusing its refreshed entry"
        );
        assert_eq!(second.pruned_texture_references, 1);
        let manifest = published_manifest(&output);
        let entry = manifest
            .entries
            .values()
            .find(|entry| entry.output == PRUNED_MESH)
            .expect("the pruned mesh has a cache entry");
        assert_eq!(
            entry.output_hash,
            hash_file(&output.join(PRUNED_MESH)).unwrap(),
            "the cache entry describes the pruned bytes that were published"
        );
    }

    /// A pruned reference whose source comes back forces the mesh to be converted again, so the
    /// reference is restored instead of staying missing behind a cached GLB.
    #[tokio::test]
    async fn restored_texture_sources_reconvert_the_mesh() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        // The normal map's source appears: the mesh must be converted again to regain it.
        fs::write(
            data.join("textures/absent_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(9),
            )
            .unwrap(),
        )
        .unwrap();

        let second = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(
            second.pruned_texture_references, 0,
            "the restored reference must not stay recorded as absent"
        );
        assert!(
            second.converted >= 1,
            "the mesh was reused without regaining its restored reference"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("absent_n")),
            "the restored texture is not referenced: {uris:?}"
        );
    }

    /// A reused mesh that already omitted one texture and has a second pruned now reports both.
    #[tokio::test]
    async fn a_second_prune_keeps_the_first_record() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        // The base-color source disappears: its reference becomes dangling on the next run while
        // the mesh's own source is unchanged, so the mesh is reused.
        fs::remove_file(data.join("textures/present.dds")).unwrap();
        let second = run_without_progress(cpu_lod_config(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(second.pruned_texture_references, 2);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([
                "textures/absent_n.opensky-wrap0.ktx2".to_owned(),
                "textures/present.opensky-srgb.opensky-wrap0.ktx2".to_owned(),
            ]))
        );
    }

    /// A source texture that exists but failed to publish is not pruned: the reference stays as
    /// the record of a failed publication, not of absent game data.
    #[tokio::test]
    async fn a_missing_artifact_with_a_present_source_is_not_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let shape = dummy_content::nif::StaticShape {
            name: "FailedTextureQuad",
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            indices: &indices,
            diffuse: "textures/present.dds",
            normal_texture: "textures/present_n.dds",
        };
        fs::write(
            data.join("meshes/failed_texture.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        // The base color exists under `vfs` but cannot be converted; the normal map publishes.
        fs::write(data.join("textures/present.dds"), b"not a DDS").unwrap();
        fs::write(
            data.join("textures/present_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(7),
            )
            .unwrap(),
        )
        .unwrap();

        let report = run_without_progress(cpu_lod_config(&data, &output)).await;

        assert!(!report.complete, "the failed texture still skips");
        assert_eq!(report.skipped, 1);
        assert_eq!(
            report.pruned_texture_references, 0,
            "a present source must not be recorded as absent game data"
        );
        let uris =
            MeshConverter::glb_texture_uris(&output.join("meshes/failed_texture.glb")).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("present.")),
            "the failed texture's reference was pruned: {uris:?}"
        );
    }

    #[tokio::test]
    async fn an_interrupt_mid_batch_stops_at_the_next_asset_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        let assets = 300;
        for index in 0..assets {
            let script = dummy_content::pex::minimal("Script").unwrap();
            fs::write(data.join(format!("scripts/script{index:03}.pex")), script).unwrap();
        }

        let cancellation = Cancellation::new();
        let interrupt = cancellation.clone();
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(64);
        let watcher = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if event.message == "Converted asset" {
                    interrupt.cancel();
                }
            }
        });
        let mut config = cpu_lod_config(&data, &output);
        config.cpu_jobs = 1;
        let failure = AssetPipeline::run_async_with_cancel(config, tx, cancellation)
            .await
            .unwrap_err();
        watcher.await.unwrap();

        assert!(failure.cancelled);
        let staging = failure
            .staging
            .clone()
            .expect("an interrupted run keeps staging");
        assert!(staging.is_dir());
        assert!(!output.exists(), "an interrupted run does not publish");
        // The manifest is written once, at the end of a run, so nothing in flight — or after it —
        // is recorded as converted; the resume re-checks the staged files instead.
        assert!(!staging.join("conversion-manifest.json").exists());
        let staged: Vec<_> = fs::read_dir(staging.join("scripts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "luau")
            })
            .collect();
        assert!(
            staged.len() < assets,
            "the run converted all {assets} assets despite the interrupt"
        );
        for file in &staged {
            assert!(
                !fs::read(file).unwrap().is_empty(),
                "{} was left half written",
                file.display()
            );
        }

        // The kept folder resumes the run to completion.
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging);
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert_eq!(report.skipped, 0);
        let converted = fs::read_dir(output.join("scripts"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "luau")
            })
            .count();
        assert!(converted >= assets, "resume left assets behind");
    }

    /// A stop pressed while one large archive is being extracted takes effect inside it, not
    /// when the whole archive is done, and ends the run as an interrupt rather than as a skipped
    /// archive. Verified workspace blobs can survive, but no archive entry is committed; resume
    /// extracts the complete archive and can reuse those blobs.
    #[tokio::test]
    async fn an_interrupt_inside_an_archive_stops_its_extraction_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        let entries = 2048;
        let names: Vec<String> = (0..entries)
            .map(|index| format!("strings/file{index:04}.strings"))
            .collect();
        let contents: Vec<Vec<u8>> = (0..entries)
            .map(|index| format!("entry {index}").into_bytes())
            .collect();
        let archive_entries: Vec<_> = names
            .iter()
            .zip(&contents)
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(&archive_entries, dummy_content::ba2::Compression::None)
                .unwrap(),
        )
        .unwrap();

        // Stop from inside the extractor after its first entries: only the entries already in
        // flight on other threads can still finish, far fewer than the archive holds.
        stop_hook::cancel_after(&output, 16);
        let cancellation = Cancellation::new();
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(64);
        let watcher = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut config = cpu_lod_config(&data, &output);
        config.extract_all_archive_files = true;
        let failure = AssetPipeline::run_async_with_cancel(config.clone(), tx, cancellation)
            .await
            .unwrap_err();
        watcher.await.unwrap();

        assert!(failure.cancelled, "a stop is reported as an interrupt");
        assert!(
            failure
                .error
                .downcast_ref::<Interrupted>()
                .is_some_and(|stop| stop.cause().is_none()),
            "a plain stop inside an archive has no cause to show: {failure}"
        );
        assert!(!output.exists(), "an interrupted run does not publish");
        let staging = failure
            .staging
            .clone()
            .expect("an interrupted run keeps staging");
        let written: Vec<_> = if staging.join("vfs/strings").is_dir() {
            fs::read_dir(staging.join("vfs/strings"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        } else {
            Vec::new()
        };
        assert!(
            written.len() < entries,
            "the whole archive was extracted despite the stop"
        );
        assert!(
            written.iter().all(|name| !name.ends_with(".partial")),
            "a stop left a half-written file behind"
        );
        // Dedup writes complete workspace blobs before linking their VFS names. An interrupt
        // may retain those blobs, but it must not leave partial or incorrectly keyed payloads.
        let workspace = staging.join(".ingestion-cache/sha256");
        let expected_payloads: BTreeMap<_, _> = contents
            .iter()
            .map(|content| (hash_bytes(content), content.as_slice()))
            .collect();
        let mut workspace_blobs = 0;
        if workspace.exists() {
            for entry in WalkDir::new(&workspace) {
                let entry = entry.unwrap();
                if entry.file_type().is_dir() {
                    continue;
                }
                assert!(entry.file_type().is_file(), "unexpected workspace entry");
                let hash = entry.file_name().to_str().unwrap();
                let expected = expected_payloads
                    .get(hash)
                    .expect("workspace blob has an unexpected content hash");
                assert_eq!(entry.path(), workspace.join(&hash[..2]).join(hash));
                assert_eq!(fs::read(entry.path()).unwrap().as_slice(), *expected);
                workspace_blobs += 1;
            }
        }
        assert!(
            workspace_blobs < entries,
            "the whole archive was materialized despite the stop"
        );
        // The archive's cache entry is only recorded once it is complete.
        assert!(!staging.join("conversion-manifest.json").exists());
        assert!(!config.ingestion_cache_dir().exists());

        config.resume_staging = Some(staging);
        let report = run_without_progress(config.clone()).await;
        assert!(report.complete);
        assert!(!output.join("vfs").exists());
        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert_eq!(manifest.archives["assets.ba2"].files.len(), entries);
        let file_entry = manifest.archives["assets.ba2"]
            .files
            .iter()
            .find(|file| file.path == "strings/file2047.strings")
            .expect("file2047.strings not in archive manifest");
        let blob = config
            .ingestion_cache_dir()
            .join(".ingestion-cache/sha256")
            .join(&file_entry.hash[..2])
            .join(&file_entry.hash);
        assert_eq!(fs::read(blob).unwrap(), b"entry 2047");
    }

    /// A front end that stops reading progress must not stall extraction: the per-file updates
    /// find the channel full and are dropped, so the archive's threads run to the end. Only the
    /// events that matter on their own wait for room, and reading again lets the run finish.
    #[tokio::test]
    async fn extraction_does_not_wait_for_a_front_end_that_stopped_reading() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        // Enough entries for several per-file updates, one every 512 files.
        let entries = 2048;
        let names: Vec<String> = (0..entries)
            .map(|index| format!("strings/file{index:04}.strings"))
            .collect();
        let contents: Vec<Vec<u8>> = (0..entries)
            .map(|index| format!("entry {index}").into_bytes())
            .collect();
        let archive_entries: Vec<_> = names
            .iter()
            .zip(&contents)
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(&archive_entries, dummy_content::ba2::Compression::None)
                .unwrap(),
        )
        .unwrap();
        // The extractor caches the files only once every one of them is written, so the last
        // file's blob in the staging cache shows the extraction threads have finished.
        let last_blob = crate::cache::hash_bytes(b"entry 2047");

        // Room for one event: after the archive's first update, the channel stays full.
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(1);
        let front_end = async {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("the run reaches extraction")
                    .expect("the run is still sending");
                if event.stage == ProgressStage::Extracting && event.stage_fraction.is_none() {
                    break;
                }
            }
            // Stop reading until the archive is extracted.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let blob = crate::find_resumable_staging(&output).map(|(staging, _)| {
                    staging
                        .join(".ingestion-cache/sha256")
                        .join(&last_blob[..2])
                        .join(&last_blob)
                });
                if blob.is_some_and(|blob| blob.is_file()) {
                    break;
                }
                if Instant::now() > deadline {
                    // Close the channel first, so stalled threads end and the test fails
                    // instead of hanging.
                    drop(rx);
                    panic!("extraction stalled while nobody read its progress");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Reading again lets the waiting events through.
            while rx.recv().await.is_some() {}
        };
        let mut config = cpu_lod_config(&data, &output);
        config.extract_all_archive_files = true;
        let (report, ()) = tokio::join!(AssetPipeline::run_async(config.clone(), tx), front_end);

        let report = report.unwrap();
        assert!(report.complete);
        let cache_root = config.ingestion_cache_dir();
        let blobs = WalkDir::new(cache_root.join(".ingestion-cache/sha256"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();
        assert_eq!(blobs, entries);
        assert_eq!(report.converted, entries as u64);
    }

    /// The window between the last stage and the publish rename cannot be hit from outside in a
    /// test without racing the run, so the gate itself is driven directly: a run that was
    /// interrupted while it was packing up must not publish, and must keep its staging folder.
    #[test]
    fn an_interrupt_before_the_publish_keeps_staging_and_leaves_the_output_alone() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("modern");

        let staging = temp.path().join("modern.staging-1-1");
        fs::create_dir_all(staging.join("scripts")).unwrap();
        fs::write(staging.join("scripts/one.luau"), b"return 1").unwrap();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let failure = publish_if_not_interrupted(&staging, &output, &cancellation).unwrap_err();
        assert!(failure.cancelled);
        assert!(failure.error.downcast_ref::<Interrupted>().is_some());
        assert!(failure.to_string().contains("interrupted"));
        assert!(!output.exists(), "an interrupted run must not publish");
        assert_eq!(failure.staging.as_deref(), Some(staging.as_path()));
        assert!(staging.is_dir(), "the staging folder is kept for a resume");

        // Without the interrupt the same gate publishes, so the assertion above is about the
        // cancellation and not about publishing being broken.
        let staging = temp.path().join("modern.staging-2-2");
        fs::create_dir_all(staging.join("scripts")).unwrap();
        fs::write(staging.join("scripts/one.luau"), b"return 1").unwrap();
        publish_if_not_interrupted(&staging, &output, &Cancellation::new()).unwrap();
        assert_eq!(
            fs::read(output.join("scripts/one.luau")).unwrap(),
            b"return 1"
        );
    }

    async fn run_without_progress(config: PipelineConfig) -> PipelineReport {
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let report = AssetPipeline::run_async(config, tx).await.unwrap();
        drain.await.unwrap();
        report
    }

    /// Runs the pipeline and returns its report together with every progress
    /// event it emitted.
    async fn run_collecting_progress(
        config: PipelineConfig,
    ) -> (PipelineReport, Vec<ProgressEvent>) {
        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let report = AssetPipeline::run_async(config, tx).await.unwrap();
        (report, collect.await.unwrap())
    }

    #[tokio::test]
    async fn database_cache_cold_warm_invalidation_and_corrupt_receipt_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("modern");
        fs::create_dir(&data).unwrap();
        let plugin = data.join("Skyrim.esm");
        let write_plugin = |author| {
            fs::write(
                &plugin,
                dummy_content::esm::plugin(&dummy_content::esm::Plugin {
                    author,
                    worldspace: "CacheWorld",
                    cells: &[dummy_content::esm::Cell {
                        grid_x: 0,
                        grid_y: 0,
                    }],
                    model_path: "test/cache.nif",
                    diffuse: "test/cache.dds",
                    normal_texture: "test/cache_n.dds",
                })
                .unwrap(),
            )
            .unwrap();
        };
        write_plugin("CacheFixture");
        let mut config = cpu_lod_config(&data, &output);
        config.record_reader = crate::config::RecordReader::Inhouse;
        config.no_lod = true;
        let cold = run_without_progress(config.clone()).await;
        assert!(cold.complete);
        assert!(!cold.database_cache_hit);
        assert!(cold.database_runtime_records_built > 0);
        let source_count = |output: &Path| {
            Connection::open(output.join("skyrim_world.db"))
                .unwrap()
                .query_row::<u64, _, _>("SELECT count(*) FROM inhouse_source_records", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        let winners = source_count(&output);
        let diagnostics = fs::read(output.join("inhouse-reader-diagnostics.json")).unwrap();
        let (warm, events) = run_collecting_progress(config.clone()).await;
        assert!(warm.complete && warm.database_cache_hit);
        assert_eq!(warm.database_runtime_records_built, 0);
        // Database, cell caches, reader identity, notes, diagnostics and profile are retained.
        assert_eq!(warm.database_cache_files, 7);
        assert_eq!(source_count(&output), winners);
        assert_eq!(
            fs::read(output.join("inhouse-reader-diagnostics.json")).unwrap(),
            diagnostics
        );
        assert!(warm.phase_elapsed_ms.contains_key("database_validation"));
        assert!(
            events
                .iter()
                .any(|event| event.message == "Reusing verified skyrim_world.db")
        );
        let profile: serde_json::Value = serde_json::from_slice(
            &fs::read(output.join(crate::esm::inhouse::DATABASE_PROFILE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(profile["cache_hit"], true);
        assert_eq!(profile["timing_source"], "cache_restore");
        assert!(profile["producer_elapsed_seconds"]["total_reader"].is_number());
        assert!(
            !profile["elapsed_seconds"]
                .as_object()
                .unwrap()
                .contains_key("total_reader")
        );
        let receipt = config
            .ingestion_cache_dir()
            .join(".database-cache")
            .join(warm.database_cache_key.as_ref().unwrap())
            .join("receipt.json");
        fs::write(&receipt, b"{truncated").unwrap();
        assert!(
            !run_without_progress(config.clone())
                .await
                .database_cache_hit
        );
        assert!(
            run_without_progress(config.clone())
                .await
                .database_cache_hit
        );
        let mut invalidated = config.clone();
        invalidated.invalidate_cache = true;
        assert!(!run_without_progress(invalidated).await.database_cache_hit);
        write_plugin("ChangedPluginBytes");
        assert!(
            !run_without_progress(config.clone())
                .await
                .database_cache_hit
        );
        fs::create_dir(data.join("Strings")).unwrap();
        fs::write(
            data.join("Strings/Skyrim_english.strings"),
            b"malformed bank bytes",
        )
        .unwrap();
        assert!(
            !run_without_progress(config.clone())
                .await
                .database_cache_hit
        );
        config.record_reader = crate::config::RecordReader::Legacy;
        assert!(!run_without_progress(config).await.database_cache_hit);
    }

    /// Loads the manifest published in an output directory.
    fn published_manifest(output: &Path) -> ConversionManifest {
        ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap()
    }

    /// Copies a published asset tree into a staging directory, so a run can
    /// resume from it.
    fn copy_tree(source: &Path, destination: &Path) {
        for entry in WalkDir::new(source) {
            let entry = entry.unwrap();
            let relative = entry.path().strip_prefix(source).unwrap();
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target).unwrap();
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn published_pack_excludes_build_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        let config = cpu_lod_config(&data, &output);
        let report = run_without_progress(config.clone()).await;

        assert!(report.complete);
        assert!(output.join("scripts/one.luau").is_file());
        assert!(output.join("conversion-manifest.json").is_file());
        assert!(!output.join("vfs").exists());
        assert!(!output.join(".ingestion-cache").exists());
    }

    #[test]
    fn persisting_the_ingestion_cache_skips_spill_copies() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        fs::create_dir_all(&staging).unwrap();
        let blob = hash_bytes(b"blob");
        fs::write(staging.join(&blob), b"blob").unwrap();
        fs::write(staging.join(format!("{blob}.1")), b"spill").unwrap();
        fs::write(staging.join(".tmp-abandoned-blob"), b"unfinished blob").unwrap();
        let root = directory.path().join("cache");

        persist_ingestion_cache(&staging, &root).unwrap();

        assert!(root.join(&blob).is_file());
        assert!(!root.join(format!("{blob}.1")).exists());
        assert!(!root.join(".tmp-abandoned-blob").exists());
    }

    #[tokio::test]
    async fn resumed_ingestion_drops_unsealed_packs_and_retains_warm_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        let staging = directory.path().join("assets.staging-abandoned-packs");
        fs::create_dir_all(&data).unwrap();
        let archive = dummy_content::ba2::general(
            &[dummy_content::Entry::new(
                "strings/review_english.strings",
                b"input",
            )],
            dummy_content::ba2::Compression::None,
        )
        .unwrap();
        fs::write(data.join("review.ba2"), &archive).unwrap();
        let recipe = PathBuf::from("batches")
            .join(hash_bytes(&archive))
            .join(IngestionSelection::RuntimeEnglishV1.recipe());
        let pack_directory = staging.join(".ingestion-cache").join(&recipe);
        fs::create_dir_all(&pack_directory).unwrap();
        let abandoned = [
            ".tmp-abandoned-pack",
            "unfinished.partial",
            "unfinished.pack",
        ];
        for name in abandoned {
            fs::write(pack_directory.join(name), b"unsealed bytes").unwrap();
        }
        let mut config = cpu_lod_config(&data, &output);
        let cache = config.ingestion_cache_dir().join(".ingestion-cache");
        config.resume_staging = Some(staging.clone());
        assert!(run_without_progress(config.clone()).await.complete);
        assert!(!staging.exists());
        for name in abandoned {
            assert!(!cache.join(&recipe).join(name).exists());
        }
        assert_eq!(fs::read_dir(cache.join(&recipe)).unwrap().count(), 1);

        config.resume_staging = None;
        let warm = run_without_progress(config).await;
        assert!(warm.complete);
        assert_eq!(warm.cache_hits, 1);
        assert_eq!(warm.converted, 0);
    }

    #[test]
    fn persisting_durable_packs_repairs_existing_corrupt_copies() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        let root = directory.path().join("cache");
        let relative = PathBuf::from("batches")
            .join("ab".repeat(32))
            .join(IngestionSelection::ConverterInputsV1.recipe())
            .join(format!("{}.pack", hash_bytes(b"sealed batch")));
        fs::create_dir_all(staging.join(&relative).parent().unwrap()).unwrap();
        fs::create_dir_all(root.join(&relative).parent().unwrap()).unwrap();
        fs::write(staging.join(&relative), b"sealed batch").unwrap();
        fs::write(root.join(&relative), b"damaged batch").unwrap();
        persist_ingestion_cache(&staging, &root).unwrap();
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"sealed batch");
    }

    /// Persist a relative cache destination from a private cwd independent of checkout depth.
    #[test]
    fn durable_cache_persistence_accepts_relative_destinations() {
        const CHILD: &str = "MUDCRAB_TEST_RELATIVE_CACHE_CHILD";
        const TEST: &str =
            "pipeline::tests::durable_cache_persistence_accepts_relative_destinations";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            // Keep process-global cwd unchanged for other tests running in parallel.
            let directory = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .current_dir(directory.path())
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout);
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(
                result.status.success(),
                "relative-cache subprocess failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                stdout.contains(TEST) && stdout.contains("1 passed"),
                "subprocess did not run the relative-cache test\nstdout:\n{stdout}"
            );
            return;
        }
        let directory = tempfile::tempdir_in(".").unwrap();
        let staging = directory.path().join("staging-cache");
        let current_directory = std::env::current_dir().unwrap();
        let root = directory
            .path()
            .strip_prefix(&current_directory)
            .unwrap()
            .join("new-parent/cache");
        assert!(!root.is_absolute());
        let relative = PathBuf::from("batches")
            .join("ab".repeat(32))
            .join(IngestionSelection::ConverterInputsV1.recipe())
            .join(format!("{}.pack", hash_bytes(b"sealed batch")));
        fs::create_dir_all(staging.join(&relative).parent().unwrap()).unwrap();
        fs::write(staging.join(&relative), b"sealed batch").unwrap();
        persist_ingestion_cache(&staging, &root).unwrap();
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"sealed batch");
    }

    #[test]
    fn persisting_ingestion_blobs_repairs_existing_corrupt_copies() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        let root = directory.path().join("cache");
        let hash = hash_bytes(b"valid blob");
        let relative = PathBuf::from("sha256").join(&hash[..2]).join(&hash);
        fs::create_dir_all(staging.join(&relative).parent().unwrap()).unwrap();
        fs::create_dir_all(root.join(&relative).parent().unwrap()).unwrap();
        fs::write(staging.join(&relative), b"valid blob").unwrap();
        fs::write(root.join(&relative), b"wrong blob").unwrap();
        persist_ingestion_cache(&staging, &root).unwrap();
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"valid blob");
    }

    #[test]
    fn persisting_the_ingestion_cache_repairs_corruption_without_mutating_linked_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        let root = directory.path().join("cache");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&root).unwrap();
        let expected = b"healthy blob";
        let hash = hash_bytes(expected);
        let source = staging.join(&hash);
        let destination = root.join(&hash);
        fs::write(&source, expected).unwrap();
        fs::write(&destination, b"corrupt blob").unwrap();
        let linked_output = directory.path().join("old-output");
        link_or_copy(&destination, &linked_output).unwrap();

        persist_ingestion_cache(&staging, &root).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), expected);
        assert_eq!(fs::read(&linked_output).unwrap(), b"corrupt blob");
        assert_eq!(hash_file(&destination).unwrap(), hash);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn persisting_the_ingestion_cache_replaces_symlinks_without_touching_their_targets() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        let root = directory.path().join("cache");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&root).unwrap();
        let bytes = b"verified payload";
        let hash = hash_bytes(bytes);
        let outside = directory.path().join("outside");
        fs::write(&outside, bytes).unwrap();
        fs::write(staging.join(&hash), bytes).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(&hash)).unwrap();
        persist_ingestion_cache(&staging, &root).unwrap();
        assert!(fs::symlink_metadata(root.join(&hash)).unwrap().is_file());
        assert_eq!(fs::read(root.join(&hash)).unwrap(), bytes);
        assert_eq!(fs::read(&outside).unwrap(), bytes);
    }

    #[test]
    fn persisting_the_ingestion_cache_preserves_healthy_blobs() {
        for linked in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path().join("staging-cache");
            let root = directory.path().join("cache");
            fs::create_dir_all(&staging).unwrap();
            fs::create_dir_all(&root).unwrap();
            let expected = b"healthy blob";
            let hash = hash_bytes(expected);
            let source = staging.join(&hash);
            let destination = root.join(&hash);
            fs::write(&source, expected).unwrap();
            if linked {
                link_or_copy(&source, &destination).unwrap();
            } else {
                fs::write(&destination, expected).unwrap();
            }
            let before = fs::metadata(&destination).unwrap();

            persist_ingestion_cache(&staging, &root).unwrap();

            let after = fs::metadata(&destination).unwrap();
            assert_eq!(before.len(), after.len());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            }
            assert_eq!(fs::read(&destination).unwrap(), expected);
            if !linked {
                fs::write(&source, b"changed blob").unwrap();
                assert_eq!(fs::read(&destination).unwrap(), expected);
            }
        }
    }

    #[test]
    fn persisting_the_ingestion_cache_rejects_a_corrupt_repair_source() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        let root = directory.path().join("cache");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&root).unwrap();
        let hash = hash_bytes(b"healthy blob");
        let destination = root.join(&hash);
        fs::write(staging.join(&hash), b"invalid blob").unwrap();
        fs::write(&destination, b"corrupt blob").unwrap();

        let error = persist_ingestion_cache(&staging, &root).unwrap_err();

        assert!(error.to_string().contains("SHA-256 filename"));
        assert_eq!(fs::read(&destination).unwrap(), b"corrupt blob");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn persistent_archive_corruption_is_repaired_and_reused_on_the_next_run() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("modern");
        write_archived_script(&data);
        let config = cpu_lod_config(&data, &output);
        let first = run_without_progress(config.clone()).await;
        assert!(first.complete);
        assert_eq!(first.converted, 2);
        let manifest = published_manifest(&output);
        let file = &manifest.archives.values().next().unwrap().files[0];
        let blob = config
            .ingestion_cache_dir()
            .join(".ingestion-cache/sha256")
            .join(&file.hash[..2])
            .join(&file.hash);
        let expected = fs::read(&blob).unwrap();
        fs::write(&blob, vec![b'x'; expected.len()]).unwrap();

        let recovered = run_without_progress(config.clone()).await;

        assert!(recovered.complete);
        assert_eq!(
            recovered.converted, 0,
            "sealed packs recover raw bytes without archive decoding"
        );
        assert_eq!(
            recovered.cache_hits, 2,
            "the sealed archive and unchanged script can be reused"
        );
        assert_eq!(fs::read(&blob).unwrap(), expected);
        assert_eq!(hash_file(&blob).unwrap(), file.hash);

        let reused = run_without_progress(config).await;

        assert!(reused.complete);
        assert_eq!(reused.converted, 0);
        assert_eq!(reused.cache_hits, 2);
        assert_eq!(fs::read(&blob).unwrap(), expected);
    }

    #[test]
    fn prune_removes_only_unreferenced_blobs_and_spills() {
        use crate::cache::{IngestedFile, IngestionCacheEntry};

        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join(".ingestion-cache/sha256/ab");
        fs::create_dir_all(&cache).unwrap();
        let live = "ab".repeat(32);
        let dead = "cd".repeat(32);
        fs::write(cache.join(&live), b"live").unwrap();
        fs::write(cache.join(format!("{live}.1")), b"live spill").unwrap();
        fs::write(cache.join(&dead), b"dead").unwrap();
        fs::write(cache.join(format!("{dead}.2")), b"dead spill").unwrap();
        fs::write(cache.join("probe.tmp"), b"not a blob").unwrap();
        let packs = directory.path().join(".ingestion-cache/batches");
        let live_packs = packs
            .join("00".repeat(32))
            .join(IngestionSelection::ConverterInputsV1.recipe());
        let dead_packs = packs
            .join("ff".repeat(32))
            .join(IngestionSelection::ConverterInputsV1.recipe());
        fs::create_dir_all(&live_packs).unwrap();
        fs::create_dir_all(&dead_packs).unwrap();
        fs::write(live_packs.join(format!("{dead}.pack")), b"live pack").unwrap();
        fs::write(dead_packs.join(format!("{dead}.pack")), b"stale pack").unwrap();

        let mut manifest = ConversionManifest::default();
        manifest.archives.insert(
            "assets.ba2".to_owned(),
            IngestionCacheEntry {
                source_hash: "00".repeat(32),
                selection: IngestionSelection::All,
                recipe: String::new(),
                files: vec![IngestedFile {
                    path: "textures/rock.dds".to_owned(),
                    size: 4,
                    hash: live.clone(),
                }],
                reader_version: 0,
            },
        );
        prune_stale_ingestion_blobs(
            directory.path().join(".ingestion-cache").as_path(),
            &manifest,
        )
        .unwrap();

        assert!(cache.join(&live).is_file());
        assert!(cache.join(format!("{live}.1")).is_file());
        assert!(!cache.join(&dead).exists());
        assert!(!cache.join(format!("{dead}.2")).exists());
        assert!(cache.join("probe.tmp").is_file());
        assert!(live_packs.join(format!("{dead}.pack")).is_file());
        assert!(!dead_packs.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn a_resumed_publish_removes_its_staging_and_keeps_the_output() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        run_without_progress(cpu_lod_config(&data, &output)).await;
        let expected = fs::read(output.join("scripts/one.luau")).unwrap();

        // The published pack shares files with staging through hard links, so removing the
        // resumed staging folder must leave every published file in place.
        let staging = temp.path().join("modern.staging-resume-links");
        fs::create_dir_all(&staging).unwrap();
        let mut config = cpu_lod_config(&data, &output);
        config.resume_staging = Some(staging.clone());
        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert!(
            !staging.exists(),
            "a resumed publish kept its staging folder, which would be offered for resume again"
        );
        assert_eq!(fs::read(output.join("scripts/one.luau")).unwrap(), expected);
        assert_eq!(crate::find_resumable_staging(&output), None);
    }

    #[test]
    fn staging_that_holds_the_data_folder_is_kept_after_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("modern.staging-1-1");
        let data = staging.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), b"game data").unwrap();

        assert!(!remove_staging(&staging, &data));
        assert!(!remove_staging(&staging, &staging));
        assert_eq!(fs::read(data.join("Skyrim.esm")).unwrap(), b"game data");

        // A staging folder beside the data, the normal case, is removed.
        let beside = temp.path().join("modern.staging-2-2");
        fs::create_dir_all(beside.join("vfs")).unwrap();
        assert!(remove_staging(&beside, &data));
        assert!(!beside.exists());
        assert!(data.join("Skyrim.esm").is_file());
    }

    /// A staging folder with `count` valid artifacts of each kind validation checks, the
    /// artifact list and the texture semantics the KTX2s were encoded for.
    fn staging_with_valid_artifacts(
        staging: &Path,
        count: usize,
    ) -> (Vec<PathBuf>, BTreeMap<String, BTreeSet<TextureSemantic>>) {
        let dds = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 4, 4),
            &mut dummy_content::rng::Rng::new(0),
        )
        .unwrap();
        let uses = BTreeSet::from([TextureSemantic::BaseColor]);
        let ktx2 = crate::texture::TextureConverter::convert(
            &dds,
            TextureEncoding::from_semantics(&uses).unwrap(),
        )
        .unwrap();
        for folder in ["textures", "meshes", "scripts"] {
            fs::create_dir_all(staging.join(folder)).unwrap();
        }
        let mut artifacts = Vec::new();
        let mut semantics = BTreeMap::new();
        for index in 0..count {
            let texture = PathBuf::from(format!("textures/t{index}.ktx2"));
            fs::write(staging.join(&texture), &ktx2).unwrap();
            semantics.insert(format!("textures/t{index}.ktx2"), uses.clone());
            let mesh = PathBuf::from(format!("meshes/m{index}.glb"));
            fs::write(staging.join(&mesh), b"glTF\x02\x00\x00\x00\x0c\x00\x00\x00").unwrap();
            let script = PathBuf::from(format!("scripts/s{index}.luau"));
            fs::write(staging.join(&script), format!("return {index}")).unwrap();
            artifacts.extend([texture, mesh, script]);
        }
        (artifacts, semantics)
    }

    #[test]
    fn validates_a_mix_of_good_artifacts_on_one_thread_and_many() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 16);
        for jobs in [1, 4] {
            validate_artifacts(staging, &artifacts, &semantics, jobs)
                .unwrap_or_else(|error| panic!("jobs {jobs}: {error:#}"));
        }
    }

    #[test]
    fn rejects_one_bad_artifact_of_each_kind_among_good_ones() {
        let cases: [(&str, &[u8], &str); 3] = [
            ("textures/bad.ktx2", b"not a texture", "invalid KTX2"),
            (
                "meshes/bad.glb",
                b"glTX\x02\x00\x00\x00\x0c\x00\x00\x00",
                "invalid GLB artifact",
            ),
            ("scripts/bad.luau", b"return (", "invalid Luau artifact"),
        ];
        for (relative, bytes, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path();
            let (mut artifacts, mut semantics) = staging_with_valid_artifacts(staging, 16);
            fs::write(staging.join(relative), bytes).unwrap();
            semantics.insert(
                "textures/bad.ktx2".to_owned(),
                BTreeSet::from([TextureSemantic::BaseColor]),
            );
            // In the middle of the list, so a parallel run meets good files on both sides.
            artifacts.insert(artifacts.len() / 2, PathBuf::from(relative));
            for jobs in [1, 4] {
                let error =
                    validate_artifacts(staging, &artifacts, &semantics, jobs).expect_err(relative);
                let chain = format!("{error:#}");
                assert!(
                    chain.contains(expected) && chain.contains("bad."),
                    "jobs {jobs}: {chain}"
                );
            }
        }
    }

    #[test]
    fn reports_the_first_bad_artifact_when_several_fail() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 16);
        // `m1` comes before `m10` in the artifact list; both fail the GLB check the same way.
        for relative in ["meshes/m1.glb", "meshes/m10.glb"] {
            fs::write(
                staging.join(relative),
                b"glTX\x02\x00\x00\x00\x0c\x00\x00\x00",
            )
            .unwrap();
        }
        let expected = staging.join("meshes/m1.glb").display().to_string();
        for jobs in [1, 4, 16] {
            let error = validate_artifacts(staging, &artifacts, &semantics, jobs)
                .expect_err("both meshes are invalid");
            let chain = format!("{error:#}");
            assert!(
                chain.contains(&expected) && !chain.contains("m10.glb"),
                "jobs {jobs}: {chain}"
            );
        }
    }

    #[test]
    fn a_glb_shorter_than_its_header_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 2);
        fs::write(staging.join("meshes/m1.glb"), b"glTF\x02\x00").unwrap();
        let error = validate_artifacts(staging, &artifacts, &semantics, 2)
            .expect_err("a 6-byte GLB has no full header");
        assert!(
            format!("{error:#}").contains("invalid GLB artifact"),
            "{error:#}"
        );
    }

    /// Times artifact validation on a real converted output, for before/after comparisons.
    ///
    /// `MUDCRAB_VALIDATE_OUTPUT` names a converted output folder, which is only read.
    /// `MUDCRAB_VALIDATE_LIMIT` caps the artifact count by sampling evenly across the sorted
    /// manifest; this does not guarantee every artifact kind is represented.
    /// `MUDCRAB_VALIDATE_JOBS` sets the thread count (default: every core, as a conversion does).
    /// Run with
    /// `cargo test --release -p converter validation_timing_on_a_real_output -- --ignored --nocapture`.
    ///
    /// Checks every manifest artifact present in this output (packages can contain a subset).
    /// Failures are counted per kind and fail the test; timing never discards invalid artifacts.
    #[test]
    #[ignore = "needs a converted output; set MUDCRAB_VALIDATE_OUTPUT"]
    fn validation_timing_on_a_real_output() -> Result<()> {
        use std::time::Instant;

        let output = std::env::var_os("MUDCRAB_VALIDATE_OUTPUT")
            .or_else(|| std::env::var_os("OPENSKYRIM_VALIDATE_OUTPUT"))
            .map(PathBuf::from)
            .expect("set MUDCRAB_VALIDATE_OUTPUT to run real-output validation");
        let limit = std::env::var("MUDCRAB_VALIDATE_LIMIT")
            .or_else(|_| std::env::var("OPENSKYRIM_VALIDATE_LIMIT"))
            .ok()
            .map(|value| value.parse::<usize>().expect("MUDCRAB_VALIDATE_LIMIT"));
        let jobs = std::env::var("MUDCRAB_VALIDATE_JOBS")
            .or_else(|_| std::env::var("OPENSKYRIM_VALIDATE_JOBS"))
            .map_or_else(
                |_| std::thread::available_parallelism().map_or(1, usize::from),
                |value| value.parse::<usize>().expect("MUDCRAB_VALIDATE_JOBS"),
            );

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(output.join("conversion-manifest.json")).expect("read manifest"),
        )
        .expect("parse manifest");
        let mut artifacts: Vec<PathBuf> = manifest["entries"]
            .as_object()
            .expect("manifest entries")
            .values()
            .filter_map(|entry| entry["output"].as_str())
            .map(PathBuf::from)
            .filter(|relative| output.join(relative).is_file())
            .collect();
        artifacts.sort();
        artifacts.dedup();
        if let Some(limit) = limit.filter(|&limit| limit > 0 && limit < artifacts.len()) {
            let total = artifacts.len();
            artifacts = (0..limit)
                .map(|index| {
                    // Widened to u64: `index * total` would overflow a 32-bit usize.
                    let sampled = index as u64 * total as u64 / limit as u64;
                    artifacts[sampled as usize].clone()
                })
                .collect();
        }
        let mut by_kind = BTreeMap::<String, usize>::new();
        for relative in &artifacts {
            *by_kind.entry(artifact_kind(relative)).or_default() += 1;
        }

        // Collecting semantics reads every GLB and takes minutes on a full install.
        let started = Instant::now();
        let texture_semantics = collect_texture_semantics(&output, &BTreeSet::new())
            .wrap_err("collect texture semantics")?;
        eprintln!(
            "texture semantics: {} textures in {:.2} s (not part of the timing)",
            texture_semantics.len(),
            started.elapsed().as_secs_f64()
        );

        eprintln!(
            "validating {} artifacts from {} on {jobs} threads ({by_kind:?})",
            artifacts.len(),
            output.display()
        );
        time_artifact_validation(&output, &artifacts, &texture_semantics, jobs)
            .wrap_err("validate selected output artifacts")?;
        Ok(())
    }

    /// Times selected-artifact validation, reporting failures while preserving the original result.
    fn time_artifact_validation(
        output: &Path,
        artifacts: &[PathBuf],
        texture_semantics: &BTreeMap<String, BTreeSet<TextureSemantic>>,
        jobs: usize,
    ) -> Result<()> {
        ensure!(
            !artifacts.is_empty(),
            "no output artifacts selected for validation"
        );
        let started = std::time::Instant::now();
        let result = validate_artifacts(output, artifacts, texture_semantics, jobs);
        let elapsed = started.elapsed().as_secs_f64();
        let mut errors = BTreeMap::<String, (usize, String)>::new();
        if result.is_err() {
            let mut lua = None;
            for relative in artifacts {
                if let Err(error) = validate_artifact(output, relative, texture_semantics, &mut lua)
                {
                    let slot = errors
                        .entry(artifact_kind(relative))
                        .or_insert_with(|| (0, format!("{error:#}")));
                    slot.0 += 1;
                    if slot.0 <= 10 {
                        eprintln!("invalid output {}: {error:#}", relative.display());
                    }
                }
            }
            for (kind, (count, first)) in &errors {
                eprintln!("errors[{kind}] = {count}; first: {first}");
            }
        }
        let failed: usize = errors.values().map(|(count, _)| count).sum();
        eprintln!(
            "RESULT artifacts={} seconds={elapsed:.2} errors={failed}",
            artifacts.len()
        );
        // Keep the original result even if files change during diagnostic enumeration.
        result
    }

    /// Checks semantic collection and contextual artifact errors through the real-output harness.
    #[test]
    fn timing_validation_collects_semantics_in_subprocess_even_if_skip_is_set() {
        use std::process::Command;

        let directory = tempfile::tempdir().unwrap();
        let output = directory.path();
        fs::create_dir_all(output.join("textures")).unwrap();
        fs::create_dir_all(output.join("meshes")).unwrap();

        let dds = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 4, 4),
            &mut dummy_content::rng::Rng::new(1),
        )
        .unwrap();
        let ktx2 = TextureConverter::convert(&dds, TextureEncoding::ColorSrgb).unwrap();
        fs::write(output.join("textures/srgb.ktx2"), ktx2).unwrap();

        let mut json = serde_json::to_vec(&serde_json::json!({
            "asset": { "version": "2.0" },
            "images": [{ "uri": "../textures/srgb.ktx2" }],
            "textures": [{ "source": 0 }],
            "materials": [{
                "pbrMetallicRoughness": { "baseColorTexture": { "index": 0 } }
            }]
        }))
        .unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&u32::try_from(20 + json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(&u32::try_from(json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        fs::write(output.join("meshes/material.glb"), &glb).unwrap();
        fs::write(
            output.join("conversion-manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "entries": {
                    "textures/srgb.dds": { "output": "textures/srgb.ktx2" },
                    "meshes/material.nif": { "output": "meshes/material.glb" }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let run_timing = || {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipeline::tests::validation_timing_on_a_real_output",
                    "--ignored",
                    "--nocapture",
                ])
                .env("MUDCRAB_VALIDATE_OUTPUT", output)
                .env("MUDCRAB_VALIDATE_LIMIT", "0")
                .env("MUDCRAB_VALIDATE_JOBS", "2")
                .env("MUDCRAB_VALIDATE_SEMANTICS", "skip")
                .output()
                .unwrap()
        };
        let result = run_timing();
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            result.status.success(),
            "real-output timing subprocess failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed")
                && stdout.contains("pipeline::tests::validation_timing_on_a_real_output"),
            "subprocess did not run the ignored timing test\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("RESULT artifacts=2") && stderr.contains("errors=0"),
            "real-output test did not validate the complete artifact set\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        for (artifact, context) in [
            ("meshes/material.glb", "collect texture semantics"),
            ("textures/srgb.ktx2", "validate selected output artifacts"),
        ] {
            fs::write(output.join("meshes/material.glb"), &glb).unwrap();
            fs::write(output.join(artifact), b"broken").unwrap();
            let result = run_timing();
            let diagnostic = format!(
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                !result.status.success(),
                "accepted {artifact}: {diagnostic}"
            );
            assert!(diagnostic.contains(context), "{artifact}: {diagnostic}");
            assert!(
                !diagnostic.contains("panicked at"),
                "{artifact} must return an error: {diagnostic}"
            );
        }
    }

    /// Checks that timing rejects a corrupt artifact or an empty selection after a valid run.
    #[test]
    fn v200_timing_validation_rejects_any_bad_artifact_and_empty_selection() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(output, 2);
        time_artifact_validation(output, &artifacts, &semantics, 2).unwrap();
        fs::write(output.join("meshes/m1.glb"), b"broken").unwrap();
        let error = time_artifact_validation(output, &artifacts, &semantics, 2).unwrap_err();
        assert!(format!("{error:#}").contains("invalid GLB artifact"));
        assert!(time_artifact_validation(output, &[], &semantics, 2).is_err());
    }

    fn artifact_kind(relative: &Path) -> String {
        relative
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    }
}
