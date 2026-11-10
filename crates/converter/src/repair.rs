//! Targeted repair of a published pack, including asset-aware database finalization.
use crate::{
    archive::{ArchiveExtractor, archive_reader_version, safe_relative_path},
    asset_path::{AssetKind, canonical_asset_path, is_authoring_resource, resolve_asset_uri},
    cache::{
        CONVERTER_SCHEMA_VERSION, CacheEntry, ConversionManifest, configuration_hash, hash_file,
        link_or_copy,
    },
    check::{CheckMode, CheckProblem, check_output},
    config::{PipelineConfig, TextureEncoder},
    integration::finalize_world_database_with_sources,
    mesh::MeshConverter,
    pipeline::{
        collect_texture_semantics, installed_archive_owners, mo2_archive_is_active,
        mo2_installed_archive_owners, non_mo2_archive_is_active, plugin_paths,
        publish_srgb_texture_aliases, sort_archives_by_load_order, source_texture_key,
        validate_artifact,
    },
    script::ScriptConverter,
    texture::{TextureConverter, TextureEncoding, publish_ktx2_file},
    texture_gpu::{self, GpuJob, GpuUastc, PreparedTexture},
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Serialize)]
pub struct RepairReport {
    pub directory: PathBuf,
    pub converted: usize,
    pub excluded: usize,
    pub published: bool,
    /// Record data was verified complete by its last authoritative conversion.
    /// False also covers historical manifests without that proof.
    pub record_data_complete: bool,
    pub failures: BTreeMap<String, String>,
    /// Optional capability declarations dropped because their payload is no longer valid.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Source {
    path: PathBuf,
    archive: Option<(PathBuf, String)>,
    expected_hash: Option<String>,
}

/// Maps a supported source extension to its asset kind, source extension, and output extension.
fn kind(path: &str) -> Option<(AssetKind, &'static str, &'static str)> {
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "nif" => Some((AssetKind::Mesh, "nif", "glb")),
        "dds" => Some((AssetKind::Texture, "dds", "ktx2")),
        "pex" => Some((AssetKind::Script, "pex", "luau")),
        _ => None,
    }
}

/// Shared conversion/repair ownership. Keep this guard alive through publication.
/// The lock lives beside the output, so renaming the output cannot change its identity.
pub struct OutputOwnership {
    output: PathBuf,
    _lock: fs::File,
}

impl OutputOwnership {
    /// Acquires exclusive ownership and recovers any interrupted repair before
    /// callers inspect the previous manifest. Also supports first-time outputs.
    pub fn acquire(output: &Path) -> Result<Self> {
        let name = output
            .file_name()
            .ok_or_else(|| color_eyre::eyre::eyre!("output must name a directory"))?;
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        // Never canonicalize the leaf: it may be temporarily absent during a
        // conversion's directory swap, while its sibling lock remains held.
        let output = fs::canonicalize(parent)?.join(name);
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(sibling_path(&output, "repair.lock"))?;
        lock.try_lock()
            .wrap_err("another conversion or repair is using this output")?;
        if let Ok(metadata) = fs::symlink_metadata(&output) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "output directory cannot be a symlink"
            );
        }
        recover_publication(&output)?;
        Ok(Self {
            output,
            _lock: lock,
        })
    }

    pub fn output(&self) -> &Path {
        &self.output
    }
}

/// Without `apply`, only a repair directory inside the output is written. Keep it for inspection.
pub fn repair_failed(config: &PipelineConfig, apply: bool) -> Result<RepairReport> {
    config.validate()?;
    ensure!(
        config.resume_staging.is_none() && !config.invalidate_cache,
        "repair cannot resume staging or invalidate the pack"
    );
    let _asset_lock = shared::asset_lock::AssetLock::acquire_exclusive(&config.output_dir)
        .wrap_err("cannot repair an asset directory in use")?;
    let ownership = OutputOwnership::acquire(&config.output_dir)?;
    let output = ownership.output().to_path_buf();
    let mut pinned_config = config.clone();
    pinned_config.output_dir = output.clone();
    let config = &pinned_config;
    let manifest_path = config.output_dir.join("conversion-manifest.json");
    let original_bytes = fs::read(&manifest_path)?;
    let mut manifest: ConversionManifest = serde_json::from_slice(&original_bytes)?;
    ensure!(
        manifest.schema_version == CONVERTER_SCHEMA_VERSION,
        "repair requires the current manifest schema"
    );
    ensure!(
        !manifest.failures.is_empty(),
        "manifest has no failed inputs to repair"
    );
    ensure!(
        manifest.configuration_hash == configuration_hash(config)?,
        "repair configuration differs from the original conversion; use the same Data, MO2 instance/profile and encoding settings"
    );
    ensure!(
        manifest.asset_configuration_hashes.get("nif")
            == Some(&crate::cache::asset_configuration_hash_for_schema(
                config,
                CONVERTER_SCHEMA_VERSION,
                "nif"
            )?),
        "repair mesh output contract is stale or missing; run normal conversion to regenerate models"
    );
    check_existing(
        &config.output_dir,
        if apply {
            CheckMode::Full
        } else {
            CheckMode::Quick
        },
    )?;

    let directory = output.join(format!(
        ".repair-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&directory).wrap_err_with(|| {
        format!(
            "repair directory already exists or cannot be created: {}",
            directory.display()
        )
    })?;
    let staged = directory.join("assets");
    fs::create_dir(&staged)?;
    // Discover semantics from a pack-only snapshot, never earlier repair previews.
    let published = directory.join("published");
    fs::create_dir(&published)?;
    for entry in WalkDir::new(&output)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with(".repair-")
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(&output)?;
            let destination = checked_destination(&published, &relative.to_string_lossy())?;
            fs::create_dir_all(destination.parent().unwrap())?;
            link_or_copy(entry.path(), &destination)?;
        }
    }
    let mut report = RepairReport {
        directory: directory.clone(),
        converted: 0,
        excluded: 0,
        published: false,
        record_data_complete: manifest.record_data_complete == Some(true),
        failures: BTreeMap::new(),
        warnings: Vec::new(),
    };

    let mut selection_notices = Vec::new();
    let (resolved, plugins) = if let Some(selection) = &config.mo2 {
        let resolved = mo2::Instance::open(&selection.instance_path)?
            .resolve(&config.data_dir, &selection.profile)?;
        selection_notices.extend(resolved.notices);
        selection_notices.extend(
            resolved
                .selection_warnings
                .into_iter()
                .map(|warning| warning.to_string()),
        );
        let plugins = resolved
            .plugins
            .iter()
            .map(|key| resolved.files[key].clone())
            .collect();
        let plugins = crate::esm::plugins_txt::recover_with_diagnostics(
            plugins,
            &mut selection_notices,
            &mut Vec::new(),
        )?;
        (resolved.files, plugins)
    } else {
        let mut resolved = BTreeMap::new();
        for entry in WalkDir::new(&config.data_dir).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let key = entry
                    .path()
                    .strip_prefix(&config.data_dir)?
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_lowercase();
                ensure!(
                    resolved.insert(key.clone(), entry.into_path()).is_none(),
                    "source collision: {key}"
                );
            }
        }
        let files = resolved.values().cloned().collect::<Vec<_>>();
        let plugins = plugin_paths(config, &files, &mut selection_notices)?;
        (resolved, plugins)
    };
    for notice in selection_notices {
        eprintln!("Repair input: {notice}");
    }
    // Repair uses the same installed-root ownership and recovered selection as conversion.
    // Retain omitted owners so a shorter active stem cannot supply their companion bytes.
    let archive_owners = if config.mo2.is_some() {
        mo2_installed_archive_owners(&resolved, &plugins)
    } else {
        let files = resolved.values().cloned().collect::<Vec<_>>();
        installed_archive_owners(&config.data_dir, &files, &plugins)
    };
    let mut archives: Vec<_> = resolved
        .iter()
        .filter(|(key, path)| {
            (key.ends_with(".bsa") || (config.enable_ba2 && key.ends_with(".ba2")))
                && if config.mo2.is_some() {
                    mo2_archive_is_active(path, &plugins)
                        && non_mo2_archive_is_active(path, &archive_owners)
                } else {
                    non_mo2_archive_is_active(path, &archive_owners)
                }
        })
        .map(|(_, path)| path.clone())
        .collect();
    sort_archives_by_load_order(&mut archives, &plugins);
    let archive_keys: BTreeMap<_, _> = resolved
        .iter()
        .map(|(key, path)| (path.clone(), key.clone()))
        .collect();
    let mut targets: BTreeSet<String> = manifest
        .failures
        .keys()
        .filter(|key| kind(key).is_some())
        .cloned()
        .collect();
    let mut sources = BTreeMap::<String, Source>::new();
    let cache = config.ingestion_cache_dir().join(".ingestion-cache");
    let new_cache = directory.join(".ingestion-cache");
    let mut repaired_archives = BTreeSet::new();
    for archive in archives {
        let archive_key = &archive_keys[&archive];
        let failure_key = manifest
            .failures
            .keys()
            .find(|key| {
                key.replace('\\', "/")
                    .eq_ignore_ascii_case(&archive.to_string_lossy().replace('\\', "/"))
            })
            .cloned();
        let fresh = failure_key.is_some();
        if let Some(failure_key) = failure_key {
            eprintln!("Repair archive: {}", archive.display());
            match ArchiveExtractor::extract_cached(
                &archive,
                &directory.join("extracted").join(archive_key),
                &cache,
                &new_cache,
                None,
                true,
                None,
                None,
            ) {
                Ok(outcome) => {
                    for file in &outcome.cache_entry.files {
                        if kind(&file.path).is_some() {
                            targets.insert(file.path.clone());
                        }
                    }
                    manifest
                        .archives
                        .insert(archive_key.clone(), outcome.cache_entry);
                    manifest.failures.remove(&failure_key);
                    repaired_archives.insert(archive_key.clone());
                }
                Err(error) => {
                    manifest.failures.insert(failure_key, format!("{error:#}"));
                    continue;
                }
            }
        }
        let Some(entry) = manifest.archives.get(archive_key) else {
            continue;
        };
        let mut magic = [0u8; 4];
        fs::File::open(&archive)
            .wrap_err_with(|| format!("cannot read archive header {}", archive.display()))?
            .read_exact(&mut magic)
            .wrap_err_with(|| format!("cannot read archive header {}", archive.display()))?;
        let current_reader_version = archive_reader_version(&magic);
        if entry.reader_version != current_reader_version {
            fs::remove_dir_all(&directory)
                .wrap_err("cannot discard repair staging for a stale archive reader entry")?;
            bail!(
                "archive {} was ingested with reader version {} but current archive requires {}; run a normal conversion before repair",
                archive.display(),
                entry.reader_version,
                current_reader_version
            );
        }
        for file in &entry.files {
            if kind(&file.path).is_none() {
                continue;
            }
            let hash = &file.hash;
            ensure!(
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid ingestion hash"
            );
            let blob = (if fresh { &new_cache } else { &cache })
                .join("sha256")
                .join(&hash[..2])
                .join(hash);
            sources.insert(
                file.path.clone(),
                Source {
                    path: blob,
                    archive: Some((archive.clone(), entry.source_hash.clone())),
                    expected_hash: Some(hash.clone()),
                },
            );
        }
    }
    // Loose files override archives, just as in the main conversion pipeline.
    for (relative, path) in &resolved {
        if let Some((asset_kind, ext, _)) = kind(relative) {
            let key = canonical_asset_path(relative, asset_kind, ext)?;
            ensure!(
                !sources
                    .get(&key)
                    .is_some_and(|source| source.archive.is_none()),
                "normalized loose-source collision: {key}"
            );
            sources.insert(
                key,
                Source {
                    path: path.clone(),
                    archive: None,
                    expected_hash: None,
                },
            );
        }
    }
    // Keys stay logical so the resolver returns a source identity, not a
    // content-addressed blob two archives may share; values are readable paths
    // so the skeleton index can parse them.
    let mesh_sources = sources
        .iter()
        .map(|(key, source)| (PathBuf::from(key), source.path.clone()))
        .collect();
    // Parsing every skeleton is costly, so it happens at most once per run and
    // only if a mesh needs repair.
    let skeletons = OnceCell::new();
    for extension in ["nif", "dds", "pex"] {
        let count = sources
            .keys()
            .filter(|key| {
                !is_authoring_resource(key)
                    && Path::new(key)
                        .extension()
                        .is_some_and(|ext| ext == extension)
            })
            .count();
        manifest
            .inputs_by_kind
            .insert(extension.into(), count as u64);
    }
    let gpu = match config.texture_encoder {
        TextureEncoder::Gpu { quality, batch_mb } => match GpuUastc::new(quality, batch_mb) {
            Ok(mut gpu) => {
                gpu.zstd_level = config.texture_zstd_level;
                Some(gpu)
            }
            Err(error) => {
                eprintln!("GPU texture encoder unavailable ({error:#}); using CPU encoder");
                None
            }
        },
        TextureEncoder::Cpu => None,
    };
    let mut verified_archives = BTreeSet::new();
    let mut changed = BTreeSet::<String>::new();
    eprintln!(
        "Resolved {} runtime sources; inspecting published texture semantics",
        sources.len()
    );
    let source_textures = sources
        .keys()
        .filter(|key| key.starts_with("textures/") && key.ends_with(".dds"))
        .cloned()
        .collect();
    let mut semantics = collect_texture_semantics(&published, &source_textures)?;
    for key in targets
        .clone()
        .into_iter()
        .filter(|key| !key.ends_with(".dds"))
    {
        if is_authoring_resource(&key) {
            manifest.failures.remove(&key);
            manifest.excluded_inputs.insert(
                key,
                "BodySlide/Outfit Studio authoring resource, not runtime data".into(),
            );
            report.excluded += 1;
            continue;
        }
        attempt(
            config,
            gpu.as_ref(),
            &staged,
            &key,
            &sources,
            &mesh_sources,
            &skeletons,
            &mut verified_archives,
            &semantics,
            &mut manifest,
            &mut changed,
            &mut report,
        );
    }
    // Alias discovery and pruning need retained as well as repaired GLBs. Copy
    // mutable artifacts so a preview cannot rewrite the published pack through links.
    for entry in WalkDir::new(&published).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(&published)?;
            let destination = checked_destination(&staged, &relative.to_string_lossy())?;
            if !destination.exists() {
                fs::create_dir_all(destination.parent().unwrap())?;
                if relative
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("glb"))
                    || matches!(
                        relative.to_str(),
                        Some(
                            "skyrim_world.db"
                                | "integration-report.json"
                                | "conversion-manifest.json"
                        )
                    )
                {
                    fs::copy(entry.path(), &destination)?;
                } else {
                    link_or_copy(entry.path(), &destination)?;
                }
            }
        }
    }
    for (key, values) in collect_texture_semantics(&staged, &source_textures)? {
        semantics.entry(key).or_default().extend(values);
    }
    // Stage existing texture dependencies by link, and repair missing dependencies
    // from their winning DDS sources. Never prune a texture that actually exists in Data.
    let mut meshes = Vec::new();
    for entry in WalkDir::new(&staged).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("glb"))
        {
            meshes.push(
                entry
                    .path()
                    .strip_prefix(&staged)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    meshes.sort();
    for mesh in &meshes {
        for dependency in MeshConverter::glb_texture_dependencies(&staged.join(mesh))? {
            let destination = resolve_asset_uri(&staged, &staged.join(mesh), &dependency.uri)?;
            let relative = destination
                .strip_prefix(&staged)?
                .to_string_lossy()
                .replace('\\', "/");
            let base = source_texture_key(&relative)?;
            let old = checked_destination(&config.output_dir, &base)?;
            let encoding = TextureEncoding::from_semantics(
                &semantics.get(&base).cloned().unwrap_or_default(),
            )?;
            let compatible =
                old.is_file() && crate::texture::inspect_ktx2(&fs::read(&old)?, encoding).is_ok();
            if compatible {
                let to = checked_destination(&staged, &base)?;
                if !to.exists() {
                    fs::create_dir_all(to.parent().unwrap())?;
                    link_or_copy(&old, &to)?;
                }
            } else {
                let source = base
                    .strip_suffix(".ktx2")
                    .map(|stem| format!("{stem}.dds"))
                    .unwrap();
                if sources.contains_key(&source) {
                    targets.insert(source);
                } else {
                    ensure!(
                        !old.is_file(),
                        "texture {base} needs a new encoding, but its DDS source is unavailable"
                    );
                }
            }
        }
    }
    for key in targets.into_iter().filter(|key| key.ends_with(".dds")) {
        attempt(
            config,
            gpu.as_ref(),
            &staged,
            &key,
            &sources,
            &mesh_sources,
            &skeletons,
            &mut verified_archives,
            &semantics,
            &mut manifest,
            &mut changed,
            &mut report,
        );
    }
    let aliases = publish_srgb_texture_aliases(&staged)?;
    for alias in aliases {
        let key = alias.to_string_lossy().replace('\\', "/");
        let base = source_texture_key(&key)?;
        // Publish new aliases or refresh an alias whose underlying texture was repaired.
        if changed.contains(&base) || !config.output_dir.join(&key).is_file() {
            changed.insert(key.clone());
            record_output(
                &mut manifest,
                format!("alias:{key}"),
                key.clone(),
                hash_file(&staged.join(&base))?,
                &staged,
            )?;
        }
    }

    for file in MeshConverter::prune_dangling_texture_uris_with_sources(&staged, &source_textures)?
    {
        if !file.removed_uris.is_empty() {
            changed.insert(file.glb.clone());
            let references = manifest
                .pruned_texture_references
                .entry(file.glb.clone())
                .or_default();
            for uri in file.removed_uris {
                let resolved = resolve_asset_uri(&staged, &staged.join(&file.glb), &uri)?;
                references.insert(
                    resolved
                        .strip_prefix(&staged)?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    for mesh in &meshes {
        for dependency in MeshConverter::glb_texture_dependencies(&staged.join(mesh))? {
            let target = resolve_asset_uri(&staged, &staged.join(mesh), &dependency.uri)?;
            // A failed DDS remains in the failure list, not a silent material prune.
            if !target.is_file() {
                let relative = target
                    .strip_prefix(&staged)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let base = source_texture_key(&relative)?;
                let key = format!("{}.dds", base.strip_suffix(".ktx2").unwrap());
                ensure!(
                    manifest.failures.contains_key(&key),
                    "unresolved texture {} in final {mesh}",
                    dependency.uri
                );
            }
        }
    }
    let mut lua = None;
    for relative in &changed {
        validate_artifact(&staged, Path::new(relative), &semantics, &mut lua)?;
    }
    for entry in manifest
        .entries
        .values_mut()
        .filter(|entry| changed.contains(&entry.output))
    {
        entry.output_size = fs::metadata(staged.join(&entry.output))?.len();
        entry.output_hash = hash_file(&staged.join(&entry.output))?;
    }

    let integration_sources = sources
        .iter()
        .map(|(key, source)| (key.clone(), source.path.clone()))
        .collect();
    if let Some(integration) = finalize_world_database_with_sources(&staged, &integration_sources)?
    {
        ensure!(
            !apply || integration.passed,
            "repaired pack failed asset integration"
        );
        changed.insert("skyrim_world.db".into());
        changed.insert("integration-report.json".into());
        for entry in manifest
            .entries
            .values_mut()
            .filter(|entry| changed.contains(&entry.output))
        {
            entry.output_size = fs::metadata(staged.join(&entry.output))?.len();
            entry.output_hash = hash_file(&staged.join(&entry.output))?;
        }
    }
    report.failures = manifest.failures.clone();
    manifest.complete = false;
    report
        .warnings
        .extend(crate::extensions::refresh_manifest_with_notices(
            &mut manifest,
            &staged,
        )?);
    manifest.save(&staged.join("conversion-manifest.json"))?;
    fs::write(
        directory.join("repair-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if apply {
        ensure!(
            manifest.failures.is_empty(),
            "repair still has {} failed inputs; original pack unchanged, see {}",
            manifest.failures.len(),
            directory.display()
        );
        ensure!(
            fs::read(&manifest_path)? == original_bytes,
            "published manifest changed while repair ran"
        );
        ensure!(
            configuration_hash(config)? == manifest.configuration_hash,
            "MO2 configuration changed while repair ran"
        );
        check_existing(&config.output_dir, CheckMode::Full)?;
        // New ingestion blobs are content-addressed; persisting them does not alter
        // any existing blob or runtime artifact.
        for archive_key in repaired_archives {
            for file in &manifest.archives[&archive_key].files {
                let relative = PathBuf::from("sha256")
                    .join(&file.hash[..2])
                    .join(&file.hash);
                let destination = cache.join(&relative);
                if !destination.exists() {
                    fs::create_dir_all(destination.parent().unwrap())?;
                    link_or_copy(&new_cache.join(relative), &destination)?;
                }
            }
        }
        // Asset repair leaves record data and its exact known/unknown proof untouched.
        manifest.complete = report.record_data_complete;
        manifest.save(&staged.join("conversion-manifest.json"))?;
        changed.insert("conversion-manifest.json".into());
        let check = check_output(&staged, CheckMode::Full, |_, _| {})?;
        ensure!(
            check.problems.iter().all(|problem| {
                !manifest.complete && matches!(problem, CheckProblem::Incomplete)
            }),
            "repaired pack failed full validation: {:?}",
            check.problems
        );
        publish(&output, &staged, &directory.join("backup"), &changed)?;
        report.published = true;
        fs::write(
            directory.join("repair-report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
    }
    Ok(report)
}

/// Attempts one staged repair, updating manifest entries and counts or recording the failure.
#[allow(clippy::too_many_arguments)]
fn attempt(
    config: &PipelineConfig,
    gpu: Option<&GpuUastc>,
    staged: &Path,
    key: &str,
    sources: &BTreeMap<String, Source>,
    mesh_sources: &BTreeMap<PathBuf, PathBuf>,
    skeletons: &OnceCell<crate::mesh::SkeletonIndex>,
    verified: &mut BTreeSet<PathBuf>,
    semantics: &BTreeMap<String, BTreeSet<crate::texture::TextureSemantic>>,
    manifest: &mut ConversionManifest,
    changed: &mut BTreeSet<String>,
    report: &mut RepairReport,
) {
    eprintln!("Repair input: {key}");
    let result = (|| -> Result<()> {
        let source = sources
            .get(key)
            .ok_or_else(|| color_eyre::eyre::eyre!("winning source is missing: {key}"))?;
        let mut source_hash = verified_source_hash(source, key, verified)?;
        let (asset_kind, ext, target_ext) = kind(key).unwrap();
        let output = canonical_asset_path(key, asset_kind, target_ext)?;
        let target = checked_destination(staged, &output)?;
        if ext == "nif" {
            let skeletons =
                skeletons.get_or_init(|| crate::mesh::SkeletonIndex::from_sources(mesh_sources));
            let skeleton_key =
                crate::mesh::resolve_skeleton_with_index(Path::new(key), mesh_sources, skeletons);
            let mut unresolved_skin = None;
            let skeleton = match skeleton_key {
                crate::mesh::SkeletonResolution::Skeleton(path) => {
                    // The resolver returns the normalized key, while archive
                    // keys keep their original case.
                    let (dependency_key, dependency_source) = sources
                        .get_key_value(path.to_string_lossy().as_ref())
                        .or_else(|| {
                            sources.iter().find(|(candidate, _)| {
                                crate::mesh::normalize_path_key(Path::new(candidate.as_str()))
                                    == crate::mesh::normalize_path_key(&path)
                            })
                        })
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!(
                                "resolved skeleton {} has no winning source",
                                path.display()
                            )
                        })?;
                    source_hash.push(':');
                    source_hash.push_str(&verified_source_hash(
                        dependency_source,
                        dependency_key,
                        verified,
                    )?);
                    Some(dependency_source.path.as_path())
                }
                crate::mesh::SkeletonResolution::SelfContained => None,
                crate::mesh::SkeletonResolution::Unresolved { best_coverage } => {
                    source_hash.push_str(crate::mesh::UNRESOLVED_SKIN_HASH_SUFFIX);
                    unresolved_skin = Some(crate::mesh::unresolved_skeleton_diagnostic(
                        format_args!("NIF {key}"),
                        best_coverage,
                    ));
                    None
                }
            };
            MeshConverter::convert_nif_to_glb_with_skin_resolution(
                &source.path,
                &target,
                skeleton,
                unresolved_skin.as_deref(),
            )?;
            MeshConverter::glb_texture_dependencies(&target)?;
            if MeshConverter::is_geometry_template(&source.path)? {
                manifest.excluded_inputs.insert(key.into(), "Geometryless overlay template; standalone mesh is empty, runtime overlays are not implemented".into());
                report.excluded += 1;
            } else {
                MeshConverter::glb_bounds(&target)?;
            }
        } else if ext == "pex" {
            ScriptConverter::convert_pex_to_luau(&source.path, &target)?;
        } else {
            let encoding = TextureEncoding::from_semantics(
                &semantics.get(&output).cloned().unwrap_or_default(),
            )?;
            source_hash.push_str(&format!(":texture-encoding:{encoding:?}"));
            if let Some(label) = convert_texture(config, gpu, &source.path, &target, encoding)? {
                source_hash.push_str(&label);
            }
        }
        record_output(manifest, key.into(), output.clone(), source_hash, staged)?;
        changed.insert(output);
        manifest.failures.remove(key);
        report.converted += 1;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Repair failed: {key}: {error:#}");
        manifest.failures.insert(key.into(), format!("{error:#}"));
    }
}

/// Uses the normal GPU worker and labels only successfully published GPU output.
fn convert_texture(
    config: &PipelineConfig,
    gpu: Option<&GpuUastc>,
    source: &Path,
    target: &Path,
    encoding: TextureEncoding,
) -> Result<Option<String>> {
    if let Some(gpu) = gpu
        && let Ok(bytes) = fs::read(source)
        && let Ok(texture) = PreparedTexture::from_dds(bytes, encoding)
    {
        let (sender, receiver) = texture_gpu::job_channel(gpu);
        let queued = sender
            .send(GpuJob {
                texture,
                encoding,
                tag: (),
            })
            .is_ok();
        drop(sender);
        if queued {
            let result = std::sync::Mutex::new(None);
            texture_gpu::run_batcher(
                gpu,
                receiver,
                config.cpu_jobs,
                || false,
                |(), encoded| {
                    *result.lock().unwrap() =
                        Some(encoded.and_then(|encoded| publish_ktx2_file(target, &encoded.bytes)));
                },
            );
            match result.into_inner().unwrap() {
                Some(Ok(())) => return Ok(Some(texture_gpu::cache_label(gpu.quality))),
                failure => {
                    eprintln!(
                        "GPU texture conversion failed for {} ({failure:?}); using CPU encoder",
                        source.display()
                    );
                    if target.exists() {
                        fs::remove_file(target)?;
                    }
                }
            }
        }
    }
    TextureConverter::convert_dds_to_ktx2_with_options(
        source,
        target,
        encoding,
        config.texture_fallback_quality,
        config.texture_uastc_level,
        config.texture_zstd_level,
    )?;
    Ok(None)
}

/// Verifies both repaired inputs and their winning archive-backed dependencies.
fn verified_source_hash(
    source: &Source,
    key: &str,
    verified: &mut BTreeSet<PathBuf>,
) -> Result<String> {
    if let Some((archive, expected)) = &source.archive
        && !verified.contains(archive)
    {
        ensure!(
            hash_file(archive)? == *expected,
            "archive changed since conversion: {}",
            archive.display()
        );
        verified.insert(archive.clone());
    }
    let hash = hash_file(&source.path)?;
    if let Some(expected) = &source.expected_hash {
        ensure!(hash == *expected, "ingestion blob hash mismatch for {key}");
    }
    Ok(hash)
}

/// Records a staged output with its source hash, output hash, and size in the manifest.
fn record_output(
    manifest: &mut ConversionManifest,
    key: String,
    output: String,
    source_hash: String,
    root: &Path,
) -> Result<()> {
    let path = checked_destination(root, &output)?;
    manifest.entries.insert(
        key,
        CacheEntry {
            source_hash,
            output,
            output_size: fs::metadata(&path)?.len(),
            output_hash: hash_file(&path)?,
        },
    );
    Ok(())
}

/// Checks a published pack, allowing only incomplete-conversion and recorded-input-failure problems.
fn check_existing(output: &Path, mode: CheckMode) -> Result<()> {
    let check = check_output(output, mode, |_, _| {})?;
    for problem in check.problems {
        ensure!(
            matches!(
                problem,
                CheckProblem::Incomplete | CheckProblem::RecordedFailures { .. }
            ),
            "existing pack cannot be repaired safely: {problem}"
        );
    }
    Ok(())
}

/// Validates a relative repair path and ensures its nearest existing ancestor remains inside the root.
fn checked_destination(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = safe_relative_path(relative)?;
    let path = root.join(relative);
    let canonical_root = fs::canonicalize(root)?;
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid repair path"))?;
    }
    ensure!(
        fs::canonicalize(ancestor)?.starts_with(canonical_root),
        "repair path escapes output: {}",
        path.display()
    );
    Ok(path)
}

fn sync_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

fn sibling_path(output: &Path, suffix: &str) -> PathBuf {
    output.with_file_name(format!(
        "{}.{}",
        output.file_name().unwrap().to_string_lossy(),
        suffix
    ))
}

#[derive(Serialize, Deserialize)]
struct PublicationJournal {
    backup: PathBuf,
    paths: Vec<(String, bool)>,
}

/// Restore every path, retaining backups and the journal until all restores succeed.
/// Copying backups makes recovery repeatable even if recovery itself is interrupted.
fn recover_publication(output: &Path) -> Result<()> {
    let path = sibling_path(output, "repair-journal.json");
    if !path.exists() {
        return Ok(());
    }
    let journal: PublicationJournal = serde_json::from_slice(&fs::read(&path)?)?;
    let mut errors = Vec::new();
    for (relative, existed) in journal.paths.iter().rev() {
        let result = (|| -> Result<()> {
            let destination = checked_destination(output, relative)?;
            if *existed {
                let old = checked_destination(&journal.backup, relative)?;
                fs::copy(old, &destination)?;
                sync_file(&destination)?;
            } else if destination.exists() {
                fs::remove_file(destination)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(format!("{relative}: {error:#}"));
        }
    }
    ensure!(
        errors.is_empty(),
        "repair rollback failed (journal retained): {}",
        errors.join("; ")
    );
    fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_PUBLICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Write and sync all backups and rollback intents before changing any published path.
/// The caller holds the output lock; an interrupted transaction is recovered on next repair.
fn publish(output: &Path, staged: &Path, backup: &Path, changed: &BTreeSet<String>) -> Result<()> {
    fs::create_dir(backup)?;
    let mut paths: Vec<_> = changed
        .iter()
        .filter(|path| path.as_str() != "conversion-manifest.json")
        .cloned()
        .collect();
    paths.push("conversion-manifest.json".into());
    let mut journal = PublicationJournal {
        backup: fs::canonicalize(backup)?,
        paths: Vec::new(),
    };
    for relative in paths {
        let destination = checked_destination(output, &relative)?;
        let source = checked_destination(staged, &relative)?;
        ensure!(source.is_file(), "missing staged repair: {relative}");
        sync_file(&source)?;
        let existed = destination.exists();
        if existed {
            let old = checked_destination(backup, &relative)?;
            fs::create_dir_all(old.parent().unwrap())?;
            fs::copy(&destination, &old)?;
            sync_file(&old)?;
        }
        journal.paths.push((relative, existed));
    }
    let journal_path = sibling_path(output, "repair-journal.json");
    ensure!(!journal_path.exists(), "unrecovered repair journal exists");
    let temporary = sibling_path(output, "repair-journal.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&journal)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, &journal_path)?;
    let result = (|| -> Result<()> {
        for (relative, _) in &journal.paths {
            let destination = checked_destination(output, relative)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(checked_destination(staged, relative)?, destination)?;
            #[cfg(test)]
            ensure!(
                !FAIL_PUBLICATION.with(std::cell::Cell::get),
                "injected publication failure"
            );
        }
        fs::remove_file(&journal_path)?;
        Ok(())
    })();
    if let Err(error) = result {
        return match recover_publication(output) {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.wrap_err(format!("also failed to roll back: {rollback:#}"))),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texture_source(root: &Path, format: ddsfile::D3DFormat) -> PathBuf {
        let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
            height: 4,
            width: 4,
            depth: None,
            format,
            mipmap_levels: Some(1),
            caps2: None,
        })
        .unwrap();
        dds.data.fill(127);
        let source = root.join(format!("{format:?}.dds"));
        dds.write(&mut fs::File::create(&source).unwrap()).unwrap();
        source
    }

    #[test]
    fn unavailable_gpu_repair_uses_cpu_options_without_gpu_label() {
        let root = tempfile::tempdir().unwrap();
        let source = texture_source(root.path(), ddsfile::D3DFormat::A8R8G8B8);
        let mut config = PipelineConfig::new(root.path(), root.path());
        config.texture_encoder = TextureEncoder::Gpu {
            quality: 3,
            batch_mb: 1,
        };
        config.texture_uastc_level = 0;
        config.texture_zstd_level = 0;
        let repaired = root.path().join("repaired.ktx2");
        let normal = root.path().join("normal.ktx2");
        assert_eq!(
            convert_texture(
                &config,
                None,
                &source,
                &repaired,
                TextureEncoding::ColorSrgb
            )
            .unwrap(),
            None
        );
        TextureConverter::convert_dds_to_ktx2_with_options(
            &source,
            &normal,
            TextureEncoding::ColorSrgb,
            config.texture_fallback_quality,
            config.texture_uastc_level,
            config.texture_zstd_level,
        )
        .unwrap();
        assert_eq!(fs::read(repaired).unwrap(), fs::read(normal).unwrap());
    }

    #[test]
    fn hardware_repair_labels_gpu_output_but_not_native_blocks() {
        let root = tempfile::tempdir().unwrap();
        let mut gpu = match GpuUastc::new(3, 1) {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("SKIP hardware repair encoding: {error:#}");
                return;
            }
        };
        gpu.zstd_level = 6;
        let config = PipelineConfig::new(root.path(), root.path());
        for (format, expected) in [
            (
                ddsfile::D3DFormat::A8R8G8B8,
                Some(texture_gpu::cache_label(3)),
            ),
            (ddsfile::D3DFormat::DXT1, None),
        ] {
            let source = texture_source(root.path(), format);
            let target = source.with_extension("ktx2");
            assert_eq!(
                convert_texture(
                    &config,
                    Some(&gpu),
                    &source,
                    &target,
                    TextureEncoding::ColorSrgb
                )
                .unwrap(),
                expected
            );
            crate::texture::inspect_ktx2(&fs::read(target).unwrap(), TextureEncoding::ColorSrgb)
                .unwrap();
        }
    }

    /// Verifies failed publication restores original files and repair paths cannot traverse outside the pack.
    #[test]
    fn publication_rolls_back_and_rejects_escaping_paths() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let staged = root.path().join("staged");
        fs::create_dir(&output).unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(output.join("a.luau"), "original").unwrap();
        fs::write(output.join("conversion-manifest.json"), "original manifest").unwrap();
        fs::write(staged.join("a.luau"), "repaired").unwrap();
        let changed = BTreeSet::from(["a.luau".into(), "missing.luau".into()]);
        assert!(publish(&output, &staged, &root.path().join("backup"), &changed).is_err());
        assert_eq!(
            fs::read_to_string(output.join("a.luau")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(output.join("conversion-manifest.json")).unwrap(),
            "original manifest"
        );
        assert!(checked_destination(&output, "../outside").is_err());
    }

    #[test]
    fn failed_publication_preserves_error_and_restores_pack() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let staged = root.path().join("staged");
        fs::create_dir(&output).unwrap();
        fs::create_dir(&staged).unwrap();
        for relative in ["a.luau", "conversion-manifest.json"] {
            fs::write(output.join(relative), "original").unwrap();
            fs::write(staged.join(relative), "repaired").unwrap();
        }
        FAIL_PUBLICATION.with(|flag| flag.set(true));
        let error = publish(
            &output,
            &staged,
            &root.path().join("backup"),
            &BTreeSet::from(["a.luau".into()]),
        )
        .unwrap_err();
        FAIL_PUBLICATION.with(|flag| flag.set(false));
        assert!(format!("{error:#}").contains("injected publication failure"));
        assert_eq!(
            fs::read_to_string(output.join("a.luau")).unwrap(),
            "original"
        );
        assert!(!sibling_path(&output, "repair-journal.json").exists());
    }

    #[test]
    fn recovery_attempts_every_restore_and_is_repeatable() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let backup = root.path().join("backup");
        fs::create_dir(&output).unwrap();
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("a"), "original").unwrap();
        fs::write(output.join("a"), "repaired").unwrap();
        fs::write(output.join("new"), "new").unwrap();
        let journal = PublicationJournal {
            backup: backup.clone(),
            paths: vec![
                ("a".into(), true),
                ("new".into(), false),
                ("missing-backup".into(), true),
            ],
        };
        let path = sibling_path(&output, "repair-journal.json");
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(recover_publication(&output).is_err());
        assert_eq!(fs::read_to_string(output.join("a")).unwrap(), "original");
        assert!(!output.join("new").exists());
        assert!(path.exists());
        fs::write(backup.join("missing-backup"), "restored").unwrap();
        recover_publication(&output).unwrap();
        recover_publication(&output).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn ownership_identity_survives_missing_and_renamed_output() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("new-parent/output");
        let owner = OutputOwnership::acquire(&output).unwrap();
        assert!(!output.exists());
        assert!(OutputOwnership::acquire(&output).is_err());
        fs::create_dir(&output).unwrap();
        fs::rename(&output, output.with_file_name("backup")).unwrap();
        let alternate = output.parent().unwrap().join(".").join("output");
        assert!(OutputOwnership::acquire(&alternate).is_err());
        drop(owner);
        OutputOwnership::acquire(&alternate).unwrap();
    }

    #[test]
    fn repair_hashes_and_verifies_winning_archive_or_loose_skeleton() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("Data");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::all(),
        )
        .unwrap();
        let mesh = data.join("meshes/generated.nif");
        // Cache blobs have no useful physical neighbors; resolution must use
        // logical archive paths rather than their content-addressed filenames.
        let blob = root.path().join("opaque-blob");
        fs::copy(&mesh, &blob).unwrap();
        let archive = root.path().join("source.bsa");
        fs::write(&archive, "archive provenance fixture").unwrap();
        let key = "meshes/actors/character/armor/body.nif";
        // Archive keys keep their authored case; the resolver answers with a
        // normalized key, so the lookup must not depend on matching spelling.
        let skeleton_key = "meshes/actors/character/Character Assets/skeleton.nif";
        let skeleton = Source {
            path: blob.clone(),
            archive: Some((archive.clone(), hash_file(&archive).unwrap())),
            expected_hash: Some(hash_file(&blob).unwrap()),
        };
        let mut sources = BTreeMap::from([
            (
                key.into(),
                Source {
                    path: mesh.clone(),
                    archive: None,
                    expected_hash: None,
                },
            ),
            (skeleton_key.into(), skeleton),
        ]);
        let config = PipelineConfig::new(&data, root.path().join("output"));
        for loose in [false, true] {
            if loose {
                sources.insert(
                    skeleton_key.into(),
                    Source {
                        path: mesh.clone(),
                        archive: None,
                        expected_hash: None,
                    },
                );
                // A losing archive must not be inspected after the loose overlay wins.
                fs::remove_file(&archive).unwrap();
            }
            let staged = root.path().join(if loose { "loose" } else { "archived" });
            fs::create_dir(&staged).unwrap();
            let mesh_sources = sources
                .iter()
                .map(|(key, source)| (PathBuf::from(key), source.path.clone()))
                .collect();
            let mut manifest = ConversionManifest::default();
            let mut report = RepairReport {
                directory: staged.clone(),
                converted: 0,
                excluded: 0,
                published: false,
                record_data_complete: false,
                failures: BTreeMap::new(),
                warnings: Vec::new(),
            };
            attempt(
                &config,
                None,
                &staged,
                key,
                &sources,
                &mesh_sources,
                &OnceCell::new(),
                &mut BTreeSet::new(),
                &BTreeMap::new(),
                &mut manifest,
                &mut BTreeSet::new(),
                &mut report,
            );
            assert!(manifest.failures.is_empty(), "{:?}", manifest.failures);
            assert_eq!(
                manifest.entries[key].source_hash,
                format!(
                    "{}:{}",
                    hash_file(&mesh).unwrap(),
                    hash_file(&sources[skeleton_key].path).unwrap()
                )
            );
            assert_eq!(report.converted, 1);
        }
    }

    #[test]
    fn archive_dependency_hash_rejects_modified_blob_and_archive() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("source.bsa");
        let blob = root.path().join("blob");
        fs::write(&archive, "archive").unwrap();
        fs::write(&blob, "skeleton").unwrap();
        let source = Source {
            path: blob.clone(),
            archive: Some((archive.clone(), hash_file(&archive).unwrap())),
            expected_hash: Some(hash_file(&blob).unwrap()),
        };
        fs::write(&blob, "modified skeleton").unwrap();
        assert!(
            verified_source_hash(&source, "skeleton", &mut BTreeSet::new())
                .unwrap_err()
                .to_string()
                .contains("blob hash mismatch")
        );
        fs::write(&archive, "modified archive").unwrap();
        assert!(
            verified_source_hash(&source, "skeleton", &mut BTreeSet::new())
                .unwrap_err()
                .to_string()
                .contains("archive changed")
        );
    }

    #[test]
    fn output_lock_excludes_another_handle_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repair.lock");
        let open = || {
            fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let first = open();
        first.try_lock().unwrap();
        let second = open();
        assert!(second.try_lock().is_err());
        drop(first);
        second.try_lock().unwrap();
    }

    /// Repairs an installed failed pack into a sibling directory and verifies its manifest stays unchanged.
    #[test]
    #[ignore = "requires MUDCRAB_REPAIR_DATA, OUTPUT, INSTANCE and PROFILE for an installed failed pack"]
    fn repairs_installed_profile_without_publishing() {
        let value = |name: &str| {
            std::env::var_os(format!("MUDCRAB_REPAIR_{name}"))
                .expect("set repair fixture environment")
        };
        let mut config = PipelineConfig::new(value("DATA"), value("OUTPUT"));
        config.mo2 = Some(mo2::Selection {
            instance_path: value("INSTANCE").into(),
            profile: value("PROFILE").to_string_lossy().into_owned(),
        });
        let before = fs::read(config.output_dir.join("conversion-manifest.json")).unwrap();
        let report = repair_failed(&config, false).unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert!(!report.published);
        assert_eq!(
            fs::read(config.output_dir.join("conversion-manifest.json")).unwrap(),
            before
        );
        assert!(
            report.failures.is_empty(),
            "unresolved real inputs: {:?}",
            report.failures
        );
    }
}
