mod ba2;
mod batched;
mod bsa;
mod bsa_name;

pub(crate) use batched::{prune_stale_reader_packs, sync_directory, sync_pack_file};

use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    cache::{
        IngestedFile, IngestionCacheEntry, IngestionSelection, SpillCache, hash_bytes, hash_file,
        link_or_copy, link_or_copy_spilling, link_or_copy_spilling_with_copy_and_link,
    },
    pipeline::Interrupted,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail},
};
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

/// How far an archive extraction has got: files and bytes done, against the archive's own totals
/// (its file table says how many entries it holds and how many bytes each one takes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractionProgress {
    pub completed_files: u64,
    pub completed_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
}

/// Called as an archive is extracted, and reused from its ingestion cache. Extraction runs on many
/// threads, so the callback has to be safe to call from any of them.
pub type ExtractionProgressCallback<'a> = &'a (dyn Fn(ExtractionProgress) + Send + Sync);

/// Asked before each entry of an archive is extracted or restored; `true` means the run was
/// stopped, and the archive is abandoned at that point without recording its cache entry.
/// Checked from the extraction threads, like [`ExtractionProgressCallback`].
pub type StopCheck<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// Returns the [`Interrupted`] error once `stop` says the run was stopped.
fn check_stop(stop: Option<StopCheck<'_>>) -> Result<()> {
    if stop.is_some_and(|stop| stop()) {
        return Err(Interrupted::new().into());
    }
    Ok(())
}

/// How often an extraction reports: every this many files, so a 173,000-file archive sends
/// hundreds of progress events rather than one per file.
const PROGRESS_FILE_STEP: u64 = 512;

/// Counts an archive's finished work and reports it at [`PROGRESS_FILE_STEP`] intervals.
struct ProgressReporter<'a> {
    callback: ExtractionProgressCallback<'a>,
    files: AtomicU64,
    bytes: AtomicU64,
    totals: ExtractionProgress,
    emission: Mutex<()>,
}

impl<'a> ProgressReporter<'a> {
    fn new(callback: ExtractionProgressCallback<'a>, total_files: u64, total_bytes: u64) -> Self {
        Self {
            callback,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            emission: Mutex::new(()),
            totals: ExtractionProgress {
                completed_files: 0,
                completed_bytes: 0,
                total_files,
                total_bytes,
            },
        }
    }

    /// Records the archive's totals before any of it is written, so the caller learns the
    /// denominator with its first event.
    fn announce(&self) {
        (self.callback)(self.totals);
    }

    fn advance(&self, bytes: u64) {
        let _emission = self.emission.lock().unwrap();
        let files = self.files.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if files.is_multiple_of(PROGRESS_FILE_STEP) || files == self.totals.total_files {
            (self.callback)(ExtractionProgress {
                completed_files: files,
                completed_bytes: bytes,
                ..self.totals
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveKind {
    Bsa,
    Ba2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFile {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub sha256: String,
}

/// Durability of disposable extracted inputs; published runtime assets keep their own contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestionSync {
    #[default]
    PerFile,
    Archive,
    None,
}

#[derive(Debug, Clone, Copy)]
pub struct IngestionOptions {
    pub selection: IngestionSelection,
    pub sync: IngestionSync,
    pub cpu_jobs: usize,
}

impl Default for IngestionOptions {
    fn default() -> Self {
        Self {
            selection: IngestionSelection::All,
            sync: IngestionSync::PerFile,
            cpu_jobs: std::thread::available_parallelism().map_or(1, usize::from),
        }
    }
}

/// Measured wall spans within archive ingestion; metadata parsing/setup is boundary overhead.
/// Per-file flushes occur inside extraction_seconds.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractionTimings {
    pub source_hash_seconds: f64,
    pub cache_restore_seconds: f64,
    pub extraction_seconds: f64,
    pub sync_seconds: f64,
    pub cache_link_seconds: f64,
    /// Summed worker flush time overlaps across threads and is not a wall-time phase.
    pub sync_worker_seconds: f64,
    pub sync_calls: u64,
    /// Pack flushes overlap extraction and are separate from derived blob flushes.
    pub checkpoint_sync_calls: u64,
    pub checkpoint_sync_worker_seconds: f64,
    pub selected_files: u64,
    pub selected_bytes: u64,
    pub unique_payloads: u64,
    /// New canonical SHA blobs, rather than VFS names or separate spill copies.
    pub payload_writes: u64,
    pub payload_bytes_written: u64,
    /// Fresh entries whose canonical blob was already materialized.
    pub payload_reuses: u64,
    pub physical_copy_writes: u64,
    pub physical_copy_bytes_written: u64,
}

/// Coordinates content-addressed raw writes across the serial archive overlay.
/// A new session verifies surviving staging bytes and does not inherit flush state.
pub struct RawIngestionSession {
    cache_root: PathBuf,
    blobs: Mutex<BTreeMap<String, Arc<Mutex<RawBlobState>>>>,
    next_archive: AtomicU64,
    spill_cache: SpillCache,
}

impl RawIngestionSession {
    pub fn new(cache_root: &Path) -> Self {
        Self {
            cache_root: cache_root.to_owned(),
            blobs: Mutex::new(BTreeMap::new()),
            next_archive: AtomicU64::new(1),
            spill_cache: SpillCache::default(),
        }
    }

    fn blob(&self, hash: &str) -> Arc<Mutex<RawBlobState>> {
        self.blobs
            .lock()
            .unwrap()
            .entry(hash.to_owned())
            .or_default()
            .clone()
    }
}

#[derive(Default)]
struct RawBlobState {
    size: Option<u64>,
    physical: BTreeMap<PathBuf, RawPhysicalFile>,
}

#[derive(Default)]
struct RawPhysicalFile {
    verified_archive: u64,
    durable: bool,
    identity: Option<RawFileIdentity>,
}

#[derive(PartialEq, Eq)]
struct RawFileIdentity {
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn raw_file_identity(path: &Path) -> Result<RawFileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    color_eyre::eyre::ensure!(metadata.is_file(), "raw payload is not a regular file");
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(RawFileIdentity {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn open_raw_payload_for_sync(path: &Path) -> std::io::Result<File> {
    // Windows FlushFileBuffers needs write access; Unix fsync permits a read-only descriptor.
    #[cfg(windows)]
    {
        File::options().read(true).write(true).open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

#[derive(Default)]
struct RawWriteMeasurements {
    unique_payloads: AtomicU64,
    payload_writes: AtomicU64,
    payload_bytes_written: AtomicU64,
    payload_reuses: AtomicU64,
    physical_copy_writes: AtomicU64,
    physical_copy_bytes_written: AtomicU64,
}

struct FreshIngestion<'a> {
    session: &'a RawIngestionSession,
    archive: u64,
    sync_mode: IngestionSync,
    sync: &'a SyncMeasurements,
    writes: RawWriteMeasurements,
    used_blobs: Mutex<Vec<Arc<Mutex<RawBlobState>>>>,
}

impl FreshIngestion<'_> {
    fn materialize(&self, destination: &Path, data: &[u8], hash: &str) -> Result<()> {
        self.materialize_with_link(destination, data, hash, |from, to| fs::hard_link(from, to))
    }

    fn materialize_with_link(
        &self,
        destination: &Path,
        data: &[u8],
        hash: &str,
        link: fn(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let blob = blob_path(&self.session.cache_root, hash)?;
        let blob_state = self.session.blob(hash);
        // A hash has one writer, including all of its spill names. Unrelated hashes proceed
        // concurrently in the bounded extraction pool.
        let mut state = blob_state.lock().unwrap();
        let size = data.len() as u64;
        color_eyre::eyre::ensure!(
            state.size.is_none_or(|previous| previous == size),
            "conflicting raw payload sizes for SHA-256 {hash}"
        );
        state.size = Some(size);
        let main = state.physical.entry(blob.clone()).or_default();
        let mut written = false;
        if main.verified_archive != self.archive {
            self.writes.unique_payloads.fetch_add(1, Ordering::Relaxed);
            self.used_blobs
                .lock()
                .unwrap()
                .push(Arc::clone(&blob_state));
            if cache_blob_matches(&blob, size, hash) {
                self.retain_durability(main, &blob)?;
            } else {
                write_raw_payload(&blob, data, self.sync_mode, self.sync)?;
                main.durable = self.sync_mode == IngestionSync::PerFile;
                main.identity = Some(raw_file_identity(&blob)?);
                self.writes.payload_writes.fetch_add(1, Ordering::Relaxed);
                self.writes
                    .payload_bytes_written
                    .fetch_add(size, Ordering::Relaxed);
                written = true;
            }
            main.verified_archive = self.archive;
        }
        if !written {
            self.writes.payload_reuses.fetch_add(1, Ordering::Relaxed);
        }
        self.flush_per_file(main, &blob)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        link_or_copy_spilling_with_copy_and_link(
            &blob,
            destination,
            &self.session.spill_cache,
            &link,
            |source, copy, spill| {
                self.materialize_copy(&mut state, source, copy, spill, size, hash)
                    .map_err(|error| std::io::Error::other(format!("{error:#}")))
            },
        )?;
        Ok(())
    }

    fn retain_durability(&self, file: &mut RawPhysicalFile, path: &Path) -> Result<()> {
        let identity = raw_file_identity(path)?;
        // On Unix an inode and device identify the physical file; a changed inode or mtime
        // invalidates the flush memo. Other platforms conservatively flush again per archive.
        file.durable &= cfg!(unix) && file.identity.as_ref() == Some(&identity);
        file.identity = Some(identity);
        Ok(())
    }

    fn materialize_copy(
        &self,
        state: &mut RawBlobState,
        source: &Path,
        destination: &Path,
        spill: bool,
        size: u64,
        hash: &str,
    ) -> Result<()> {
        let copy = state.physical.entry(destination.to_owned()).or_default();
        if spill && copy.verified_archive == self.archive {
            return self.flush_per_file(copy, destination);
        }
        if spill && cache_blob_matches(destination, size, hash) {
            self.retain_durability(copy, destination)?;
        } else {
            copy_raw_payload(source, destination, self.sync_mode, self.sync)?;
            copy.durable = self.sync_mode == IngestionSync::PerFile;
            copy.identity = Some(raw_file_identity(destination)?);
            self.writes
                .physical_copy_writes
                .fetch_add(1, Ordering::Relaxed);
            self.writes
                .physical_copy_bytes_written
                .fetch_add(size, Ordering::Relaxed);
        }
        copy.verified_archive = self.archive;
        self.flush_per_file(copy, destination)
    }

    fn flush_per_file(&self, file: &mut RawPhysicalFile, path: &Path) -> Result<()> {
        if self.sync_mode == IngestionSync::PerFile && !file.durable {
            self.sync.flush(&open_raw_payload_for_sync(path)?)?;
            file.durable = true;
        }
        Ok(())
    }

    #[cfg(test)]
    fn flush_archive(&self, stop: Option<StopCheck<'_>>) -> Result<()> {
        let blobs = self.used_blobs.lock().unwrap().clone();
        blobs.par_iter().try_for_each(|blob| -> Result<()> {
            let mut blob = blob.lock().unwrap();
            for (path, file) in &mut blob.physical {
                if file.verified_archive == self.archive && !file.durable {
                    check_stop(stop)?;
                    self.sync.flush(&open_raw_payload_for_sync(path)?)?;
                    file.durable = true;
                }
            }
            Ok(())
        })
    }

    fn record_timings(&self, timings: &mut ExtractionTimings) {
        timings.unique_payloads = self.writes.unique_payloads.load(Ordering::Relaxed);
        timings.payload_writes = self.writes.payload_writes.load(Ordering::Relaxed);
        timings.payload_bytes_written = self.writes.payload_bytes_written.load(Ordering::Relaxed);
        timings.payload_reuses = self.writes.payload_reuses.load(Ordering::Relaxed);
        timings.physical_copy_writes = self.writes.physical_copy_writes.load(Ordering::Relaxed);
        timings.physical_copy_bytes_written = self
            .writes
            .physical_copy_bytes_written
            .load(Ordering::Relaxed);
    }
}

#[derive(Default)]
struct SyncMeasurements {
    nanoseconds: AtomicU64,
    calls: AtomicU64,
}

impl SyncMeasurements {
    fn flush(&self, file: &File) -> Result<()> {
        let started = Instant::now();
        file.sync_all()?;
        self.nanoseconds.fetch_add(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Debug)]
pub struct ExtractionOutcome {
    pub files: Vec<ExtractedFile>,
    pub cache_entry: IngestionCacheEntry,
    pub cache_hit: bool,
    pub timings: ExtractionTimings,
}

/// Current reader version of BSA archives, stored in their ingestion cache entries. Version 1
/// decodes names as Windows-1252; entries from the earlier UTF-8 decoder hold wrong paths for any
/// non-ASCII name and must not be restored. Other formats stay at 0.
const BSA_READER_VERSION: u32 = 1;

/// Reader version for an archive's bytes, from its magic. Only BSA names depend on the version.
pub(super) fn archive_reader_version(archive: &[u8]) -> u32 {
    if archive.starts_with(b"BSA\0") {
        BSA_READER_VERSION
    } else {
        0
    }
}

pub struct ArchiveExtractor;

/// One versioned coverage contract for both cache manifests and sealed packs.
pub type ArchiveSelection = IngestionSelection;

impl IngestionSelection {
    pub fn recipe(self) -> &'static str {
        match self {
            Self::All => "all-v1",
            Self::ConverterInputsV1 => "converter-inputs-v4",
            Self::RuntimeEnglishV1 => "runtime-english-v4",
        }
    }

    fn includes(self, path: &Path) -> bool {
        selection_includes(self, path)
    }

    fn accepts_recipe(self, recipe: &str) -> bool {
        let proven = match recipe {
            "" | "all-v1" => Self::All,
            "converter-inputs-v4" => Self::ConverterInputsV1,
            "runtime-english-v4" => Self::RuntimeEnglishV1,
            _ => return false,
        };
        proven.can_satisfy(self)
    }
}

impl IngestionCacheEntry {
    pub(crate) fn covers(&self, requested: IngestionSelection) -> bool {
        self.selection.can_satisfy(requested)
            && if self.recipe.is_empty() {
                self.selection == IngestionSelection::All
            } else {
                requested.accepts_recipe(&self.recipe)
            }
    }
}

/// CPU decompression and bounded I/O scheduling are independent. Completed
/// batches are durable immutable packs; per-file cache blobs are derived copies.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub cpu_jobs: usize,
    pub io_jobs: usize,
    pub selection: ArchiveSelection,
    pub checkpoint_dir: Option<PathBuf>,
    pub reuse_cache: bool,
    pub sync: IngestionSync,
}

impl ExtractOptions {
    pub fn converter(cpu_jobs: usize, io_jobs: usize, checkpoint_dir: Option<PathBuf>) -> Self {
        Self {
            cpu_jobs,
            io_jobs,
            selection: ArchiveSelection::ConverterInputsV1,
            checkpoint_dir,
            reuse_cache: true,
            sync: IngestionSync::Archive,
        }
    }
}

impl ArchiveExtractor {
    /// Inventories canonical paths without decompressing any payload.
    pub fn list_paths(archive_path: &Path) -> Result<Vec<PathBuf>> {
        let file = File::open(archive_path)?;
        // SAFETY: this is a read-only mapping, held only while the archive is open.
        let bytes = unsafe { Mmap::map(&file) }?;
        let seen = std::cell::RefCell::new(BTreeMap::new());
        let paths = std::cell::RefCell::new(Vec::new());
        let record = |name: &str| -> Result<bool> {
            let path = safe_relative_path(name)?;
            detect_archive_collision(&mut seen.borrow_mut(), &path, name)?;
            paths.borrow_mut().push(path);
            Ok(false)
        };
        match bytes.get(..4) {
            Some(b"BSA\0") => {
                for entry in bsa::iter_raw_entries(&bytes)? {
                    record(&entry.name)?;
                }
            }
            Some(b"BTDX") => {
                ba2::read_entries_matching(&bytes, record)?;
            }
            _ => bail!("unsupported archive magic in {}", archive_path.display()),
        }
        let mut paths = paths.into_inner();
        paths.sort();
        Ok(paths)
    }

    /// Extracts only the selected inputs, reusing verified durable batches left
    /// by a previous run, including one interrupted before the archive completed.
    /// The source inode must stay immutable while its bytes are memory-mapped.
    /// A replaced or unreadable source path invalidates newly sealed checkpoints.
    /// Reused sealed-pack bytes are always hashed, so `_verify_integrity` (the
    /// `--no-verify-cache` setting) does not change this path.
    #[allow(clippy::too_many_arguments)]
    pub fn extract_batched(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        options: &ExtractOptions,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
    ) -> Result<ExtractionOutcome> {
        color_eyre::eyre::ensure!(
            options.sync != IngestionSync::None || verify_integrity,
            "ingestion_sync=none requires cache verification"
        );
        // Packed recipes materialize ordinary blobs without per-file flushes.
        // Every reused blob is checked even if legacy cache verification was disabled.
        batched::extract(
            archive_path,
            output_root,
            previous_cache_root,
            cache_root,
            previous,
            options,
            progress,
            stop,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn extract_batched_with_session(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        options: &ExtractOptions,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
        session: &RawIngestionSession,
    ) -> Result<ExtractionOutcome> {
        color_eyre::eyre::ensure!(
            options.sync != IngestionSync::None || verify_integrity,
            "ingestion_sync=none requires cache verification"
        );
        color_eyre::eyre::ensure!(
            session.cache_root == cache_root,
            "raw ingestion session belongs to a different cache root"
        );
        batched::extract_with_session(
            archive_path,
            output_root,
            previous_cache_root,
            cache_root,
            previous,
            options,
            progress,
            stop,
            None,
            session,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extract_cached(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
    ) -> Result<ExtractionOutcome> {
        Self::extract_cached_with_options(
            archive_path,
            output_root,
            previous_cache_root,
            cache_root,
            previous,
            verify_integrity,
            progress,
            stop,
            IngestionOptions::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extract_cached_with_options(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
        options: IngestionOptions,
    ) -> Result<ExtractionOutcome> {
        let session = RawIngestionSession::new(cache_root);
        Self::extract_cached_with_session(
            archive_path,
            output_root,
            previous_cache_root,
            cache_root,
            previous,
            verify_integrity,
            progress,
            stop,
            &session,
            options,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extract_cached_with_session(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
        session: &RawIngestionSession,
        options: IngestionOptions,
    ) -> Result<ExtractionOutcome> {
        color_eyre::eyre::ensure!(
            session.cache_root == cache_root,
            "raw ingestion session belongs to a different cache root"
        );
        color_eyre::eyre::ensure!(
            options.sync != IngestionSync::None || verify_integrity,
            "ingestion_sync=none requires cache verification"
        );
        let mut batched = ExtractOptions::converter(options.cpu_jobs, options.cpu_jobs, None);
        batched.selection = options.selection;
        batched.sync = options.sync;
        batched::extract_with_session(
            archive_path,
            output_root,
            previous_cache_root,
            cache_root,
            previous,
            &batched,
            progress,
            stop,
            None,
            session,
        )
    }

    pub fn extract(archive_path: &Path, output_root: &Path) -> Result<Vec<ExtractedFile>> {
        extract_reporting(archive_path, output_root, None, None)
    }

    pub(crate) fn extract_lod_settings(
        archive_path: &Path,
        output_root: &Path,
    ) -> Result<Vec<ExtractedFile>> {
        extract_selected_reporting(archive_path, output_root, is_lod_setting, None, None)
    }

    /// Extracts only the archive entries named in `paths`, writing them under `output_root`
    /// with their archive-relative layout preserved.
    ///
    /// Paths are matched against the same normalized form the extractor writes: lowercase,
    /// forward slashes, no leading separator (`textures/actors/character/male/malebody_1.dds`).
    /// Entries that are not present are simply absent from the result, so a caller can walk a
    /// set of archives and remove whatever each one supplied.
    pub fn extract_paths(
        archive_path: &Path,
        output_root: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<ExtractedFile>> {
        extract_selected_reporting(
            archive_path,
            output_root,
            |path| paths.contains(path),
            None,
            None,
        )
    }
}

/// Extracts every entry of an archive, reporting progress as the archive's file table allows, and
/// stopping before the next entry once `stop` says so.
fn extract_reporting(
    archive_path: &Path,
    output_root: &Path,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Vec<ExtractedFile>> {
    extract_selected_reporting(archive_path, output_root, |_| true, progress, stop)
}

fn extract_selected_reporting(
    archive_path: &Path,
    output_root: &Path,
    include: impl Fn(&Path) -> bool + Sync,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Vec<ExtractedFile>> {
    extract_selected_with_sync_reporting(
        archive_path,
        output_root,
        include,
        progress,
        stop,
        ExtractionWriteContext {
            sync_mode: IngestionSync::PerFile,
            sync: &SyncMeasurements::default(),
            ingestion: None,
        },
    )
}

struct ExtractionWriteContext<'a, 'session> {
    sync_mode: IngestionSync,
    sync: &'a SyncMeasurements,
    ingestion: Option<&'a FreshIngestion<'session>>,
}

fn extract_selected_with_sync_reporting(
    archive_path: &Path,
    output_root: &Path,
    include: impl Fn(&Path) -> bool + Sync,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
    writes: ExtractionWriteContext<'_, '_>,
) -> Result<Vec<ExtractedFile>> {
    let ExtractionWriteContext {
        sync_mode,
        sync,
        ingestion,
    } = writes;
    let file = File::open(archive_path)
        .wrap_err_with(|| format!("failed to open archive {}", archive_path.display()))?;
    // SAFETY: the file remains open and the mapping is read-only for the duration
    // of parsing. No process-local code mutates the archive while it is mapped.
    let bytes = unsafe { Mmap::map(&file) }
        .wrap_err_with(|| format!("failed to map archive {}", archive_path.display()))?;
    let progress_lock = Mutex::new(());

    match bytes.get(..4) {
        Some(b"BSA\0") => {
            let entries = bsa::iter_raw_entries(&bytes).wrap_err_with(|| {
                format!("failed to parse BSA archive {}", archive_path.display())
            })?;
            let mut seen = BTreeMap::new();
            let entries = entries
                .into_iter()
                .map(|entry| {
                    let relative = safe_relative_path(&entry.name)?;
                    detect_archive_collision(&mut seen, &relative, &entry.name)?;
                    Ok((entry, relative))
                })
                .collect::<Result<Vec<_>>>()?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|(_, relative)| include(relative))
                .collect();
            // A BSA's file records and payloads are both in the mapping, so the bytes it will
            // write are known before the first entry is decompressed.
            let reporter = progress.map(|callback| {
                ProgressReporter::new(
                    callback,
                    entries.len() as u64,
                    entries
                        .iter()
                        .map(|(entry, _)| entry.payload.len() as u64)
                        .sum(),
                )
            });
            if let Some(reporter) = &reporter {
                reporter.announce();
            }
            entries
                .into_par_iter()
                .map(|(entry, relative)| {
                    check_stop(stop)?;
                    let destination = output_root.join(&relative);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let stored_bytes = entry.payload.len() as u64;
                    let data = entry.decompress()?;
                    let bytes_written = data.len() as u64;
                    let sha256 = hash_bytes(&data);

                    write_extracted_payload(
                        &destination,
                        &data,
                        &sha256,
                        sync_mode,
                        sync,
                        ingestion,
                    )
                    .wrap_err_with(|| format!("failed to extract {}", destination.display()))?;
                    if let Some(reporter) = &reporter {
                        let _guard = progress_lock.lock().unwrap();
                        reporter.advance(stored_bytes);
                    }

                    Ok(ExtractedFile {
                        path: relative,
                        bytes_written,
                        sha256,
                    })
                })
                .collect()
        }
        Some(b"BTDX") => {
            let all_names = std::cell::RefCell::new(BTreeMap::new());
            let entries = ba2::read_entries_matching(&bytes, |name| {
                let relative = safe_relative_path(name)?;
                detect_archive_collision(&mut all_names.borrow_mut(), &relative, name)?;
                Ok(include(&relative))
            })?;
            let mut seen = BTreeMap::new();
            let entries = entries
                .into_iter()
                .map(|(name, data)| {
                    let relative = safe_relative_path(&name)?;
                    detect_archive_collision(&mut seen, &relative, &name)?;
                    Ok((name, data, relative))
                })
                .collect::<Result<Vec<_>>>()?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|(_, _, relative)| include(relative))
                .collect();
            let reporter = progress.map(|callback| {
                ProgressReporter::new(
                    callback,
                    entries.len() as u64,
                    entries.iter().map(|(_, data, _)| data.len() as u64).sum(),
                )
            });
            if let Some(reporter) = &reporter {
                reporter.announce();
            }
            entries
                .into_par_iter()
                .map(|(_name, data, relative)| {
                    check_stop(stop)?;
                    let destination = output_root.join(&relative);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let bytes_written = data.len() as u64;
                    let sha256 = hash_bytes(&data);

                    write_extracted_payload(
                        &destination,
                        &data,
                        &sha256,
                        sync_mode,
                        sync,
                        ingestion,
                    )
                    .wrap_err_with(|| format!("failed to extract {}", destination.display()))?;
                    if let Some(reporter) = &reporter {
                        let _guard = progress_lock.lock().unwrap();
                        reporter.advance(bytes_written);
                    }

                    Ok(ExtractedFile {
                        path: relative,
                        bytes_written,
                        sha256,
                    })
                })
                .collect()
        }
        _ => bail!("unsupported archive magic in {}", archive_path.display()),
    }
}

/// Keep authored Havok metadata beside its clips. RACE projects may live outside
/// actors/ (including Creation Club), and neither rig nor idle names are universal.
pub(crate) fn is_animation_input(path: &Path) -> bool {
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    lower.starts_with("meshes/") && lower.ends_with(".hkx")
}

fn selection_includes(selection: IngestionSelection, path: &Path) -> bool {
    if selection == IngestionSelection::All {
        return true;
    }
    if is_animation_input(path) {
        return true;
    }
    if selection == IngestionSelection::ConverterInputsV1 {
        return path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "dds" | "nif" | "pex" | "lod" | "strings" | "ilstrings" | "dlstrings"
                )
            });
    }
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let path = Path::new(&lower);
    let extension = path.extension().and_then(|extension| extension.to_str());
    if matches!(extension, Some("dds" | "nif" | "pex")) {
        return true;
    }
    if path.parent() == Some(Path::new("lodsettings")) && extension == Some("lod") {
        return true;
    }
    path.parent() == Some(Path::new("strings"))
        && matches!(extension, Some("strings" | "dlstrings" | "ilstrings"))
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.ends_with("_english"))
}

fn is_lod_setting(path: &Path) -> bool {
    path.starts_with("lodsettings") && path.extension().is_some_and(|extension| extension == "lod")
}

struct CachedRestoreBlob<'a> {
    previous: PathBuf,
    staged: PathBuf,
    representative: &'a IngestedFile,
    files: Vec<(usize, &'a IngestedFile, PathBuf)>,
}

#[cfg(test)]
fn restore_cached_files(
    entry: &IngestionCacheEntry,
    output_root: &Path,
    previous_cache_root: &Path,
    cache_root: &Path,
    verify_integrity: bool,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Option<Vec<ExtractedFile>>> {
    restore_cached_files_with_io_limit(
        entry,
        output_root,
        previous_cache_root,
        cache_root,
        verify_integrity,
        progress,
        stop,
        rayon::current_num_threads(),
    )
}

#[allow(clippy::too_many_arguments)]
fn restore_cached_files_with_io_limit(
    entry: &IngestionCacheEntry,
    output_root: &Path,
    previous_cache_root: &Path,
    cache_root: &Path,
    verify_integrity: bool,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
    io_jobs: usize,
) -> Result<Option<Vec<ExtractedFile>>> {
    let mut seen_paths = BTreeMap::new();
    let mut blobs = BTreeMap::<&str, CachedRestoreBlob<'_>>::new();
    for (index, file) in entry.files.iter().enumerate() {
        let relative = safe_relative_path(&file.path)?;
        detect_archive_collision(&mut seen_paths, &relative, &file.path)?;
        let blob = match blobs.entry(file.hash.as_str()) {
            std::collections::btree_map::Entry::Occupied(blob) => blob.into_mut(),
            std::collections::btree_map::Entry::Vacant(blob) => blob.insert(CachedRestoreBlob {
                previous: blob_path(previous_cache_root, &file.hash)?,
                staged: blob_path(cache_root, &file.hash)?,
                representative: file,
                files: Vec::new(),
            }),
        };
        if blob.representative.size != file.size {
            return Ok(None);
        }
        blob.files.push((index, file, relative));
    }
    let blobs: Vec<_> = blobs.into_values().collect();
    // Validate each unique payload before writing any restored file. The caller installs this
    // work in the same bounded pool used for fresh extraction.
    let valid = blobs
        .par_iter()
        .map(|blob| -> Result<bool> {
            check_stop(stop)?;
            Ok(fs::symlink_metadata(&blob.previous).is_ok_and(|metadata| {
                metadata.is_file() && metadata.len() == blob.representative.size
            }) && (!verify_integrity
                || hash_file(&blob.previous).is_ok_and(|hash| hash == blob.representative.hash)))
        })
        .collect::<Result<Vec<_>>>()?;
    if valid.iter().any(|valid| !valid) {
        return Ok(None);
    }

    let reporter = progress.map(|callback| {
        ProgressReporter::new(
            callback,
            entry.files.len() as u64,
            entry.files.iter().map(|file| file.size).sum(),
        )
    });
    if let Some(reporter) = &reporter {
        reporter.announce();
    }
    // Only one worker owns a hash, including its spill names. Different VFS aliases of the same
    // payload stay sequential, avoiding shared spill temporary-file races on link-limited disks.
    let progress_lock = Mutex::new(());
    let spill_cache = SpillCache::default();
    let io_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(io_jobs)
        .build()?;
    let restored = io_pool.install(|| {
        blobs
            .into_par_iter()
            .map(|blob| -> Result<Vec<_>> {
                let mut restored = Vec::with_capacity(blob.files.len());
                for (offset, (index, file, relative)) in blob.files.into_iter().enumerate() {
                    check_stop(stop)?;
                    if offset == 0 {
                        copy_verified_cache_blob(
                            &blob.previous,
                            &blob.staged,
                            blob.representative,
                        )?;
                    }
                    share_blob(&blob.staged, &output_root.join(&relative), &spill_cache)?;
                    if let Some(reporter) = &reporter {
                        let _guard = progress_lock.lock().unwrap();
                        reporter.advance(file.size);
                    }
                    restored.push((
                        index,
                        ExtractedFile {
                            path: relative,
                            bytes_written: file.size,
                            sha256: file.hash.clone(),
                        },
                    ));
                }
                Ok(restored)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let mut restored: Vec<_> = restored.into_iter().flatten().collect();
    restored.sort_unstable_by_key(|(index, _)| *index);
    Ok(Some(restored.into_iter().map(|(_, file)| file).collect()))
}

/// Makes `destination` a name for `blob`, a blob in this run's cache that many paths may share
/// (see `link_or_copy_spilling`).
fn share_blob(blob: &Path, destination: &Path, spill_cache: &SpillCache) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    link_or_copy_spilling(blob, destination, spill_cache).wrap_err_with(|| {
        format!(
            "failed to restore cached asset {} to {}",
            blob.display(),
            destination.display()
        )
    })
}

fn blob_path(cache_root: &Path, hash: &str) -> Result<PathBuf> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        bail!("invalid SHA-256 cache key");
    }
    Ok(cache_root.join("sha256").join(&hash[..2]).join(hash))
}

fn cache_blob_matches(path: &Path, size: u64, hash: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == size)
        && hash_file(path).is_ok_and(|actual| actual == hash)
}

fn copy_verified_cache_blob(source: &Path, destination: &Path, file: &IngestedFile) -> Result<()> {
    if cache_blob_matches(destination, file.size, &file.hash) {
        return Ok(());
    }
    copy_file(source, destination)
}

/// Puts `source`'s bytes at `destination`, replacing any file already there.
///
/// An extracted entry is stored as the `vfs` file and as its cache blob, and both names point at
/// one file (see `link_or_copy` in `cache`), so the destination is replaced, not written through.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    link_or_copy(source, destination).wrap_err_with(|| {
        format!(
            "failed to restore cached asset {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn write_extracted_payload(
    destination: &Path,
    data: &[u8],
    hash: &str,
    sync_mode: IngestionSync,
    sync: &SyncMeasurements,
    ingestion: Option<&FreshIngestion<'_>>,
) -> Result<()> {
    match ingestion {
        Some(ingestion) => ingestion.materialize(destination, data, hash),
        None => atomic_write(destination, data, sync_mode, sync),
    }
}

fn write_raw_payload(
    destination: &Path,
    data: &[u8],
    sync_mode: IngestionSync,
    sync: &SyncMeasurements,
) -> Result<()> {
    replace_raw_payload(destination, sync_mode, sync, |file| {
        file.write_all(data)?;
        Ok(())
    })
}

fn copy_raw_payload(
    source: &Path,
    destination: &Path,
    sync_mode: IngestionSync,
    sync: &SyncMeasurements,
) -> Result<()> {
    replace_raw_payload(destination, sync_mode, sync, |file| {
        std::io::copy(&mut File::open(source)?, file)?;
        Ok(())
    })
}

fn replace_raw_payload(
    destination: &Path,
    sync_mode: IngestionSync,
    sync: &SyncMeasurements,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    let parent = destination.parent().unwrap();
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    if sync_mode == IngestionSync::PerFile {
        sync.flush(temporary.as_file())?;
    }
    // Replacing the name preserves any old-cache inode shared through other names, including
    // corrupt resumed blobs or dangling symlinks. NamedTempFile cleans failed writes on drop.
    crate::cache::persist_cache_file(temporary, destination)?;
    Ok(())
}

/// Atomically writes data to a file, ensuring the destination is replaced atomically.
fn atomic_write(
    destination: &Path,
    data: &[u8],
    sync_mode: IngestionSync,
    sync: &SyncMeasurements,
) -> Result<()> {
    let file_name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let temporary =
        destination.with_file_name(format!(".{file_name}.{}.partial", std::process::id()));
    let mut file = File::create(&temporary)?;
    file.write_all(data)?;
    if sync_mode == IngestionSync::PerFile {
        sync.flush(&file)?;
    }
    drop(file);
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(&temporary, destination)?;
    Ok(())
}

/// Converts an archive-relative path to a safe, relative path, ensuring it does not contain
/// absolute paths or drive letters.
pub(crate) fn safe_relative_path(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() || normalized.contains(':') {
        bail!("archive contains absolute path: {name}");
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => safe.push(part),
            Component::CurDir => {}
            _ => bail!("archive contains unsafe path: {name}"),
        }
    }
    if safe.as_os_str().is_empty() {
        bail!("archive contains an empty path");
    }
    let extension = safe.extension().and_then(|value| value.to_str());
    let kind = extension.and_then(|extension| {
        if extension.eq_ignore_ascii_case("dds") {
            Some(AssetKind::Texture)
        } else if extension.eq_ignore_ascii_case("nif") {
            Some(AssetKind::Mesh)
        } else if extension.eq_ignore_ascii_case("pex") {
            Some(AssetKind::Script)
        } else if extension.eq_ignore_ascii_case("lod") {
            Some(AssetKind::LodSettings)
        } else if extension.eq_ignore_ascii_case("hkx") {
            Some(AssetKind::Animation)
        } else {
            None
        }
    });
    if let (Some(kind), Some(extension)) = (kind, extension) {
        return canonical_asset_path(name, kind, extension).map(PathBuf::from);
    }
    Ok(safe)
}

fn detect_archive_collision(
    seen: &mut BTreeMap<String, String>,
    relative: &Path,
    original: &str,
) -> Result<()> {
    let key = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if let Some(previous) = seen.insert(key.clone(), original.to_owned()) {
        bail!("archive contains normalized path collision for {key}: {previous} and {original}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_archive(path: &Path, entries: &[dummy_content::Entry<'_>]) {
        let bytes = if path.extension().unwrap() == "bsa" {
            dummy_content::bsa::v105(entries, dummy_content::bsa::Compression::None).unwrap()
        } else {
            dummy_content::ba2::general(entries, dummy_content::ba2::Compression::None).unwrap()
        };
        fs::write(path, bytes).unwrap();
    }

    fn fresh_context<'a>(
        session: &'a RawIngestionSession,
        sync: &'a SyncMeasurements,
        sync_mode: IngestionSync,
    ) -> FreshIngestion<'a> {
        FreshIngestion {
            session,
            archive: session.next_archive.fetch_add(1, Ordering::Relaxed),
            sync_mode,
            sync,
            writes: RawWriteMeasurements::default(),
            used_blobs: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn fresh_parallel_duplicates_write_and_flush_one_blob_per_payload() {
        let directory = tempfile::tempdir().unwrap();
        let names: Vec<_> = (0..513)
            .map(|index| format!("textures/{index:04}.dds"))
            .collect();
        let payloads = [
            b"first payload".as_slice(),
            b"second shared payload".as_slice(),
        ];
        let entries: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| dummy_content::Entry::new(name, payloads[index % 2]))
            .collect();
        for kind in ["bsa", "ba2"] {
            let archive = directory.path().join(format!("duplicates.{kind}"));
            write_archive(&archive, &entries);
            for mode in [
                IngestionSync::PerFile,
                IngestionSync::Archive,
                IngestionSync::None,
            ] {
                let root = directory.path().join(format!("{kind}-{mode:?}"));
                let progress = Mutex::new(Vec::new());
                let callback = |event| progress.lock().unwrap().push(event);
                let outcome = ArchiveExtractor::extract_cached_with_options(
                    &archive,
                    &root.join("vfs"),
                    Path::new("unused"),
                    &root.join("cache"),
                    None,
                    true,
                    Some(&callback),
                    None,
                    IngestionOptions {
                        sync: mode,
                        cpu_jobs: 4,
                        ..IngestionOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(outcome.files.len(), names.len());
                assert_eq!(outcome.timings.selected_files, names.len() as u64);
                assert_eq!(outcome.timings.unique_payloads, 2);
                assert_eq!(outcome.timings.payload_writes, 2);
                assert_eq!(outcome.timings.payload_reuses, names.len() as u64 - 2);
                assert_eq!(
                    outcome.timings.payload_bytes_written,
                    payloads.iter().map(|p| p.len() as u64).sum::<u64>()
                );
                assert_eq!(outcome.timings.physical_copy_writes, 0);
                assert_eq!(
                    outcome.timings.sync_calls,
                    if mode == IngestionSync::PerFile { 2 } else { 0 }
                );
                assert_eq!(count_files(&root.join("cache")), 2);
                for (file, name) in outcome.files.iter().zip(&names) {
                    assert_eq!(&file.path, &PathBuf::from(name));
                    let index = names
                        .iter()
                        .position(|candidate| candidate == name)
                        .unwrap();
                    assert_eq!(
                        fs::read(root.join("vfs").join(name)).unwrap(),
                        payloads[index % 2]
                    );
                }
                let progress = progress.lock().unwrap();
                assert_eq!(progress.len(), 3);
                assert_eq!(progress[1].completed_files, PROGRESS_FILE_STEP);
                assert_eq!(progress[2].completed_files, names.len() as u64);
                assert_eq!(progress[2].completed_bytes, progress[2].total_bytes);
            }
        }
    }

    #[test]
    fn session_deduplicates_across_archives_preserves_overlay_and_resumes_verified_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let session = RawIngestionSession::new(&cache);
        let first = directory.path().join("first.bsa");
        let second = directory.path().join("second.ba2");
        let a = b"shared payload";
        let b = b"replacement payload";
        write_archive(
            &first,
            &[
                dummy_content::Entry::new("textures/overlay.dds", a),
                dummy_content::Entry::new("textures/first-alias.dds", a),
            ],
        );
        write_archive(
            &second,
            &[
                dummy_content::Entry::new("textures/second-alias.dds", a),
                dummy_content::Entry::new("textures/overlay.dds", b),
            ],
        );
        let extract = |archive: &Path, session: &RawIngestionSession| {
            ArchiveExtractor::extract_cached_with_session(
                archive,
                &output,
                Path::new("unused"),
                &cache,
                None,
                true,
                None,
                None,
                session,
                IngestionOptions {
                    sync: IngestionSync::PerFile,
                    cpu_jobs: 2,
                    ..IngestionOptions::default()
                },
            )
            .unwrap()
        };
        let first = extract(&first, &session);
        assert_eq!(first.timings.payload_writes, 1);
        assert_eq!(first.timings.sync_calls, 1);
        let changed = extract(&second, &session);
        assert_eq!(changed.timings.payload_writes, 1);
        assert_eq!(changed.timings.payload_reuses, 1);
        assert_eq!(changed.timings.unique_payloads, 2);
        assert_eq!(changed.timings.sync_calls, if cfg!(unix) { 1 } else { 2 });
        assert_eq!(fs::read(output.join("textures/overlay.dds")).unwrap(), b);
        assert_eq!(
            fs::read(output.join("textures/first-alias.dds")).unwrap(),
            a
        );
        assert_eq!(
            fs::read(output.join("textures/second-alias.dds")).unwrap(),
            a
        );
        assert_eq!(count_files(&cache), 2);
        let resumed = extract(&second, &RawIngestionSession::new(&cache));
        assert_eq!(resumed.timings.payload_writes, 0);
        assert_eq!(resumed.timings.payload_reuses, 2);
        assert_eq!(resumed.timings.sync_calls, 2);
    }

    #[test]
    fn cancelled_archive_can_retry_in_the_same_session_without_trusting_unflushed_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("cancel.bsa");
        write_archive(
            &archive,
            &[
                dummy_content::Entry::new("textures/a.dds", b"a"),
                dummy_content::Entry::new("textures/alias.dds", b"a"),
                dummy_content::Entry::new("textures/b.dds", b"b"),
            ],
        );
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let session = RawIngestionSession::new(&cache);
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let callback = |event: ExtractionProgress| {
            if event.completed_files == event.total_files {
                stopped.store(true, Ordering::Relaxed);
            }
        };
        let stop = || stopped.load(Ordering::Relaxed);
        let options = IngestionOptions {
            sync: IngestionSync::Archive,
            cpu_jobs: 2,
            ..IngestionOptions::default()
        };
        let error = ArchiveExtractor::extract_cached_with_session(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            Some(&callback),
            Some(&stop),
            &session,
            options,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(count_files(&cache), 2);
        let resumed = ArchiveExtractor::extract_cached_with_session(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
            &session,
            options,
        )
        .unwrap();
        assert_eq!(resumed.timings.payload_writes, 0);
        assert_eq!(resumed.timings.payload_reuses, 3);
        assert_eq!(resumed.timings.sync_calls, 0);
        assert!(resumed.cache_hit);
    }

    #[test]
    fn a_partially_flushed_attempt_retries_only_pending_physical_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let session = RawIngestionSession::new(&cache);
        let payloads = [b"a".as_slice(), b"b".as_slice()];
        let sync = SyncMeasurements::default();
        let first = fresh_context(&session, &sync, IngestionSync::Archive);
        for (index, data) in payloads.iter().enumerate() {
            first
                .materialize(
                    &directory.path().join(format!("alias-{index}")),
                    data,
                    &hash_bytes(data),
                )
                .unwrap();
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let stop = stop_after(1);
        let error = pool
            .install(|| first.flush_archive(Some(&stop)))
            .unwrap_err();
        assert!(error.downcast_ref::<Interrupted>().is_some());
        assert_eq!(sync.calls.load(Ordering::Relaxed), 1);
        let retry_sync = SyncMeasurements::default();
        let retry = fresh_context(&session, &retry_sync, IngestionSync::Archive);
        for (index, data) in payloads.iter().enumerate() {
            retry
                .materialize(
                    &directory.path().join(format!("retry-{index}")),
                    data,
                    &hash_bytes(data),
                )
                .unwrap();
        }
        retry.flush_archive(None).unwrap();
        assert_eq!(retry.writes.payload_writes.load(Ordering::Relaxed), 0);
        assert_eq!(
            retry_sync.calls.load(Ordering::Relaxed),
            if cfg!(unix) { 1 } else { 2 }
        );
    }

    #[test]
    fn archive_durability_does_not_trust_a_prior_no_sync_write_or_replaced_inode() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let session = RawIngestionSession::new(&cache);
        let data = b"shared payload";
        let hash = hash_bytes(data);
        let no_sync = SyncMeasurements::default();
        let initial = fresh_context(&session, &no_sync, IngestionSync::None);
        initial
            .materialize(&directory.path().join("old-name"), data, &hash)
            .unwrap();
        assert_eq!(no_sync.calls.load(Ordering::Relaxed), 0);
        let sync = SyncMeasurements::default();
        let durable = fresh_context(&session, &sync, IngestionSync::Archive);
        durable
            .materialize(&directory.path().join("durable-name"), data, &hash)
            .unwrap();
        durable.flush_archive(None).unwrap();
        assert_eq!(durable.writes.payload_writes.load(Ordering::Relaxed), 0);
        assert_eq!(sync.calls.load(Ordering::Relaxed), 1);

        // Valid bytes under a new inode were not flushed by the previous attempt.
        let blob = blob_path(&cache, &hash).unwrap();
        fs::remove_file(&blob).unwrap();
        fs::write(&blob, data).unwrap();
        let replacement_sync = SyncMeasurements::default();
        let replacement = fresh_context(&session, &replacement_sync, IngestionSync::Archive);
        replacement
            .materialize(&directory.path().join("replacement-name"), data, &hash)
            .unwrap();
        replacement.flush_archive(None).unwrap();
        assert_eq!(replacement.writes.payload_writes.load(Ordering::Relaxed), 0);
        assert_eq!(replacement_sync.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn filtered_archive_paths_are_checked_for_collisions_before_blob_writes() {
        let directory = tempfile::tempdir().unwrap();
        for extension in ["ba2", "bsa"] {
            let archive = directory.path().join(format!("collision.{extension}"));
            write_archive(
                &archive,
                &[
                    dummy_content::Entry::new("docs/Readme.txt", b"one"),
                    dummy_content::Entry::new("docs/readme.txt", b"two"),
                    dummy_content::Entry::new("textures/valid.dds", b"valid"),
                ],
            );
            let root = directory.path().join(extension);
            let error = ArchiveExtractor::extract_cached_with_options(
                &archive,
                &root.join("vfs"),
                Path::new("unused"),
                &root.join("cache"),
                None,
                true,
                None,
                None,
                IngestionOptions {
                    selection: IngestionSelection::RuntimeEnglishV1,
                    cpu_jobs: 2,
                    ..IngestionOptions::default()
                },
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("normalized path collision"));
            assert_eq!(count_files(&root), 0);
        }
    }

    #[test]
    fn session_repairs_corruption_by_replacing_the_blob_without_mutating_old_names() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let session = RawIngestionSession::new(&cache);
        let data = b"good payload";
        let hash = hash_bytes(data);
        let first_sync = SyncMeasurements::default();
        let first = fresh_context(&session, &first_sync, IngestionSync::PerFile);
        let old = directory.path().join("old-name");
        first.materialize(&old, data, &hash).unwrap();
        let corrupt = vec![b'!'; data.len()];
        fs::write(&old, &corrupt).unwrap();
        let sync = SyncMeasurements::default();
        let repair = fresh_context(&session, &sync, IngestionSync::Archive);
        let new = directory.path().join("new-name");
        repair.materialize(&new, data, &hash).unwrap();
        repair.flush_archive(None).unwrap();
        assert_eq!(sync.calls.load(Ordering::Relaxed), 1);
        assert_eq!(repair.writes.payload_writes.load(Ordering::Relaxed), 1);
        assert_eq!(fs::read(&old).unwrap(), corrupt);
        assert_eq!(fs::read(&new).unwrap(), data);
        assert_eq!(fs::read(blob_path(&cache, &hash).unwrap()).unwrap(), data);
    }

    #[test]
    fn spills_and_copy_fallbacks_flush_each_new_physical_payload_and_repair_old_spills() {
        fn blob_is_full(from: &Path, to: &Path) -> std::io::Result<()> {
            if from.extension().is_none() {
                return Err(std::io::ErrorKind::TooManyLinks.into());
            }
            fs::hard_link(from, to)
        }
        fn cannot_link(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        let directory = tempfile::tempdir().unwrap();
        let data = b"shared physical payload";
        let hash = hash_bytes(data);
        for mode in [IngestionSync::PerFile, IngestionSync::Archive] {
            for (name, link, copies) in [
                (
                    "spill",
                    blob_is_full as fn(&Path, &Path) -> std::io::Result<()>,
                    1,
                ),
                (
                    "copy",
                    cannot_link as fn(&Path, &Path) -> std::io::Result<()>,
                    2,
                ),
            ] {
                let root = directory.path().join(format!("{mode:?}-{name}"));
                let cache = root.join("cache");
                let session = RawIngestionSession::new(&cache);
                let sync = SyncMeasurements::default();
                let fresh = fresh_context(&session, &sync, mode);
                for index in 0..2 {
                    fresh
                        .materialize_with_link(
                            &root.join(format!("alias-{index}")),
                            data,
                            &hash,
                            link,
                        )
                        .unwrap();
                }
                if mode == IngestionSync::Archive {
                    fresh.flush_archive(None).unwrap();
                }
                assert_eq!(fresh.writes.payload_writes.load(Ordering::Relaxed), 1);
                assert_eq!(
                    fresh.writes.physical_copy_writes.load(Ordering::Relaxed),
                    copies
                );
                assert_eq!(sync.calls.load(Ordering::Relaxed), 1 + copies);
                if name == "spill" {
                    let blob = blob_path(&cache, &hash).unwrap();
                    let spill = blob.with_extension("1");
                    let old = root.join("old-spill-name");
                    fs::hard_link(&spill, &old).unwrap();
                    let corrupt = vec![b'!'; data.len()];
                    fs::write(&spill, &corrupt).unwrap();
                    let repaired_sync = SyncMeasurements::default();
                    let repair = fresh_context(&session, &repaired_sync, IngestionSync::Archive);
                    repair
                        .materialize_with_link(&root.join("repaired-name"), data, &hash, link)
                        .unwrap();
                    repair.flush_archive(None).unwrap();
                    assert_eq!(repair.writes.payload_writes.load(Ordering::Relaxed), 0);
                    assert_eq!(
                        repair.writes.physical_copy_writes.load(Ordering::Relaxed),
                        1
                    );
                    assert_eq!(
                        repaired_sync.calls.load(Ordering::Relaxed),
                        if cfg!(unix) { 1 } else { 2 }
                    );
                    assert_eq!(fs::read(&old).unwrap(), corrupt);
                    assert_eq!(fs::read(&spill).unwrap(), data);
                }
            }
        }
    }

    #[test]
    fn failed_raw_write_cleans_temporary_bytes_and_preserves_existing_shared_inode() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let blob = directory.path().join("blob");
        fs::write(&original, b"original bytes").unwrap();
        fs::hard_link(&original, &blob).unwrap();
        let result = replace_raw_payload(
            &blob,
            IngestionSync::Archive,
            &SyncMeasurements::default(),
            |file| {
                file.write_all(b"partial replacement")?;
                bail!("simulated failed raw write")
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&original).unwrap(), b"original bytes");
        assert_eq!(fs::read(&blob).unwrap(), b"original bytes");
        assert_eq!(count_files(directory.path()), 2);
    }

    fn cached_files(root: &Path, files: &[(&str, &[u8])]) -> IngestionCacheEntry {
        let mut cached = Vec::with_capacity(files.len());
        for (path, bytes) in files {
            let hash = hash_bytes(bytes);
            let blob = blob_path(root, &hash).unwrap();
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            if !blob.exists() {
                fs::write(blob, bytes).unwrap();
            }
            cached.push(IngestedFile {
                path: (*path).to_owned(),
                size: bytes.len() as u64,
                hash,
            });
        }
        IngestionCacheEntry {
            source_hash: hash_bytes(b"archive"),
            selection: IngestionSelection::All,
            recipe: String::new(),
            files: cached,
            reader_version: 0,
        }
    }

    #[test]
    fn parallel_restore_verifies_duplicate_hashes_once_and_preserves_file_order_and_progress() {
        use std::sync::atomic::AtomicUsize;

        let directory = tempfile::tempdir().unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let names: Vec<_> = (0..513).map(|index| format!("docs/{index}.txt")).collect();
        let payloads = [
            b"first shared payload".as_slice(),
            b"second shared payload".as_slice(),
        ];
        let files: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), payloads[index % payloads.len()]))
            .collect();
        let entry = cached_files(&previous_cache, &files);
        let output = directory.path().join("restored/vfs");
        let cache = directory.path().join("restored/cache");
        let checks = AtomicUsize::new(0);
        let stop = || {
            checks.fetch_add(1, Ordering::Relaxed);
            false
        };
        let progress = Mutex::new(Vec::new());
        let callback = |event| progress.lock().unwrap().push(event);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let restored = pool
            .install(|| {
                restore_cached_files(
                    &entry,
                    &output,
                    &previous_cache,
                    &cache,
                    true,
                    Some(&callback),
                    Some(&stop),
                )
            })
            .unwrap()
            .unwrap();

        // One check for each of the two unique verification jobs, then one per restored alias.
        assert_eq!(checks.load(Ordering::Relaxed), files.len() + payloads.len());
        assert_eq!(count_files(&cache), payloads.len());
        for (restored, (name, payload)) in restored.iter().zip(&files) {
            assert_eq!(restored.path, PathBuf::from(*name));
            assert_eq!(restored.bytes_written, payload.len() as u64);
            assert_eq!(fs::read(output.join(name)).unwrap(), *payload);
        }
        let progress = progress.lock().unwrap();
        assert_eq!(progress.len(), 3);
        assert_eq!(progress[0].completed_files, 0);
        assert_eq!(progress[1].completed_files, PROGRESS_FILE_STEP);
        assert_eq!(progress[2].completed_files, files.len() as u64);
        assert_eq!(
            progress[2].completed_bytes,
            entry.files.iter().map(|file| file.size).sum::<u64>()
        );
        assert_eq!(progress[2].completed_bytes, progress[2].total_bytes);
        assert!(
            progress
                .windows(2)
                .all(|pair| pair[0].completed_bytes <= pair[1].completed_bytes)
        );
    }

    #[test]
    fn cached_restore_returns_a_miss_before_writes_for_corrupt_payloads_or_conflicting_sizes() {
        let directory = tempfile::tempdir().unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let mut entry = cached_files(
            &previous_cache,
            &[
                ("docs/first.txt", b"same payload"),
                ("docs/alias.txt", b"same payload"),
                ("docs/second.txt", b"other payload"),
            ],
        );
        let blob = blob_path(&previous_cache, &entry.files[0].hash).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        for (index, corrupt) in [b"bad! payload".as_slice(), b"short".as_slice()]
            .into_iter()
            .enumerate()
        {
            fs::write(&blob, corrupt).unwrap();
            let output = directory.path().join(format!("corrupt-{index}/vfs"));
            let cache = directory.path().join(format!("corrupt-{index}/cache"));
            let restored = pool
                .install(|| {
                    restore_cached_files(&entry, &output, &previous_cache, &cache, true, None, None)
                })
                .unwrap();
            assert!(restored.is_none());
            assert_eq!(count_files(&output), 0);
            assert_eq!(count_files(&cache), 0);
            assert_eq!(fs::read(&blob).unwrap(), corrupt);
        }
        fs::write(&blob, b"same payload").unwrap();
        entry.files[1].size += 1;
        let output = directory.path().join("conflict/vfs");
        let cache = directory.path().join("conflict/cache");
        assert!(
            pool.install(|| restore_cached_files(
                &entry,
                &output,
                &previous_cache,
                &cache,
                true,
                None,
                None,
            ))
            .unwrap()
            .is_none()
        );
        assert_eq!(count_files(&output), 0);
        assert_eq!(count_files(&cache), 0);
    }

    #[test]
    fn cached_restore_rejects_normalized_path_collisions_before_writes() {
        let directory = tempfile::tempdir().unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let entry = cached_files(
            &previous_cache,
            &[("Textures/A.DDS", b"first"), ("textures/a.dds", b"second")],
        );
        let output = directory.path().join("restored/vfs");
        let cache = directory.path().join("restored/cache");
        let error =
            restore_cached_files(&entry, &output, &previous_cache, &cache, true, None, None)
                .unwrap_err();
        assert!(format!("{error:#}").contains("normalized path collision"));
        assert_eq!(count_files(&output), 0);
        assert_eq!(count_files(&cache), 0);
    }

    #[test]
    fn cached_restore_rejects_case_variant_hash_keys_before_workers_or_writes() {
        use std::sync::atomic::AtomicUsize;

        let directory = tempfile::tempdir().unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let mut entry = cached_files(
            &previous_cache,
            &[
                ("docs/first.txt", b"shared payload"),
                ("docs/alias.txt", b"shared payload"),
            ],
        );
        entry.files[1].hash.make_ascii_uppercase();
        assert_ne!(entry.files[0].hash, entry.files[1].hash);
        let output = directory.path().join("restored/vfs");
        let cache = directory.path().join("restored/cache");
        let checks = AtomicUsize::new(0);
        let stop = || {
            checks.fetch_add(1, Ordering::Relaxed);
            false
        };
        let error = restore_cached_files(
            &entry,
            &output,
            &previous_cache,
            &cache,
            false,
            None,
            Some(&stop),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("invalid SHA-256 cache key"));
        assert_eq!(checks.load(Ordering::Relaxed), 0);
        assert_eq!(count_files(&output), 0);
        assert_eq!(count_files(&cache), 0);
    }

    #[test]
    fn cached_restore_cancels_during_verification_and_between_duplicate_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let names: Vec<_> = (0..8).map(|index| format!("docs/{index}.txt")).collect();
        let files: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), b"shared payload".as_slice()))
            .collect();
        let entry = cached_files(&previous_cache, &files);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        for (label, allowed_checks, expected_files) in [("verify", 0, 0), ("aliases", 4, 3)] {
            let output = directory.path().join(label).join("vfs");
            let cache = directory.path().join(label).join("cache");
            let stop = stop_after(allowed_checks);
            let error = pool
                .install(|| {
                    restore_cached_files(
                        &entry,
                        &output,
                        &previous_cache,
                        &cache,
                        true,
                        None,
                        Some(&stop),
                    )
                })
                .unwrap_err();
            assert!(error.downcast_ref::<Interrupted>().is_some());
            assert_eq!(count_files(&output), expected_files);
            if expected_files == 0 {
                assert_eq!(count_files(&cache), 0);
            }
        }
    }

    #[test]
    fn cache_hit_uses_the_configured_bounded_pool_for_parallel_work() {
        use std::sync::{Condvar, atomic::AtomicUsize};
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        let names: Vec<_> = (0..16).map(|index| format!("docs/{index}.txt")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| dummy_content::Entry::new(name, name.as_bytes()))
            .collect();
        fs::write(
            &archive,
            dummy_content::ba2::general(&entries, dummy_content::ba2::Compression::None).unwrap(),
        )
        .unwrap();
        let previous_cache = directory.path().join("previous/cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("previous/vfs"),
            Path::new("unused"),
            &previous_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let checks = AtomicUsize::new(0);
        let arrivals = Mutex::new(0);
        let ready = Condvar::new();
        let stop = || {
            if rayon::current_thread_index().is_none() {
                return false;
            }
            assert_eq!(rayon::current_num_threads(), 2);
            let working = active.fetch_add(1, Ordering::Relaxed) + 1;
            peak.fetch_max(working, Ordering::Relaxed);
            if checks.fetch_add(1, Ordering::Relaxed) < 2 {
                let mut arrived = arrivals.lock().unwrap();
                *arrived += 1;
                ready.notify_all();
                let (arrived, timeout) = ready
                    .wait_timeout_while(arrived, Duration::from_secs(5), |arrived| *arrived < 2)
                    .unwrap();
                assert!(
                    !timeout.timed_out() || *arrived >= 2,
                    "cache verification jobs did not run concurrently"
                );
            }
            active.fetch_sub(1, Ordering::Relaxed);
            false
        };
        let outcome = ArchiveExtractor::extract_cached_with_options(
            &archive,
            &directory.path().join("restored/vfs"),
            &previous_cache,
            &directory.path().join("restored/cache"),
            Some(&first.cache_entry),
            true,
            None,
            Some(&stop),
            IngestionOptions {
                cpu_jobs: 2,
                ..IngestionOptions::default()
            },
        )
        .unwrap();
        assert!(outcome.cache_hit);
        assert_eq!(peak.load(Ordering::Relaxed), 2);
        assert_eq!(outcome.files.len(), names.len());
    }

    fn mixed_archive(root: &Path) -> PathBuf {
        let archive = root.join("mixed.bsa");
        fs::write(
            &archive,
            dummy_content::bsa::v105(
                &[
                    dummy_content::Entry::new("textures/test.dds", b"DDS payload"),
                    dummy_content::Entry::new("meshes/test.nif", b"mesh"),
                    dummy_content::Entry::new("scripts/test.pex", b"script"),
                    dummy_content::Entry::new("lodsettings/test.lod", b"setting"),
                    dummy_content::Entry::new("strings/Test_english.strings", b"names"),
                    dummy_content::Entry::new("strings/Test_english.ilstrings", b"info"),
                    dummy_content::Entry::new("strings/Test_english.dlstrings", b"dialogue"),
                    dummy_content::Entry::new("strings/Test_french.strings", b"francais"),
                    dummy_content::Entry::new(
                        "Meshes/Actors/Character/Character Assets/skeleton.hkx",
                        b"skeleton",
                    ),
                    dummy_content::Entry::new(
                        "Meshes/Actors/Character/Animations/Male/mt_idle.hkx",
                        b"male idle",
                    ),
                    dummy_content::Entry::new(
                        "Meshes/Actors/Character/Animations/Female/mt_idle.hkx",
                        b"female idle",
                    ),
                    dummy_content::Entry::new(
                        "Meshes/Actors/Character/Behaviors/0_master.hkx",
                        b"behaviour",
                    ),
                    dummy_content::Entry::new("sound/test.wav", b"sound"),
                    dummy_content::Entry::new("docs/readme.txt", b"readme"),
                ],
                dummy_content::bsa::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        archive
    }

    #[test]
    fn inventory_returns_canonical_sorted_paths_without_materializing_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let archive = mixed_archive(directory.path());
        let paths = ArchiveExtractor::list_paths(&archive).unwrap();
        assert_eq!(paths.len(), 14);
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(paths.contains(&PathBuf::from(
            "meshes/actors/character/behaviors/0_master.hkx"
        )));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let ba2 = directory.path().join("test.ba2");
        fs::write(
            &ba2,
            dummy_content::ba2::general(
                &[
                    dummy_content::Entry::new("Meshes/B.hkx", b"payload"),
                    dummy_content::Entry::new("meshes/a.hkx", b"other"),
                ],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            ArchiveExtractor::list_paths(&ba2).unwrap(),
            vec![PathBuf::from("meshes/a.hkx"), PathBuf::from("meshes/b.hkx")]
        );
    }

    #[test]
    fn extraction_timings_accept_reports_without_new_fields() {
        let timings: ExtractionTimings =
            serde_json::from_value(serde_json::json!({"extraction_seconds": 1.5})).unwrap();
        assert_eq!(timings.extraction_seconds, 1.5);
        assert_eq!(timings.cache_link_seconds, 0.0);
        assert_eq!(timings.sync_calls, 0);
    }

    #[test]
    fn creature_idle_selection_requires_the_new_filtered_recipe() {
        for selection in [
            IngestionSelection::ConverterInputsV1,
            IngestionSelection::RuntimeEnglishV1,
        ] {
            for profile in shared::creature_idle::CREATURE_IDLES {
                for path in [profile.clip_source, profile.skeleton_hkx] {
                    assert!(selection.includes(Path::new(path)));
                    assert!(selection.includes(Path::new(&path.to_uppercase().replace('/', "\\"))));
                }
            }
            assert!(selection.accepts_recipe(selection.recipe()));
            assert!(selection.accepts_recipe("all-v1"));
            assert!(!selection.accepts_recipe("converter-inputs-v2"));
            assert!(!selection.accepts_recipe("converter-inputs-v3"));
            assert!(!selection.accepts_recipe("runtime-english-v2"));
            assert!(!selection.accepts_recipe("runtime-english-v3"));
            assert!(selection.includes(Path::new("meshes/actors/cow/animations/walkforward.hkx")));
            assert!(selection.includes(Path::new("meshes/actors/cow/behaviors/noncombatidle.hkx")));
        }
    }

    #[test]
    fn runtime_selection_is_case_insensitive_and_keeps_only_consumed_paths() {
        for (path, expected) in [
            ("Textures/Test.DDS", true),
            ("meshes/test.nif", true),
            ("scripts/test.pex", true),
            ("LODSettings/Test.LOD", true),
            ("Strings/Test_ENGLISH.STRINGS", true),
            ("strings/test_english.ilstrings", true),
            ("strings/test_english.dlstrings", true),
            ("strings/test_french.strings", false),
            ("strings/test.english.strings", false),
            ("strings/subdir/test_english.strings", false),
            ("sound/test.wav", false),
            ("meshes/animation.hkx", true),
            (
                "Meshes/Actors/Character/Character Assets/skeleton.hkx",
                true,
            ),
            ("meshes/actors/character/animations/male/mt_idle.hkx", true),
            ("meshes/actors/character/behaviors/0_master.hkx", true),
        ] {
            assert_eq!(
                selection_includes(IngestionSelection::RuntimeEnglishV1, Path::new(path)),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn converter_inputs_include_every_string_bank_and_character_animation_input() {
        for (path, expected) in [
            ("strings/test_french.strings", true),
            ("strings/subdir/test_english.strings", true),
            (
                "Meshes/Actors/Character/Character Assets/skeleton.hkx",
                true,
            ),
            (
                "meshes/actors/character/animations/female/mt_idle.hkx",
                true,
            ),
            ("meshes/actors/character/behaviors/0_master.hkx", true),
            ("meshes/animations/furniture/chair.hkx", true),
        ] {
            assert_eq!(
                selection_includes(IngestionSelection::ConverterInputsV1, Path::new(path)),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn all_cache_can_supply_runtime_subset_but_subset_requires_full_reextraction() {
        let directory = tempfile::tempdir().unwrap();
        let archive = mixed_archive(directory.path());
        let first_cache = directory.path().join("first/cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("first/vfs"),
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(first.cache_entry.selection, IngestionSelection::All);
        assert_eq!(first.files.len(), 14);
        let subset_output = directory.path().join("subset/vfs");
        let subset_cache = directory.path().join("subset/cache");
        let subset = ArchiveExtractor::extract_cached_with_options(
            &archive,
            &subset_output,
            &first_cache,
            &subset_cache,
            Some(&first.cache_entry),
            true,
            None,
            None,
            IngestionOptions {
                selection: IngestionSelection::RuntimeEnglishV1,
                cpu_jobs: 1,
                ..IngestionOptions::default()
            },
        )
        .unwrap();
        assert!(subset.cache_hit);
        assert_eq!(subset.files.len(), 11);
        assert_eq!(
            subset.cache_entry.selection,
            IngestionSelection::RuntimeEnglishV1
        );
        assert_eq!(count_files(&subset_cache), 11);
        assert!(!subset_output.join("sound/test.wav").exists());
        assert!(!subset_output.join("strings/Test_french.strings").exists());
        assert!(
            subset_output
                .join("meshes/actors/character/character assets/skeleton.hkx")
                .is_file()
        );
        assert!(
            subset_output
                .join("meshes/actors/character/animations/male/mt_idle.hkx")
                .is_file()
        );
        assert!(
            subset_output
                .join("meshes/actors/character/behaviors/0_master.hkx")
                .exists()
        );
        let full_output = directory.path().join("full/vfs");
        let full = ArchiveExtractor::extract_cached(
            &archive,
            &full_output,
            &subset_cache,
            &directory.path().join("full/cache"),
            Some(&subset.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(
            full.cache_hit,
            "a retained all-files pack independently proves broader coverage"
        );
        assert_eq!(full.files.len(), 14);
        assert!(full_output.join("sound/test.wav").is_file());
    }

    #[test]
    fn sync_modes_flush_selected_files_and_cancellation_precedes_cache_entry_publication() {
        let directory = tempfile::tempdir().unwrap();
        let archive = mixed_archive(directory.path());
        for mode in [
            IngestionSync::PerFile,
            IngestionSync::Archive,
            IngestionSync::None,
        ] {
            let root = directory.path().join(format!("{mode:?}"));
            let outcome = ArchiveExtractor::extract_cached_with_options(
                &archive,
                &root.join("vfs"),
                Path::new("unused"),
                &root.join("cache"),
                None,
                true,
                None,
                None,
                IngestionOptions {
                    selection: IngestionSelection::RuntimeEnglishV1,
                    sync: mode,
                    cpu_jobs: 1,
                },
            )
            .unwrap();
            assert_eq!(outcome.files.len(), 11);
            assert_eq!(
                outcome.timings.sync_calls,
                if mode == IngestionSync::PerFile {
                    11
                } else {
                    0
                }
            );
            if mode != IngestionSync::Archive {
                assert_eq!(outcome.timings.sync_seconds, 0.0);
            }
        }
        let output = directory.path().join("stopped/vfs");
        let cache = directory.path().join("stopped/cache");
        let stop = || count_files(&output) >= 3;
        let error = ArchiveExtractor::extract_cached_with_options(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            Some(&stop),
            IngestionOptions {
                selection: IngestionSelection::RuntimeEnglishV1,
                sync: IngestionSync::Archive,
                cpu_jobs: 1,
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(count_files(&output), 3);
        assert_eq!(count_files(&cache), 3);
        // Workspace blobs survive the failed archive, but a new session must verify and flush
        // them before returning a cache entry. It does not inherit the abandoned flush state.
        let resumed = ArchiveExtractor::extract_cached_with_options(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
            IngestionOptions {
                selection: IngestionSelection::RuntimeEnglishV1,
                sync: IngestionSync::Archive,
                cpu_jobs: 2,
            },
        )
        .unwrap();
        assert!(resumed.cache_hit);
        assert_eq!(resumed.files.len(), 11);
        assert_eq!(count_files(&output), 11);
        assert_eq!(resumed.timings.payload_writes, 8);
        assert_eq!(resumed.timings.payload_reuses, 3);
        assert_eq!(resumed.timings.sync_calls, 0);
    }

    #[test]
    fn no_sync_requires_verification_even_for_direct_library_calls() {
        let options = IngestionOptions {
            sync: IngestionSync::None,
            cpu_jobs: 1,
            ..IngestionOptions::default()
        };
        let error = ArchiveExtractor::extract_cached_with_options(
            Path::new("unused.bsa"),
            Path::new("unused-output"),
            Path::new("unused-previous"),
            Path::new("unused-cache"),
            None,
            false,
            None,
            None,
            options,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("requires cache verification"));
    }

    #[test]
    fn fresh_and_restored_inputs_replace_corrupt_staged_blobs_without_overwriting_old_links() {
        let directory = tempfile::tempdir().unwrap();
        let archive = mixed_archive(directory.path());
        let previous_cache = directory.path().join("previous/cache");
        let previous = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("previous/vfs"),
            Path::new("unused"),
            &previous_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let payload = b"DDS payload";
        let hash = hash_bytes(payload);
        for (index, corrupt) in [vec![b'!'; payload.len()], b"short".to_vec()]
            .into_iter()
            .enumerate()
        {
            for restore in [false, true] {
                let root = directory.path().join(format!("repair-{index}-{restore}"));
                let output = root.join("vfs");
                let cache = root.join("cache");
                let blob = blob_path(&cache, &hash).unwrap();
                fs::create_dir_all(blob.parent().unwrap()).unwrap();
                fs::create_dir_all(output.join("textures")).unwrap();
                let old_name = root.join("old-cache-inode");
                fs::write(&old_name, &corrupt).unwrap();
                link_or_copy(&old_name, &blob).unwrap();
                link_or_copy(&old_name, &output.join("textures/test.dds")).unwrap();
                let outcome = ArchiveExtractor::extract_cached_with_options(
                    &archive,
                    &output,
                    &previous_cache,
                    &cache,
                    restore.then_some(&previous.cache_entry),
                    true,
                    None,
                    None,
                    IngestionOptions {
                        selection: IngestionSelection::RuntimeEnglishV1,
                        sync: IngestionSync::None,
                        cpu_jobs: 2,
                    },
                )
                .unwrap();
                assert!(
                    outcome.cache_hit,
                    "sealed packs prove the source even without a manifest"
                );
                assert_eq!(fs::read(output.join("textures/test.dds")).unwrap(), payload);
                assert_eq!(fs::read(blob).unwrap(), payload);
                assert_eq!(fs::read(&old_name).unwrap(), corrupt);
                assert_eq!(
                    fs::read(blob_path(&previous_cache, &hash).unwrap()).unwrap(),
                    payload
                );
            }
        }
    }

    #[test]
    fn normalizes_archive_paths() {
        assert_eq!(
            safe_relative_path(r"meshes\actors\wolf.nif").unwrap(),
            PathBuf::from("meshes/actors/wolf.nif")
        );
        assert_eq!(
            safe_relative_path(r"textures\authoring\data\textures\landscape\Rock.DDS").unwrap(),
            PathBuf::from("textures/landscape/rock.dds")
        );
        assert_eq!(
            safe_relative_path(r"Meshes\Actors\Character\Character Assets\Skeleton.HKX").unwrap(),
            PathBuf::from("meshes/actors/character/character assets/skeleton.hkx")
        );
        assert_eq!(
            safe_relative_path(r"Meshes\Actors\Character\Animations\Male\MT_Idle.HKX").unwrap(),
            PathBuf::from("meshes/actors/character/animations/male/mt_idle.hkx")
        );
    }

    #[test]
    fn rejects_normalized_collisions_within_an_archive() {
        let mut seen = BTreeMap::new();
        detect_archive_collision(&mut seen, Path::new("textures/a.dds"), "Textures/A.DDS").unwrap();
        assert!(
            detect_archive_collision(&mut seen, Path::new("textures/a.dds"), "textures/a.dds",)
                .is_err()
        );
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_relative_path("../outside.txt").is_err());
        assert!(safe_relative_path("C:/outside.txt").is_err());
        assert!(blob_path(Path::new("cache"), "../../outside").is_err());
    }

    #[test]
    fn rejects_traversal_entries_during_archive_extraction() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("vfs");
        let mut bytes = dummy_content::ba2::general(
            &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
            dummy_content::ba2::Compression::None,
        )
        .unwrap();
        let names_offset = 24 + 36;
        let escaped = b"../../escape";
        bytes[names_offset..names_offset + 2]
            .copy_from_slice(&(escaped.len() as u16).to_le_bytes());
        bytes[names_offset + 2..names_offset + 2 + escaped.len()].copy_from_slice(escaped);

        let archive = directory.path().join("evil.ba2");
        fs::write(&archive, &bytes).unwrap();
        assert!(ArchiveExtractor::extract(&archive, &output).is_err());
        assert!(!directory.path().join("escape").exists());
        assert!(!directory.path().parent().unwrap().join("escape").exists());
    }

    /// Whether the filesystem holding `directory` can hard-link. Where it cannot, `link_or_copy`
    /// falls back to a copy, so a test can only ask for equal bytes there.
    fn hard_links_supported(directory: &Path) -> bool {
        let probe = directory.join(".link-probe");
        let link = directory.join(".link-probe-link");
        fs::write(&probe, b"probe").unwrap();
        let supported = fs::hard_link(&probe, &link).is_ok();
        let _ = fs::remove_file(&link);
        fs::remove_file(&probe).unwrap();
        supported
    }

    /// Asserts every name in `names` is one file, by appending a byte through the first name and
    /// watching it appear through all the others: a hard link sees a write made through another
    /// name, a copy does not. On a filesystem without hard links the names are copies by design,
    /// and only their bytes are compared.
    fn assert_one_file(names: &[&Path]) {
        let (first, rest) = names.split_first().expect("at least one name");
        if !hard_links_supported(first.parent().expect("a file inside a directory")) {
            let bytes = fs::read(first).unwrap();
            for name in rest {
                assert_eq!(fs::read(name).unwrap(), bytes, "{} differs", name.display());
            }
            return;
        }
        fs::OpenOptions::new()
            .append(true)
            .open(first)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        let bytes = fs::read(first).unwrap();
        assert!(
            bytes.last() == Some(&b'+'),
            "{} was not written",
            first.display()
        );
        for name in rest {
            assert_eq!(
                fs::read(name).unwrap(),
                bytes,
                "{} does not share the file at {}",
                name.display(),
                first.display()
            );
        }
    }

    #[test]
    fn fresh_extractions_link_blobs_and_vfs_files_where_the_filesystem_can() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let output = directory.path().join("vfs");
        let cache = directory.path().join(".ingestion-cache");
        let extracted = ArchiveExtractor::extract_cached(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!extracted.cache_hit);

        // A fresh install stores each extracted file as the `vfs` entry and as its blob; the two
        // names have to be one file, or the tree holds every asset twice. NTFS, ext4 and APFS
        // support links, so extraction must not have fallen back to copying.
        let vfs = output.join("textures/test.dds");
        let blob = blob_path(&cache, &extracted.files[0].sha256).unwrap();
        assert_one_file(&[&vfs, &blob]);
        assert!(fs::read(&vfs).unwrap().starts_with(b"DDS "));
    }

    #[test]
    fn bsa_entries_from_an_older_reader_are_not_reused() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.bsa");
        fs::write(
            &archive,
            dummy_content::bsa::v105(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::bsa::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        // `none` never exists, so a reuse can only come from the entry itself, not from packs.
        let extract =
            |output: &str, previous_cache: &str, previous: Option<&IngestionCacheEntry>| {
                ArchiveExtractor::extract_cached(
                    &archive,
                    &directory.path().join(output),
                    &directory.path().join(previous_cache),
                    &directory.path().join(format!("{output}/.ingestion-cache")),
                    previous,
                    true,
                    None,
                    None,
                )
                .unwrap()
            };

        let first = extract("first", "none/.ingestion-cache", None);
        assert!(!first.cache_hit);
        assert_eq!(first.cache_entry.reader_version, BSA_READER_VERSION);

        // An entry written before names were decoded as Windows-1252 has no version.
        let mut old = first.cache_entry.clone();
        old.reader_version = 0;
        assert!(!extract("second", "none/.ingestion-cache", Some(&old)).cache_hit);

        // An entry from the current reader is reused.
        assert!(extract("third", "first/.ingestion-cache", Some(&first.cache_entry)).cache_hit);
    }

    #[test]
    fn stale_entry_cannot_map_a_decoded_path_to_another_payload() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.bsa");
        fs::write(
            &archive,
            dummy_content::bsa::v105(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::bsa::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("first"),
            &directory.path().join("none/.ingestion-cache"),
            &directory.path().join("first/.ingestion-cache"),
            None,
            true,
            None,
            None,
        )
        .unwrap();
        // Valid blobs for the true payload and a same-size wrong one, but no packs: the entry
        // alone decides which blob a path uses, as a lossy name mapping would.
        let previous = directory.path().join("previous/.ingestion-cache");
        for payload in [b"DDS ".as_slice(), b"DDX ".as_slice()] {
            let blob = blob_path(&previous, &hash_bytes(payload)).unwrap();
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            fs::write(blob, payload).unwrap();
        }
        let mut stale = first.cache_entry.clone();
        stale.files[0].hash = hash_bytes(b"DDX ");
        stale.reader_version = 0;

        let stale_outcome = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("stale"),
            &previous,
            &directory.path().join("stale/.ingestion-cache"),
            Some(&stale),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!stale_outcome.cache_hit);
        assert_eq!(
            fs::read(directory.path().join("stale/textures/test.dds")).unwrap(),
            b"DDS "
        );

        let current = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("current"),
            &previous,
            &directory.path().join("current/.ingestion-cache"),
            Some(&first.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(current.cache_hit);
        assert_eq!(
            fs::read(directory.path().join("current/textures/test.dds")).unwrap(),
            b"DDS "
        );
    }

    #[test]
    fn stale_entry_for_an_empty_archive_is_not_a_cache_hit() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("empty.bsa");
        fs::write(
            &archive,
            dummy_content::bsa::v105(&[], dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();
        let extract =
            |output: &str, previous: &str, previous_entry: Option<&IngestionCacheEntry>| {
                ArchiveExtractor::extract_cached(
                    &archive,
                    &directory.path().join(output),
                    &directory.path().join(previous),
                    &directory.path().join(format!("{output}/.ingestion-cache")),
                    previous_entry,
                    true,
                    None,
                    None,
                )
                .unwrap()
            };
        let first = extract("first", "none/.ingestion-cache", None);
        assert!(!first.cache_hit);
        assert_eq!(first.cache_entry.reader_version, BSA_READER_VERSION);

        // Nothing is selected, so no blob or pack can vouch for the entry; only its version
        // separates a stale stamp from the current reader.
        let mut stale = first.cache_entry.clone();
        stale.reader_version = 0;
        assert!(!extract("stale", "none/.ingestion-cache", Some(&stale)).cache_hit);
        assert!(
            extract(
                "current",
                "first/.ingestion-cache",
                Some(&first.cache_entry)
            )
            .cache_hit
        );
    }

    #[test]
    fn only_bsa_names_carry_a_reader_version() {
        assert_eq!(
            archive_reader_version(b"BSA\0\x68\0\0\0"),
            BSA_READER_VERSION
        );
        assert_eq!(archive_reader_version(b"BTDX\x01\0\0\0"), 0);
        assert_eq!(archive_reader_version(b"BS"), 0);
    }

    #[test]
    fn fresh_extractions_link_every_path_with_the_same_bytes_to_one_blob() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[
                    dummy_content::Entry::new("textures/first.dds", b"DDS "),
                    dummy_content::Entry::new("textures/second.dds", b"DDS "),
                ],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let output = directory.path().join("vfs");
        let cache = directory.path().join(".ingestion-cache");
        let extracted = ArchiveExtractor::extract_cached(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!extracted.cache_hit);
        assert_eq!(extracted.files[0].sha256, extracted.files[1].sha256);

        // Both entries hold the same bytes, so both `vfs` paths are names for their one blob.
        let blob = blob_path(&cache, &extracted.files[0].sha256).unwrap();
        let first = output.join("textures/first.dds");
        let second = output.join("textures/second.dds");
        assert_one_file(&[&blob, &first, &second]);
    }

    #[test]
    fn a_later_archive_replaces_a_linked_vfs_file_without_changing_its_blob() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("vfs");
        let cache = directory.path().join(".ingestion-cache");
        let mut extracted = Vec::new();
        for (name, bytes) in [("base.ba2", b"base"), ("override.ba2", b"over")] {
            let archive = directory.path().join(name);
            fs::write(
                &archive,
                dummy_content::ba2::general(
                    &[dummy_content::Entry::new("textures/rock.dds", bytes)],
                    dummy_content::ba2::Compression::None,
                )
                .unwrap(),
            )
            .unwrap();
            extracted.push(
                ArchiveExtractor::extract_cached(
                    &archive,
                    &output,
                    Path::new("unused"),
                    &cache,
                    None,
                    true,
                    None,
                    None,
                )
                .unwrap(),
            );
        }

        assert_eq!(fs::read(output.join("textures/rock.dds")).unwrap(), b"over");
        let base_blob = blob_path(&cache, &extracted[0].files[0].sha256).unwrap();
        assert_eq!(fs::read(base_blob).unwrap(), b"base");
    }

    #[test]
    fn cache_hits_link_blobs_and_vfs_files_where_the_filesystem_can() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let first_output = directory.path().join("first/vfs");
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &first_output,
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!first.cache_hit);

        let second_output = directory.path().join("second/vfs");
        let second_cache = directory.path().join("second/.ingestion-cache");
        let second = ArchiveExtractor::extract_cached(
            &archive,
            &second_output,
            &first_cache,
            &second_cache,
            Some(&first.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);

        // The reused blob, the blob restored into the new cache and the `vfs` entries of both
        // runs all describe the same bytes, so they are all one file.
        let hash = &second.files[0].sha256;
        let first_blob = blob_path(&first_cache, hash).unwrap();
        let second_blob = blob_path(&second_cache, hash).unwrap();
        let first_vfs = first_output.join("textures/test.dds");
        let second_vfs = second_output.join("textures/test.dds");
        assert_one_file(&[&first_blob, &second_blob, &first_vfs, &second_vfs]);
        assert!(fs::read(&second_vfs).unwrap().starts_with(b"DDS "));
    }

    #[test]
    fn reuses_verified_archive_blobs_and_recovers_from_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let first_output = directory.path().join("first/vfs");
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &first_output,
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!first.cache_hit);
        assert_eq!(
            fs::read(first_output.join("textures/test.dds")).unwrap(),
            b"DDS "
        );

        let second_output = directory.path().join("second/vfs");
        let second_cache = directory.path().join("second/.ingestion-cache");
        let second = ArchiveExtractor::extract_cached(
            &archive,
            &second_output,
            &first_cache,
            &second_cache,
            Some(&first.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);
        assert_eq!(
            fs::read(second_output.join("textures/test.dds")).unwrap(),
            b"DDS "
        );

        let blob = blob_path(&second_cache, &second.files[0].sha256).unwrap();
        fs::write(&blob, b"BAD!").unwrap();
        let third = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("third/vfs"),
            &second_cache,
            &directory.path().join("third/.ingestion-cache"),
            Some(&second.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(
            third.cache_hit,
            "valid sealed packs recover corrupted blobs"
        );
    }

    /// A stop that turns true after `allowed` checks, so a test can stop an archive at a known
    /// entry.
    fn stop_after(allowed: usize) -> impl Fn() -> bool + Send + Sync {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        move || checks.fetch_add(1, Ordering::Relaxed) >= allowed
    }

    fn count_files(root: &Path) -> usize {
        if !root.exists() {
            return 0;
        }
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != "batches")
            .filter(|entry| entry.as_ref().unwrap().file_type().is_file())
            .count()
    }

    #[test]
    fn a_stop_ends_extraction_and_cache_restore_between_entries() {
        let directory = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..8).map(|index| format!("docs/{index}.txt")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| dummy_content::Entry::new(name, name.as_bytes()))
            .collect();
        let ba2 = directory.path().join("assets.ba2");
        fs::write(
            &ba2,
            dummy_content::ba2::general(&entries, dummy_content::ba2::Compression::None).unwrap(),
        )
        .unwrap();
        let bsa = directory.path().join("assets.bsa");
        fs::write(
            &bsa,
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();

        // Extraction: one entry is let through. Its blob can survive in the workspace, but the
        // interrupted archive does not return a cache entry.
        for archive in [&ba2, &bsa] {
            let output = directory.path().join("stopped/vfs");
            let cache = directory.path().join("stopped/.ingestion-cache");
            let stop = || count_files(&output) >= 1;
            let error = ArchiveExtractor::extract_cached(
                archive,
                &output,
                Path::new("unused"),
                &cache,
                None,
                true,
                None,
                Some(&stop),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("interrupted"));
            assert_eq!(count_files(&output), 1, "{}", archive.display());
            assert_eq!(count_files(&cache), 1, "{}", archive.display());
            fs::remove_dir_all(directory.path().join("stopped")).unwrap();
        }

        // Cache restore: a full run fills the cache, then a restore from it is stopped after
        // its verification pass and three copies.
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &ba2,
            &directory.path().join("first/vfs"),
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let output = directory.path().join("second/vfs");
        let stop = || count_files(&output) >= 3;
        let error = ArchiveExtractor::extract_cached_with_options(
            &ba2,
            &output,
            &first_cache,
            &directory.path().join("second/.ingestion-cache"),
            Some(&first.cache_entry),
            true,
            None,
            Some(&stop),
            IngestionOptions {
                cpu_jobs: 1,
                ..IngestionOptions::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(count_files(&output), 3);

        // A stop that never fires changes nothing.
        let never = || false;
        let complete = ArchiveExtractor::extract_cached(
            &ba2,
            &directory.path().join("third/vfs"),
            Path::new("unused"),
            &directory.path().join("third/.ingestion-cache"),
            None,
            true,
            None,
            Some(&never),
        )
        .unwrap();
        assert_eq!(complete.files.len(), names.len());
    }
}
