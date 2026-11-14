//! Durable archive batches. Packs are the canonical cache; ordinary blobs and
//! VFS files can be recreated after an interrupted write or a power loss.
//! The mapped source inode must remain immutable during extraction. Source
//! replacement is detected before accepting either success or a partial run.

use super::{
    ArchiveSelection, ExtractOptions, ExtractedFile, ExtractionOutcome, ExtractionProgressCallback,
    ExtractionTimings, FreshIngestion, IngestionSync, Interrupted, ProgressReporter,
    RawIngestionSession, RawWriteMeasurements, StopCheck, SyncMeasurements, archive_reader_version,
    ba2, blob_path, bsa, check_stop, detect_archive_collision, restore_cached_files_with_io_limit,
    safe_relative_path,
};
use crate::cache::{
    IngestedFile, IngestionCacheEntry, SpillCache, hash_bytes, hash_file, is_zero, link_or_copy,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure},
};
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const PACK_MAGIC: &[u8; 8] = b"MINGPK01";
const PACK_VERSION: u32 = 2;
const BATCH_FILES: usize = 256;
const BATCH_BYTES: usize = 16 * 1024 * 1024;
const QUEUED_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct WriterQueueStopped;

impl std::fmt::Display for WriterQueueStopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("archive writer queue stopped")
    }
}

impl std::error::Error for WriterQueueStopped {}

fn error_priority(error: &color_eyre::Report) -> u8 {
    if error.downcast_ref::<WriterQueueStopped>().is_some() {
        0
    } else if error
        .downcast_ref::<Interrupted>()
        .is_some_and(|stop| stop.cause().is_none())
    {
        1
    } else {
        2
    }
}

fn retain_error(current: &mut Option<color_eyre::Report>, candidate: color_eyre::Report) {
    if current
        .as_ref()
        .is_none_or(|error| error_priority(&candidate) > error_priority(error))
    {
        *current = Some(candidate);
    }
}

#[derive(Debug)]
enum Payload<'a> {
    Bsa(bsa::BsaRawEntry<'a>),
    Ba2(ba2::Ba2RawEntry<'a>),
}

impl Payload<'_> {
    fn decoded_size(&self) -> Result<usize> {
        let size = match self {
            Self::Bsa(entry) if entry.is_compressed => {
                let bytes = entry.payload.get(..4).ok_or_else(|| {
                    color_eyre::eyre::eyre!("compressed BSA payload is truncated")
                })?;
                u32::from_le_bytes(bytes.try_into().unwrap()) as usize
            }
            Self::Bsa(entry) => entry.payload.len(),
            Self::Ba2(entry) => entry.decoded_size(),
        };
        ensure!(
            size <= MAX_FILE_BYTES,
            "archive entry exceeds the 1 GiB decoding limit"
        );
        Ok(size)
    }

    fn decompress(self) -> Result<Vec<u8>> {
        match self {
            Self::Bsa(entry) => entry.decompress(),
            Self::Ba2(entry) => entry.decompress(),
        }
    }
}

struct Pending<'a> {
    path: PathBuf,
    bytes: usize,
    payload: Payload<'a>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackItem {
    path: String,
    size: u64,
    sha256: String,
    offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackIndex {
    version: u32,
    source_hash: String,
    recipe: String,
    /// Reader that decoded the item paths. Absent means 0; a BSA pack from an older reader
    /// holds wrong names and is ignored rather than restored.
    #[serde(default, skip_serializing_if = "is_zero")]
    reader_version: u32,
    files: Vec<PackItem>,
}

#[derive(Clone)]
struct PackedFile {
    pack: PathBuf,
    offset: u64,
    item: PackItem,
}

struct Prepared {
    file: ExtractedFile,
    data: Option<Vec<u8>>,
    existing: Option<PathBuf>,
    durable: bool,
}

impl Prepared {
    fn bytes(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        if let Some(bytes) = &self.data {
            Ok(std::borrow::Cow::Borrowed(bytes))
        } else {
            let path = self
                .existing
                .as_ref()
                .ok_or_else(|| color_eyre::eyre::eyre!("prepared archive file has no payload"))?;
            Ok(std::borrow::Cow::Owned(fs::read(path)?))
        }
    }
}

struct ByteBudget {
    limit: usize,
    used: Mutex<usize>,
    ready: Condvar,
    closed: AtomicBool,
}

impl ByteBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<BudgetPermit> {
        let mut used = self.used.lock().unwrap();
        while *used + bytes > self.limit {
            if self.closed.load(Ordering::Acquire) {
                return Err(WriterQueueStopped.into());
            }
            used = self.ready.wait(used).unwrap();
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(WriterQueueStopped.into());
        }
        *used += bytes;
        Ok(BudgetPermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    fn close(&self) {
        // Hold the same mutex as the wait predicate to avoid a lost wakeup.
        let _used = self.used.lock().unwrap();
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
    }
}

struct BudgetPermit {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for BudgetPermit {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap() -= self.bytes;
        self.budget.ready.notify_all();
    }
}

struct Batch {
    files: Vec<Prepared>,
    _permit: BudgetPermit,
}

struct WriterExit(Arc<ByteBudget>);

impl Drop for WriterExit {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Test instrumentation is local to an extraction; concurrent tests do not
/// share hooks or alter the production writer path.
#[derive(Debug, Clone, Copy)]
pub(super) enum WriterEvent {
    Started,
    Sealed,
    Finished,
    ProducerFailed,
}

type WriterObserver<'a> = Option<&'a (dyn Fn(WriterEvent) + Send + Sync)>;

#[allow(clippy::too_many_arguments)]
pub(super) fn extract(
    archive_path: &Path,
    output: &Path,
    previous_cache: &Path,
    cache: &Path,
    previous: Option<&IngestionCacheEntry>,
    options: &ExtractOptions,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
    observer: WriterObserver<'_>,
) -> Result<ExtractionOutcome> {
    let session = RawIngestionSession::new(cache);
    extract_with_session(
        archive_path,
        output,
        previous_cache,
        cache,
        previous,
        options,
        progress,
        stop,
        observer,
        &session,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn extract_with_session(
    archive_path: &Path,
    output: &Path,
    previous_cache: &Path,
    cache: &Path,
    previous: Option<&IngestionCacheEntry>,
    options: &ExtractOptions,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
    observer: WriterObserver<'_>,
    session: &RawIngestionSession,
) -> Result<ExtractionOutcome> {
    ensure!(
        options.cpu_jobs > 0 && options.io_jobs > 0,
        "archive CPU and I/O worker counts must be greater than zero"
    );
    check_stop(stop)?;
    let hash_started = std::time::Instant::now();
    let file = File::open(archive_path)?;
    // The archive is immutable input for this scoped extraction.
    let bytes = unsafe { Mmap::map(&file) }?;
    // Hash the exact inode decoded below; a path can be replaced between opens.
    let source_hash = hash_bytes(&bytes);
    let reader_version = archive_reader_version(&bytes);
    let mut timings = ExtractionTimings {
        source_hash_seconds: hash_started.elapsed().as_secs_f64(),
        ..ExtractionTimings::default()
    };
    let raw: Vec<(String, Payload<'_>)> = match bytes.get(..4) {
        Some(b"BSA\0") => bsa::iter_raw_entries(&bytes)?
            .into_iter()
            .map(|entry| (entry.name.clone(), Payload::Bsa(entry)))
            .collect(),
        Some(b"BTDX") => ba2::iter_raw_entries(&bytes)?
            .into_iter()
            .map(|entry| (entry.name.clone(), Payload::Ba2(entry)))
            .collect(),
        _ => bail!("unsupported archive magic in {}", archive_path.display()),
    };
    let mut seen = BTreeMap::new();
    let mut selected = Vec::new();
    for (name, payload) in raw {
        let path = safe_relative_path(&name)?;
        // Validate the whole namespace before filtering: unsupported files do
        // not conceal traversal or case-folded collisions.
        detect_archive_collision(&mut seen, &path, &name)?;
        if options.selection.includes(&path) {
            let bytes = payload.decoded_size()?;
            selected.push(Pending {
                path,
                bytes,
                payload,
            });
        }
    }
    let total_bytes = selected.iter().map(|entry| entry.bytes as u64).sum();
    let reporter = progress
        .map(|callback| ProgressReporter::new(callback, selected.len() as u64, total_bytes));

    let checkpoint_sync = SyncMeasurements::default();
    let restore_started = std::time::Instant::now();
    let packed = if options.reuse_cache {
        restore_pack_inventory(
            previous_cache,
            cache,
            &source_hash,
            reader_version,
            options.selection,
            options.sync,
            &checkpoint_sync,
            stop,
        )?
    } else {
        BTreeMap::new()
    };
    let mut expected = BTreeMap::new();
    let reusable = previous.filter(|entry| {
        options.reuse_cache
            && entry.source_hash == source_hash
            && entry.reader_version == reader_version
            && entry.covers(options.selection)
    });
    if let Some(previous) = reusable {
        for file in &previous.files {
            let path = safe_relative_path(&file.path)?;
            if options.selection.includes(&path) {
                ensure!(
                    expected.insert(path, file).is_none(),
                    "archive cache contains duplicate normalized paths"
                );
            }
        }
    }
    // A complete sealed inventory permits #209's parallel, per-hash restore path.
    // Missing/corrupt derived blobs fall through to reconstruction from packs.
    if observer.is_none()
        && expected.len() == selected.len()
        && reusable.is_some()
        && expected.iter().all(|(path, file)| {
            packed.get(path).is_some_and(|packed| {
                packed.item.sha256 == file.hash && packed.item.size == file.size
            })
        })
    {
        let entry = IngestionCacheEntry {
            source_hash: source_hash.clone(),
            selection: options.selection,
            recipe: options.selection.recipe().to_owned(),
            files: expected.values().map(|file| (*file).clone()).collect(),
            reader_version,
        };
        let restore_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(options.cpu_jobs)
            .build()?;
        if let Some(files) = restore_pool.install(|| {
            restore_cached_files_with_io_limit(
                &entry,
                output,
                previous_cache,
                cache,
                true,
                progress,
                stop,
                options.io_jobs,
            )
        })? {
            verify_checkpoint_source(archive_path, &source_hash, &BTreeSet::new(), options)?;
            timings.cache_restore_seconds = restore_started.elapsed().as_secs_f64();
            timings.selected_files = files.len() as u64;
            timings.selected_bytes = files.iter().map(|file| file.bytes_written).sum();
            timings.unique_payloads = files
                .iter()
                .map(|file| &file.sha256)
                .collect::<BTreeSet<_>>()
                .len() as u64;
            timings.checkpoint_sync_calls = checkpoint_sync.calls.load(Ordering::Relaxed);
            timings.checkpoint_sync_worker_seconds =
                checkpoint_sync.nanoseconds.load(Ordering::Relaxed) as f64 / 1e9;
            return Ok(ExtractionOutcome {
                files,
                cache_entry: entry,
                cache_hit: true,
                timings,
            });
        }
    }
    timings.cache_restore_seconds = restore_started.elapsed().as_secs_f64();
    if let Some(reporter) = &reporter {
        reporter.announce();
    }
    let extraction_started = std::time::Instant::now();
    let sync = SyncMeasurements::default();
    let ingestion = FreshIngestion {
        session,
        archive: session.next_archive.fetch_add(1, Ordering::Relaxed),
        // In archive mode the pack is canonical and durable. Derived blobs need no fsync.
        sync_mode: if options.sync == IngestionSync::PerFile {
            IngestionSync::PerFile
        } else {
            IngestionSync::None
        },
        sync: &sync,
        writes: RawWriteMeasurements::default(),
        used_blobs: Mutex::new(Vec::new()),
    };
    let batches = partition(selected);
    let largest = batches
        .iter()
        .map(|batch| batch.iter().map(|entry| entry.bytes).sum())
        .max()
        .unwrap_or(0);
    // Oversize textures use a single batch, still bounded by the 1 GiB entry
    // limit. Ordinary batches share a 64 MiB decoded-byte budget.
    let budget = Arc::new(ByteBudget {
        limit: QUEUED_BYTES.max(largest),
        used: Mutex::new(0),
        ready: Condvar::new(),
        closed: AtomicBool::new(false),
    });
    let cpu_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.cpu_jobs)
        .build()?;
    let decoded = AtomicUsize::new(0);
    let created_packs = Mutex::new(BTreeSet::new());
    let spill_cache = SpillCache::default();
    let (sender, receiver) = crossbeam_channel::bounded::<Batch>(options.io_jobs);
    let result = std::thread::scope(|scope| -> Result<Vec<ExtractedFile>> {
        let mut writers = Vec::new();
        for _ in 0..options.io_jobs {
            let receiver = receiver.clone();
            let source_hash = &source_hash;
            let reporter = &reporter;
            let created_packs = &created_packs;
            let spill_cache = &spill_cache;
            let ingestion = &ingestion;
            let checkpoint_sync = &checkpoint_sync;
            let writer_budget = Arc::clone(&budget);
            writers.push(scope.spawn(move || -> Result<Vec<ExtractedFile>> {
                let _exit = WriterExit(writer_budget);
                let mut written = Vec::new();
                for batch in receiver.iter() {
                    check_stop(stop)?;
                    if let Some(observer) = observer {
                        observer(WriterEvent::Started);
                    }
                    let result = write_batch(
                        &batch.files,
                        output,
                        cache,
                        source_hash,
                        spill_cache,
                        ingestion,
                        checkpoint_sync,
                        options,
                        reader_version,
                        created_packs,
                        stop,
                        observer,
                        reporter.as_ref(),
                    );
                    if let Some(observer) = observer {
                        observer(WriterEvent::Finished);
                    }
                    written.extend(result?);
                }
                Ok(written)
            }));
        }
        // Do not keep a spare receiver alive: a writer failure must unblock
        // producers rather than leave them waiting on a queue with no consumer.
        drop(receiver);
        let producer_error = Mutex::new(None);
        let produced: std::result::Result<Vec<()>, ()> = cpu_pool.install(|| {
            batches
                .into_par_iter()
                .map(|batch| {
                    let result: Result<()> = (|| {
                        check_stop(stop)?;
                        let reservation = batch.iter().map(|entry| entry.bytes).sum();
                        let permit = budget.reserve(reservation)?;
                        let mut files = Vec::with_capacity(batch.len());
                        for entry in batch {
                            check_stop(stop)?;
                            files.push(prepare(
                                entry,
                                &packed,
                                &expected,
                                cache,
                                previous_cache,
                                &decoded,
                            )?);
                        }
                        sender
                            .send(Batch {
                                files,
                                _permit: permit,
                            })
                            .map_err(|_| WriterQueueStopped)?;
                        Ok(())
                    })();
                    match result {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            if error_priority(&error) == 2
                                && let Some(observer) = observer
                            {
                                observer(WriterEvent::ProducerFailed);
                            }
                            // Rayon may stop collecting after another worker's
                            // cancellation; retain every failure already in flight.
                            retain_error(&mut producer_error.lock().unwrap(), error);
                            Err(())
                        }
                    }
                })
                .collect()
        });
        drop(sender);
        let mut files = Vec::new();
        let mut error = producer_error.into_inner().unwrap();
        for writer in writers {
            match writer.join() {
                Ok(Ok(written)) => files.extend(written),
                Ok(Err(writer_error)) => retain_error(&mut error, writer_error),
                Err(_) => retain_error(
                    &mut error,
                    color_eyre::eyre::eyre!("archive writer panicked"),
                ),
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        debug_assert!(produced.is_ok(), "producer failure must retain its cause");
        Ok(files)
    });
    // Sealed partial batches are reusable even without a completed inventory.
    // Verify their source generation on worker failure or cancellation too.
    if let Err(source_error) = verify_checkpoint_source(
        archive_path,
        &source_hash,
        &created_packs.into_inner().unwrap(),
        options,
    ) {
        return Err(match result {
            Err(worker_error)
                if worker_error
                    .downcast_ref::<Interrupted>()
                    .is_some_and(|stop| stop.cause().is_none()) =>
            {
                Interrupted::after(source_error).into()
            }
            Err(worker_error) => worker_error.wrap_err(format!("{source_error:#}")),
            Ok(_) => source_error,
        });
    }
    let result = result?;
    check_stop(stop)?;
    let mut files = result;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let entry = IngestionCacheEntry {
        source_hash,
        selection: options.selection,
        recipe: options.selection.recipe().to_owned(),
        files: files
            .iter()
            .map(|file| IngestedFile {
                path: file.path.to_string_lossy().replace('\\', "/"),
                size: file.bytes_written,
                hash: file.sha256.clone(),
            })
            .collect(),
        reader_version,
    };
    timings.extraction_seconds = extraction_started.elapsed().as_secs_f64();
    timings.selected_files = files.len() as u64;
    timings.selected_bytes = files.iter().map(|file| file.bytes_written).sum();
    ingestion.record_timings(&mut timings);
    timings.sync_calls = sync.calls.load(Ordering::Relaxed);
    timings.checkpoint_sync_calls = checkpoint_sync.calls.load(Ordering::Relaxed);
    timings.checkpoint_sync_worker_seconds =
        checkpoint_sync.nanoseconds.load(Ordering::Relaxed) as f64 / 1e9;
    timings.sync_worker_seconds = sync.nanoseconds.load(Ordering::Relaxed) as f64 / 1e9;
    let cache_hit = decoded.load(Ordering::Relaxed) == 0
        && (!files.is_empty()
            || previous.is_some_and(|previous| {
                options.reuse_cache
                    && previous.source_hash == entry.source_hash
                    && previous.reader_version == reader_version
                    && previous.covers(options.selection)
            }));
    Ok(ExtractionOutcome {
        timings,
        files,
        cache_entry: entry,
        cache_hit,
    })
}

fn verify_checkpoint_source(
    archive: &Path,
    source_hash: &str,
    created_packs: &BTreeSet<PathBuf>,
    options: &ExtractOptions,
) -> Result<()> {
    let source_error = match hash_file(archive) {
        Ok(actual) if actual == source_hash => return Ok(()),
        Ok(_) => color_eyre::eyre::eyre!(
            "archive inputs changed during extraction; refusing a mixed checkpoint generation"
        ),
        Err(error) => error.wrap_err("cannot verify archive checkpoint source after extraction"),
    };
    let mut directories = BTreeSet::new();
    for pack in created_packs {
        let mut paths = vec![pack.clone()];
        if let Some(checkpoints) = &options.checkpoint_dir {
            let hash = pack.file_stem().unwrap().to_string_lossy();
            paths.push(checkpoints.join(format!(
                "{source_hash}.{}.{}.json",
                options.selection.recipe(),
                hash
            )));
        }
        for path in paths {
            match fs::remove_file(&path) {
                Ok(()) => {
                    directories.insert(path.parent().unwrap().to_owned());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).wrap_err(format!(
                        "{source_error:#}; failed to invalidate new archive checkpoint {}",
                        path.display()
                    ));
                }
            }
        }
    }
    for directory in directories {
        sync_directory(&directory).wrap_err(format!(
            "{source_error:#}; failed to persist checkpoint invalidation"
        ))?;
    }
    Err(source_error)
}

fn partition(entries: Vec<Pending<'_>>) -> Vec<Vec<Pending<'_>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for entry in entries {
        if !current.is_empty()
            && (current.len() >= BATCH_FILES || bytes.saturating_add(entry.bytes) > BATCH_BYTES)
        {
            batches.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += entry.bytes;
        current.push(entry);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn valid_blob(root: &Path, hash: &str, size: u64) -> Option<PathBuf> {
    let path = blob_path(root, hash).ok()?;
    (fs::metadata(&path).is_ok_and(|metadata| metadata.len() == size)
        && hash_file(&path).is_ok_and(|actual| actual == hash))
    .then_some(path)
}

fn prepare(
    pending: Pending<'_>,
    packed: &BTreeMap<PathBuf, PackedFile>,
    expected: &BTreeMap<PathBuf, &IngestedFile>,
    cache: &Path,
    previous_cache: &Path,
    decoded: &AtomicUsize,
) -> Result<Prepared> {
    if let Some(packed) = packed
        .get(&pending.path)
        .filter(|packed| packed.item.size == pending.bytes as u64)
    {
        let existing = valid_blob(cache, &packed.item.sha256, packed.item.size)
            .or_else(|| valid_blob(previous_cache, &packed.item.sha256, packed.item.size));
        let data = if existing.is_none() {
            let mut file = File::open(&packed.pack)?;
            file.seek(SeekFrom::Start(packed.offset))?;
            let mut data = vec![0; pending.bytes];
            file.read_exact(&mut data)?;
            ensure!(
                hash_bytes(&data) == packed.item.sha256,
                "sealed archive pack changed during extraction"
            );
            Some(data)
        } else {
            None
        };
        return Ok(Prepared {
            file: ExtractedFile {
                path: pending.path,
                bytes_written: packed.item.size,
                sha256: packed.item.sha256.clone(),
            },
            data,
            existing,
            durable: true,
        });
    }
    if let Some(expected) = expected
        .get(&pending.path)
        .filter(|file| file.size == pending.bytes as u64)
        && let Some(existing) = valid_blob(previous_cache, &expected.hash, expected.size)
    {
        return Ok(Prepared {
            file: ExtractedFile {
                path: pending.path,
                bytes_written: expected.size,
                sha256: expected.hash.clone(),
            },
            data: None,
            existing: Some(existing),
            durable: false,
        });
    }
    let data = pending.payload.decompress()?;
    ensure!(
        data.len() == pending.bytes,
        "archive entry decoded to an unexpected length"
    );
    decoded.fetch_add(1, Ordering::Relaxed);
    Ok(Prepared {
        file: ExtractedFile {
            path: pending.path,
            bytes_written: data.len() as u64,
            sha256: hash_bytes(&data),
        },
        data: Some(data),
        existing: None,
        durable: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_batch(
    files: &[Prepared],
    output: &Path,
    cache: &Path,
    source_hash: &str,
    _spill_cache: &SpillCache,
    ingestion: &FreshIngestion<'_>,
    checkpoint_sync: &SyncMeasurements,
    options: &ExtractOptions,
    reader_version: u32,
    created_packs: &Mutex<BTreeSet<PathBuf>>,
    stop: Option<StopCheck<'_>>,
    observer: WriterObserver<'_>,
    progress: Option<&ProgressReporter<'_>>,
) -> Result<Vec<ExtractedFile>> {
    let unsealed: Vec<_> = files.iter().filter(|file| !file.durable).collect();
    if !unsealed.is_empty() {
        seal_pack(
            &unsealed,
            cache,
            source_hash,
            options,
            reader_version,
            created_packs,
            checkpoint_sync,
        )?;
        if let Some(observer) = observer {
            observer(WriterEvent::Sealed);
        }
    }
    let mut written = Vec::with_capacity(files.len());
    for prepared in files {
        check_stop(stop)?;
        let data = prepared.bytes()?;
        ensure!(
            data.len() as u64 == prepared.file.bytes_written
                && (prepared.data.is_some() || hash_bytes(&data) == prepared.file.sha256),
            "archive cache payload changed before materialization"
        );
        ingestion.materialize(
            &output.join(&prepared.file.path),
            &data,
            &prepared.file.sha256,
        )?;
        if let Some(progress) = progress {
            progress.advance(prepared.file.bytes_written);
        }
        written.push(prepared.file.clone());
    }
    Ok(written)
}

fn pack_directory(cache: &Path, source_hash: &str, recipe: &str) -> Result<PathBuf> {
    // The same validation as a blob key prevents provenance-controlled paths.
    blob_path(cache, source_hash)?;
    ensure!(
        matches!(
            recipe,
            "all-v1" | "converter-inputs-v4" | "runtime-english-v4"
        ),
        "unknown ingestion recipe"
    );
    Ok(cache.join("batches").join(source_hash).join(recipe))
}

#[allow(clippy::too_many_arguments)]
fn seal_pack(
    files: &[&Prepared],
    cache: &Path,
    source_hash: &str,
    options: &ExtractOptions,
    reader_version: u32,
    created_packs: &Mutex<BTreeSet<PathBuf>>,
    checkpoint_sync: &SyncMeasurements,
) -> Result<PathBuf> {
    let directory = pack_directory(cache, source_hash, options.selection.recipe())?;
    fs::create_dir_all(&directory)?;
    let mut offset = 0u64;
    let mut payloads = BTreeMap::new();
    let mut unique = Vec::new();
    let mut records = Vec::new();
    for prepared in files {
        let key = (prepared.file.sha256.clone(), prepared.file.bytes_written);
        let payload_offset = *payloads.entry(key).or_insert_with(|| {
            let start = offset;
            offset += prepared.file.bytes_written;
            unique.push(*prepared);
            start
        });
        records.push(PackItem {
            path: prepared.file.path.to_string_lossy().replace('\\', "/"),
            size: prepared.file.bytes_written,
            sha256: prepared.file.sha256.clone(),
            offset: payload_offset,
        });
    }
    let index = PackIndex {
        version: PACK_VERSION,
        source_hash: source_hash.to_owned(),
        recipe: options.selection.recipe().to_owned(),
        reader_version,
        files: records,
    };
    let index = serde_json::to_vec(&index)?;
    ensure!(
        index.len() <= MAX_INDEX_BYTES,
        "archive pack index exceeds limit"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    let mut digest = Sha256::new();
    let mut write = |bytes: &[u8]| -> Result<()> {
        temporary.write_all(bytes)?;
        digest.update(bytes);
        Ok(())
    };
    write(PACK_MAGIC)?;
    write(&(index.len() as u64).to_le_bytes())?;
    write(&index)?;
    for prepared in unique {
        let bytes = prepared.bytes()?;
        ensure!(
            bytes.len() as u64 == prepared.file.bytes_written
                // Owned buffers were hashed by prepare and have no mutable
                // aliases. A legacy blob reread here can change between steps.
                && (prepared.data.is_some() || hash_bytes(&bytes) == prepared.file.sha256),
            "archive cache payload changed before checkpoint"
        );
        write(&bytes)?;
    }
    if options.sync != IngestionSync::None {
        checkpoint_sync.flush(temporary.as_file())?;
    }
    let hash: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let destination = directory.join(format!("{hash}.pack"));
    if hash_file(&destination).is_ok_and(|actual| actual == hash) {
        // A verified pre-existing pack belongs to an earlier generation. Do
        // not replace or later invalidate it when this extraction is rejected.
        if options.sync != IngestionSync::None {
            checkpoint_sync.flush(&super::open_raw_payload_for_sync(&destination)?)?;
        }
    } else {
        temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        // Repaired paths contain this generation's newly written bytes too.
        created_packs.lock().unwrap().insert(destination.clone());
    }
    // Flush every newly introduced directory entry through the cache parent.
    if options.sync != IngestionSync::None {
        sync_directory_chain(&directory, cache_directory_anchor(cache))?;
    }
    if let Some(checkpoints) = &options.checkpoint_dir {
        fs::create_dir_all(checkpoints)?;
        let marker = checkpoints.join(format!(
            "{source_hash}.{}.{}.json",
            options.selection.recipe(),
            hash
        ));
        let mut temporary = tempfile::NamedTempFile::new_in(checkpoints)?;
        serde_json::to_writer(
            &mut temporary,
            &serde_json::json!({
                "version": PACK_VERSION, "source_hash": source_hash,
                "reader_version": reader_version,
                "recipe": options.selection.recipe(), "pack_sha256": hash,
            }),
        )?;
        temporary.as_file().sync_all()?;
        temporary.persist(marker).map_err(|error| error.error)?;
        sync_directory(checkpoints)?;
    }
    Ok(destination)
}

fn sync_directory_chain(mut path: &Path, stop: &Path) -> Result<()> {
    loop {
        sync_directory(path)?;
        if directory_path(path) == directory_path(stop) {
            return Ok(());
        }
        path = path.parent().ok_or_else(|| {
            color_eyre::eyre::eyre!("checkpoint directory does not descend from its cache parent")
        })?;
    }
}

fn cache_directory_anchor(cache: &Path) -> &Path {
    // Include the staging/cache parent entry itself, not only its children.
    cache
        .parent()
        .map(|parent| parent.parent().unwrap_or(parent))
        .unwrap_or_else(|| Path::new("."))
}

fn directory_path(path: &Path) -> &Path {
    // Relative CLI outputs have an empty lexical parent, which names the
    // current directory but cannot be passed as an empty path to File::open.
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    let path = directory_path(path);
    #[cfg(unix)]
    {
        File::open(path)?.sync_all().wrap_err_with(|| {
            format!(
                "failed to sync archive checkpoint directory {}",
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        // Rust cannot portably flush a directory handle on Windows. Pack file
        // bytes are synced before rename; lost directory entries cause a cache
        // miss and re-extraction, never acceptance of unchecked derived bytes.
        let _ = path;
        Ok(())
    }
}

pub(crate) fn sync_pack_file(path: &Path) -> Result<()> {
    // FlushFileBuffers requires GENERIC_WRITE on Windows. Unix permits fsync
    // on a read-only handle, so keep read-only caches usable there.
    fs::OpenOptions::new()
        .read(true)
        .write(cfg!(windows))
        .open(path)?
        .sync_all()
        .wrap_err_with(|| format!("failed to sync archive pack {}", path.display()))
}

fn validate_pack_layout(
    index: &PackIndex,
    base: usize,
    bytes_len: usize,
) -> Result<Vec<(PathBuf, usize, usize)>> {
    ensure!(
        index.files.len() <= BATCH_FILES,
        "archive pack file count exceeds limit"
    );
    let mut seen = BTreeMap::new();
    let mut expected_offset = 0u64;
    let mut payloads = BTreeMap::new();
    let mut layout = Vec::with_capacity(index.files.len());
    for item in &index.files {
        let relative = safe_relative_path(&item.path)?;
        detect_archive_collision(&mut seen, &relative, &item.path)?;
        ensure!(
            item.size <= MAX_FILE_BYTES as u64,
            "archive pack payload exceeds limit"
        );
        let key = (item.sha256.clone(), item.size);
        if index.version == 2 && payloads.contains_key(&key) {
            ensure!(
                payloads[&key] == item.offset,
                "archive pack alias has invalid range"
            );
        } else {
            ensure!(
                item.offset == expected_offset,
                "archive pack has invalid payload layout"
            );
            payloads.insert(key, item.offset);
            expected_offset = expected_offset
                .checked_add(item.size)
                .ok_or_else(|| color_eyre::eyre::eyre!("archive pack range overflow"))?;
        }
        let start = base
            .checked_add(usize::try_from(item.offset)?)
            .ok_or_else(|| color_eyre::eyre::eyre!("archive pack range overflow"))?;
        let end = start
            .checked_add(usize::try_from(item.size)?)
            .ok_or_else(|| color_eyre::eyre::eyre!("archive pack range overflow"))?;
        ensure!(end <= bytes_len, "truncated archive pack payload");
        layout.push((relative, start, end));
    }
    ensure!(
        base as u64 + expected_offset == bytes_len as u64,
        "archive pack has trailing or missing bytes"
    );
    Ok(layout)
}

fn read_pack(
    path: &Path,
    source_hash: &str,
    reader_version: u32,
    selection: ArchiveSelection,
) -> Result<Vec<(PathBuf, PackedFile)>> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    ensure!(
        (16..=(MAX_FILE_BYTES + MAX_INDEX_BYTES + 16) as u64).contains(&file_len),
        "archive pack size exceeds limit"
    );
    let bytes = unsafe { Mmap::map(&file) }?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES + MAX_INDEX_BYTES + 16,
        "archive pack exceeds payload limit"
    );
    ensure!(
        bytes.len() >= 16 && &bytes[..8] == PACK_MAGIC,
        "invalid archive pack header"
    );
    let length = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap()))?;
    ensure!(
        length <= MAX_INDEX_BYTES && length <= bytes.len() - 16,
        "invalid archive pack index length"
    );
    let index: PackIndex = serde_json::from_slice(&bytes[16..16 + length])?;
    ensure!(
        matches!(index.version, 1 | PACK_VERSION)
            && index.source_hash == source_hash
            && !index.recipe.is_empty()
            && selection.accepts_recipe(&index.recipe),
        "archive pack recipe or source does not match"
    );
    ensure!(
        index.reader_version == reader_version,
        "archive pack was decoded by a different archive reader"
    );
    let base = 16 + length;
    let layout = validate_pack_layout(&index, base, bytes.len())?;
    let expected_hash = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid archive pack filename"))?;
    ensure!(
        hash_bytes(&bytes) == expected_hash,
        "archive pack digest mismatch"
    );
    let mut packed = Vec::new();
    for (item, (relative, start, end)) in index.files.into_iter().zip(layout) {
        let payload = bytes
            .get(start..end)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated archive pack payload"))?;
        ensure!(
            hash_bytes(payload) == item.sha256,
            "archive pack payload digest mismatch"
        );
        if selection.includes(&relative) {
            packed.push((
                relative,
                PackedFile {
                    pack: path.to_owned(),
                    offset: start as u64,
                    item,
                },
            ));
        }
    }
    Ok(packed)
}

/// Removes known pre-reader-version BSA packs and retired-recipe packs from the writable current cache.
/// Previous generations and malformed or future pack formats remain untouched.
pub(crate) fn prune_stale_reader_packs(
    cache_root: &Path,
    live_sources: &BTreeMap<String, u32>,
) -> Result<()> {
    let batches = cache_root.join("batches");
    if !fs::symlink_metadata(&batches).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(());
    }
    for (source_hash, current_reader) in live_sources {
        if *current_reader != super::BSA_READER_VERSION {
            continue;
        }
        let source_dir = batches.join(source_hash);
        if !fs::symlink_metadata(&source_dir).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            continue;
        }
        for recipe in [
            ArchiveSelection::RuntimeEnglishV1.recipe(),
            ArchiveSelection::ConverterInputsV1.recipe(),
            "runtime-english-v3",
            "converter-inputs-v3",
            "runtime-english-v2",
            "converter-inputs-v2",
            "runtime-english-v1",
            "converter-inputs-v1",
            ArchiveSelection::All.recipe(),
        ] {
            let recipe_dir = source_dir.join(recipe);
            if !fs::symlink_metadata(&recipe_dir)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                continue;
            }
            for entry in fs::read_dir(&recipe_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry
                        .path()
                        .extension()
                        .is_none_or(|extension| extension != "pack")
                {
                    continue;
                }
                let Some(reader_version) = pack_reader_version(&entry.path(), source_hash, recipe)
                else {
                    continue;
                };
                let retired_recipe = matches!(recipe, "runtime-english-v1" | "converter-inputs-v1");
                if reader_version == 0
                    || (retired_recipe && reader_version <= super::BSA_READER_VERSION)
                {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    }
    Ok(())
}

fn pack_reader_version(path: &Path, source_hash: &str, recipe: &str) -> Option<u32> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if !(16..=(MAX_FILE_BYTES + MAX_INDEX_BYTES + 16) as u64).contains(&file_len) {
        return None;
    }
    let bytes = unsafe { Mmap::map(&file) }.ok()?;
    if bytes.len() < 16
        || bytes.len() > MAX_FILE_BYTES + MAX_INDEX_BYTES + 16
        || &bytes[..8] != PACK_MAGIC
    {
        return None;
    }
    let length = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().ok()?)).ok()?;
    if length > MAX_INDEX_BYTES || length > bytes.len() - 16 {
        return None;
    }
    let index: PackIndex = serde_json::from_slice(&bytes[16..16 + length]).ok()?;
    if !matches!(index.version, 1 | PACK_VERSION)
        || index.source_hash != source_hash
        || index.recipe != recipe
    {
        return None;
    }
    let base = 16 + length;
    validate_pack_layout(&index, base, bytes.len()).ok()?;
    Some(index.reader_version)
}

#[allow(clippy::too_many_arguments)]
fn restore_pack_inventory(
    previous: &Path,
    cache: &Path,
    source_hash: &str,
    reader_version: u32,
    selection: ArchiveSelection,
    sync_mode: IngestionSync,
    checkpoint_sync: &SyncMeasurements,
    stop: Option<StopCheck<'_>>,
) -> Result<BTreeMap<PathBuf, PackedFile>> {
    let mut inventory = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for root in [cache, previous] {
        let directory = root.join("batches").join(source_hash);
        if !directory.is_dir() {
            continue;
        }
        for recipe in BTreeSet::from([
            selection.recipe(),
            ArchiveSelection::ConverterInputsV1.recipe(),
            ArchiveSelection::All.recipe(),
        ]) {
            if !selection.accepts_recipe(recipe) {
                continue;
            }
            let directory = directory.join(recipe);
            if !directory.is_dir() {
                continue;
            }
            let mut paths: Vec<_> = fs::read_dir(&directory)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<_>>()?;
            paths.sort();
            for path in paths {
                check_stop(stop)?;
                if path.extension().is_none_or(|extension| extension != "pack")
                    || !seen.insert(path.clone())
                {
                    continue;
                }
                // A crash can leave a partial pack or lose an unsynced directory.
                // Neither certifies bytes; rebuild only the affected inputs.
                let Ok(mut records) = read_pack(&path, source_hash, reader_version, selection)
                else {
                    continue;
                };
                let destination_dir = pack_directory(cache, source_hash, recipe)?;
                let destination = destination_dir.join(path.file_name().unwrap());
                if path != destination {
                    fs::create_dir_all(&destination_dir)?;
                    if read_pack(&destination, source_hash, reader_version, selection).is_err() {
                        link_or_copy(&path, &destination)?;
                    }
                    for (_, packed) in &mut records {
                        packed.pack = destination.clone();
                    }
                }
                // A previous none-sync run proves bytes, not durability. Flush
                // reused packs even when no copy or repair was needed.
                if sync_mode != IngestionSync::None {
                    checkpoint_sync.flush(&super::open_raw_payload_for_sync(&destination)?)?;
                    sync_directory_chain(&destination_dir, cache_directory_anchor(cache))?;
                }
                for (relative, packed) in records {
                    if let Some(old) = inventory.get(&relative) {
                        let old: &PackedFile = old;
                        ensure!(
                            old.item.sha256 == packed.item.sha256
                                && old.item.size == packed.item.size,
                            "archive checkpoints disagree for one source path"
                        );
                    } else {
                        inventory.insert(relative, packed);
                    }
                }
            }
        }
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{ArchiveExtractor, BSA_READER_VERSION};
    use std::sync::{Barrier, atomic::AtomicBool};

    fn fixture(root: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = root.join(name);
        let entries: Vec<_> = entries
            .iter()
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        fs::write(
            &path,
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();
        path
    }

    fn many(root: &Path, count: usize) -> PathBuf {
        let names: Vec<_> = (0..count).map(|i| format!("textures/{i}.dds")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| dummy_content::Entry::new(name, b"DDS payload"))
            .collect();
        let path = root.join("many.bsa");
        fs::write(
            &path,
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();
        path
    }

    fn options() -> ExtractOptions {
        ExtractOptions::converter(2, 2, None)
    }

    #[test]
    fn selection_expansion_cannot_reuse_an_english_only_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "selection.bsa",
            &[
                ("textures/a.dds", b"texture"),
                ("strings/game_english.strings", b"english"),
                ("strings/game_french.strings", b"french"),
                ("notes/readme.txt", b"unused"),
            ],
        );
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let mut options = options();
        options.selection = ArchiveSelection::RuntimeEnglishV1;
        let english = extract(
            &archive, &output, &cache, &cache, None, &options, None, None, None,
        )
        .unwrap();
        assert_eq!(english.files.len(), 2);
        options.selection = ArchiveSelection::ConverterInputsV1;
        let expanded = extract(
            &archive,
            &output,
            &cache,
            &cache,
            Some(&english.cache_entry),
            &options,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(expanded.files.len(), 3);
        assert!(!expanded.cache_hit);
        assert_eq!(
            fs::read(output.join("strings/game_french.strings")).unwrap(),
            b"french"
        );
        options.selection = ArchiveSelection::RuntimeEnglishV1;
        let narrowed = extract(
            &archive,
            &output,
            &cache,
            &cache,
            Some(&expanded.cache_entry),
            &options,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(narrowed.files.len(), 2);
        assert!(narrowed.cache_hit);
        assert!(!options.selection.accepts_recipe("future-unknown-v1"));
    }

    // This checks the extraction outcome with a matching selection. The omitted HKX paths also
    // make the selected-file inventory incomplete, so the unit `covers` test proves the
    // recipe-less cache contract while this test checks that ingestion still extracts both.
    #[test]
    fn recipe_less_filtered_entry_does_not_suppress_hkx_extraction() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "legacy-filtered.bsa",
            &[
                ("strings/game_english.strings", b"english"),
                (
                    "meshes/actors/character/character assets/skeleton.hkx",
                    b"skeleton",
                ),
                ("meshes/actors/character/animations/mt_idle.hkx", b"clip"),
            ],
        );
        let archive_bytes = fs::read(&archive).unwrap();
        let stale = IngestionCacheEntry {
            source_hash: hash_bytes(&archive_bytes),
            // Match the requested selection. HKX entries remain absent from this filtered
            // fixture, so extraction's expected-file coverage still forces a cache miss.
            selection: ArchiveSelection::ConverterInputsV1,
            recipe: String::new(),
            files: vec![IngestedFile {
                path: "strings/game_english.strings".to_owned(),
                size: 7,
                hash: hash_bytes(b"english"),
            }],
            reader_version: archive_reader_version(&archive_bytes),
        };
        let output = directory.path().join("vfs");
        let cache = directory.path().join("cache");
        let result = ArchiveExtractor::extract_batched(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            Some(&stale),
            false,
            &options(),
            None,
            None,
        )
        .unwrap();

        assert!(!result.cache_hit);
        assert!(
            result
                .files
                .iter()
                .any(|file| file.path.ends_with("skeleton.hkx"))
        );
        assert!(
            result
                .files
                .iter()
                .any(|file| file.path.ends_with("mt_idle.hkx"))
        );
        assert_eq!(
            fs::read(output.join("meshes/actors/character/animations/mt_idle.hkx")).unwrap(),
            b"clip"
        );
    }

    #[test]
    fn deduplicated_pack_recovers_aliases_after_all_derived_blobs_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "aliases.bsa",
            &[
                ("textures/a.dds", b"same payload"),
                ("textures/b.dds", b"same payload"),
            ],
        );
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let options = options();
        let cold = extract(
            &archive, &output, &cache, &cache, None, &options, None, None, None,
        )
        .unwrap();
        assert_eq!(cold.timings.payload_writes, 1);
        let sealed = packs(&cache);
        assert_eq!(sealed.len(), 1);
        let bytes = fs::read(&sealed[0]).unwrap();
        let length = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let index: PackIndex = serde_json::from_slice(&bytes[16..16 + length]).unwrap();
        assert_eq!(index.version, 2);
        assert_eq!(index.files[0].offset, index.files[1].offset);
        assert_eq!(bytes.len(), 16 + length + b"same payload".len());
        fs::remove_dir_all(cache.join("sha256")).unwrap();
        fs::remove_dir_all(&output).unwrap();
        let resumed = extract(
            &archive,
            &output,
            &cache,
            &cache,
            Some(&cold.cache_entry),
            &options,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(resumed.cache_hit);
        assert_eq!(resumed.files.len(), 2);
        for name in ["a.dds", "b.dds"] {
            assert_eq!(
                fs::read(output.join("textures").join(name)).unwrap(),
                b"same payload"
            );
        }
    }

    #[test]
    fn changing_none_sync_to_archive_flushes_verified_packs_without_redecoding() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "policy.bsa",
            &[("textures/a.dds", b"payload")],
        );
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let mut options = options();
        options.sync = IngestionSync::None;
        let cold = ArchiveExtractor::extract_batched(
            &archive, &output, &cache, &cache, None, true, &options, None, None,
        )
        .unwrap();
        assert_eq!(cold.timings.checkpoint_sync_calls, 0);
        assert_eq!(cold.timings.sync_calls, 0);
        assert!(
            ArchiveExtractor::extract_batched(
                &archive,
                &output,
                &cache,
                &cache,
                Some(&cold.cache_entry),
                false,
                &options,
                None,
                None
            )
            .is_err()
        );
        options.sync = IngestionSync::Archive;
        let durable = ArchiveExtractor::extract_batched(
            &archive,
            &output,
            &cache,
            &cache,
            Some(&cold.cache_entry),
            true,
            &options,
            None,
            None,
        )
        .unwrap();
        assert!(durable.cache_hit);
        assert!(durable.timings.checkpoint_sync_calls > 0);
        assert_eq!(durable.timings.payload_writes, 0);
        assert_eq!(fs::read(output.join("textures/a.dds")).unwrap(), b"payload");
    }

    #[test]
    fn version_one_pack_remains_readable_after_deduplicated_format_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let data = b"old payload";
        let index = PackIndex {
            version: 1,
            source_hash: source.clone(),
            recipe: ArchiveSelection::ConverterInputsV1.recipe().into(),
            reader_version: 0,
            files: vec![PackItem {
                path: "textures/old.dds".into(),
                size: data.len() as u64,
                sha256: hash_bytes(data),
                offset: 0,
            }],
        };
        let index = serde_json::to_vec(&index).unwrap();
        let mut bytes = Vec::from(PACK_MAGIC.as_slice());
        bytes.extend_from_slice(&(index.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&index);
        bytes.extend_from_slice(data);
        let path = directory
            .path()
            .join(format!("{}.pack", hash_bytes(&bytes)));
        fs::write(&path, &bytes).unwrap();
        let restored = read_pack(&path, &source, 0, ArchiveSelection::RuntimeEnglishV1).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].1.item.sha256, hash_bytes(data));
    }

    fn write_test_pack(root: &Path, source: &str, reader_version: u32, data: &[u8]) -> PathBuf {
        let index = PackIndex {
            version: PACK_VERSION,
            source_hash: source.to_owned(),
            recipe: ArchiveSelection::ConverterInputsV1.recipe().into(),
            reader_version,
            files: vec![PackItem {
                path: "textures/a.dds".into(),
                size: data.len() as u64,
                sha256: hash_bytes(data),
                offset: 0,
            }],
        };
        let index = serde_json::to_vec(&index).unwrap();
        let mut bytes = Vec::from(PACK_MAGIC.as_slice());
        bytes.extend_from_slice(&(index.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&index);
        bytes.extend_from_slice(data);
        let path = root
            .join("batches")
            .join(source)
            .join(ArchiveSelection::ConverterInputsV1.recipe())
            .join(format!("{}.pack", hash_bytes(&bytes)));
        let directory = root
            .join("batches")
            .join(source)
            .join(ArchiveSelection::ConverterInputsV1.recipe());
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, &bytes).unwrap();
        path
    }

    fn pack_path(root: &Path, source: &str, reader_version: u32, data: &[u8]) -> PathBuf {
        write_test_pack(root, source, reader_version, data)
    }

    #[test]
    fn incompatible_pack_metadata_is_rejected_before_full_digest() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let path = pack_path(directory.path(), &source, 0, b"payload");
        let renamed = path.with_file_name("not-the-content-digest.pack");
        fs::rename(&path, &renamed).unwrap();

        let error = match read_pack(
            &renamed,
            &source,
            BSA_READER_VERSION,
            ArchiveSelection::ConverterInputsV1,
        ) {
            Err(error) => error,
            Ok(_) => panic!("stale reader pack unexpectedly restored"),
        };
        assert!(
            format!("{error:#}").contains("different archive reader"),
            "{error:#}"
        );
    }

    #[test]
    fn compatible_pack_still_rejects_a_corrupt_full_digest() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let path = pack_path(directory.path(), &source, BSA_READER_VERSION, b"payload");
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();

        let error = match read_pack(
            &path,
            &source,
            BSA_READER_VERSION,
            ArchiveSelection::ConverterInputsV1,
        ) {
            Err(error) => error,
            Ok(_) => panic!("corrupt pack unexpectedly restored"),
        };
        assert!(format!("{error:#}").contains("digest mismatch"));
    }

    #[test]
    fn pruning_removes_only_old_live_bsa_reader_packs() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let ba2_source = "cd".repeat(32);
        let old_bsa = pack_path(directory.path(), &source, 0, b"old");
        let current_bsa = pack_path(directory.path(), &source, BSA_READER_VERSION, b"current");
        let future_bsa = pack_path(directory.path(), &source, BSA_READER_VERSION + 1, b"future");
        let current_ba2 = pack_path(directory.path(), &ba2_source, 0, b"ba2");
        let malformed = directory
            .path()
            .join("batches")
            .join(&source)
            .join(ArchiveSelection::ConverterInputsV1.recipe())
            .join("unknown.pack");
        fs::write(&malformed, b"unknown future pack").unwrap();

        prune_stale_reader_packs(
            directory.path(),
            &BTreeMap::from([(source, BSA_READER_VERSION), (ba2_source, 0)]),
        )
        .unwrap();

        assert!(!old_bsa.exists());
        assert!(current_bsa.is_file());
        assert!(future_bsa.is_file());
        assert!(current_ba2.is_file());
        assert!(malformed.is_file());
    }

    #[test]
    fn pruning_preserves_v2_reader_policy_after_coverage_advances() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let pairs = [
            "runtime-english-v2",
            "converter-inputs-v2",
            "runtime-english-v3",
            "converter-inputs-v3",
        ]
        .map(|recipe| {
            (
                legacy_recipe_pack(directory.path(), &source, recipe, 0, b"stale reader"),
                legacy_recipe_pack(
                    directory.path(),
                    &source,
                    recipe,
                    BSA_READER_VERSION,
                    b"current reader",
                ),
            )
        });
        prune_stale_reader_packs(
            directory.path(),
            &BTreeMap::from([(source, BSA_READER_VERSION)]),
        )
        .unwrap();
        for (stale, current) in pairs {
            assert!(!stale.exists(), "pre-reader-version v2 packs still retire");
            assert!(
                current.is_file(),
                "v4 coverage does not authorize deleting current-reader packs"
            );
        }
    }

    #[test]
    fn pruning_removes_stale_packs_from_retired_recipe_directories_only() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let retired_stale = legacy_recipe_pack(
            directory.path(),
            &source,
            "converter-inputs-v1",
            0,
            b"stale converter inputs",
        );
        let retired_runtime_stale = legacy_recipe_pack(
            directory.path(),
            &source,
            "runtime-english-v1",
            0,
            b"stale runtime english",
        );
        let retired_current = legacy_recipe_pack(
            directory.path(),
            &source,
            "converter-inputs-v1",
            BSA_READER_VERSION,
            b"current reader",
        );
        let retired_runtime_current = legacy_recipe_pack(
            directory.path(),
            &source,
            "runtime-english-v1",
            BSA_READER_VERSION,
            b"current reader runtime",
        );
        let retired_future = legacy_recipe_pack(
            directory.path(),
            &source,
            "converter-inputs-v1",
            BSA_READER_VERSION + 1,
            b"future reader",
        );
        let mismatched_index = legacy_recipe_pack(
            directory.path(),
            &source,
            "converter-inputs-v1",
            BSA_READER_VERSION,
            b"mismatched recipe",
        );
        let mismatched_index_destination = directory
            .path()
            .join("batches")
            .join(&source)
            .join("runtime-english-v1")
            .join("mismatched.pack");
        fs::rename(&mismatched_index, &mismatched_index_destination).unwrap();
        let malformed = directory
            .path()
            .join("batches")
            .join(&source)
            .join("converter-inputs-v1/malformed.pack");
        fs::write(&malformed, b"malformed retired pack").unwrap();
        let current_recipe_stale = pack_path(directory.path(), &source, 0, b"stale current recipe");
        let current_recipe_live = pack_path(
            directory.path(),
            &source,
            BSA_READER_VERSION,
            b"live current recipe",
        );

        prune_stale_reader_packs(
            directory.path(),
            &BTreeMap::from([(source, BSA_READER_VERSION)]),
        )
        .unwrap();

        assert!(!retired_stale.exists());
        assert!(!retired_runtime_stale.exists());
        assert!(!retired_current.exists());
        assert!(!retired_runtime_current.exists());
        assert!(retired_future.is_file());
        assert!(mismatched_index_destination.is_file());
        assert!(malformed.is_file());
        assert!(!current_recipe_stale.exists());
        assert!(current_recipe_live.is_file());
    }

    fn legacy_recipe_pack(
        root: &Path,
        source: &str,
        recipe: &str,
        reader_version: u32,
        data: &[u8],
    ) -> PathBuf {
        let index = PackIndex {
            version: PACK_VERSION,
            source_hash: source.to_owned(),
            recipe: recipe.to_owned(),
            reader_version,
            files: vec![PackItem {
                path: "textures/a.dds".into(),
                size: data.len() as u64,
                sha256: hash_bytes(data),
                offset: 0,
            }],
        };
        let index = serde_json::to_vec(&index).unwrap();
        let mut bytes = Vec::from(PACK_MAGIC.as_slice());
        bytes.extend_from_slice(&(index.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&index);
        bytes.extend_from_slice(data);
        let path = root
            .join("batches")
            .join(source)
            .join(recipe)
            .join(format!("{}.pack", hash_bytes(&bytes)));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn sealed_packs_restore_only_for_the_reader_that_decoded_them() {
        let directory = tempfile::tempdir().unwrap();
        let source = "ab".repeat(32);
        let data = b"DDS payload";
        // Absent stamps are the pre-version format: BA2 packs still restore at version 0,
        // while BSA packs from before version 1 hold undecoded names and must not.
        let restore = |root: &str, reader_version: u32| {
            restore_pack_inventory(
                &directory.path().join(root),
                &directory.path().join(format!("{root}-cache")),
                &source,
                reader_version,
                ArchiveSelection::ConverterInputsV1,
                IngestionSync::None,
                &SyncMeasurements::default(),
                None,
            )
            .unwrap()
            .len()
        };
        write_test_pack(&directory.path().join("unstamped"), &source, 0, data);
        assert_eq!(restore("unstamped", 0), 1);
        assert_eq!(restore("unstamped", BSA_READER_VERSION), 0);

        write_test_pack(
            &directory.path().join("stamped"),
            &source,
            BSA_READER_VERSION,
            data,
        );
        assert_eq!(restore("stamped", BSA_READER_VERSION), 1);
        assert_eq!(restore("stamped", 0), 0);

        // Version 0 is omitted from new packs, so BA2 pack bytes keep their existing layout.
        let index = PackIndex {
            version: PACK_VERSION,
            source_hash: source.clone(),
            recipe: ArchiveSelection::ConverterInputsV1.recipe().into(),
            reader_version: 0,
            files: Vec::new(),
        };
        assert!(
            !serde_json::to_string(&index)
                .unwrap()
                .contains("reader_version")
        );
    }

    #[test]
    #[cfg(unix)]
    fn source_replacement_invalidates_only_new_or_repaired_checkpoint_packs() {
        // Atomic replacement keeps the mapped inode immutable. In-place writes
        // to the source would violate Mmap's safety contract and are not tested.
        for existing in ["missing", "verified", "corrupt"] {
            let directory = tempfile::tempdir().unwrap();
            let archive = fixture(
                directory.path(),
                "assets.bsa",
                &[("textures/a.dds", b"original")],
            );
            let original = fs::read(&archive).unwrap();
            let replacement = fixture(
                directory.path(),
                "replacement.bsa",
                &[("textures/a.dds", b"modified")],
            );
            let previous_cache = directory.path().join("previous/cache");
            ArchiveExtractor::extract_batched(
                &archive,
                &directory.path().join("previous/vfs"),
                Path::new("unused"),
                &previous_cache,
                None,
                true,
                &options(),
                None,
                None,
            )
            .unwrap();
            let previous_pack = packs(&previous_cache).pop().unwrap();
            let previous_bytes = fs::read(&previous_pack).unwrap();
            let cache = directory.path().join("current/cache");
            let current_pack = cache.join(previous_pack.strip_prefix(&previous_cache).unwrap());
            if existing != "missing" {
                fs::create_dir_all(current_pack.parent().unwrap()).unwrap();
                fs::write(
                    &current_pack,
                    if existing == "verified" {
                        previous_bytes.as_slice()
                    } else {
                        b"corrupt"
                    },
                )
                .unwrap();
            }
            let checkpoints = directory.path().join("checkpoints");
            let mut options = ExtractOptions::converter(1, 1, Some(checkpoints.clone()));
            options.reuse_cache = false;
            let replaced = AtomicBool::new(false);
            let observer = |event| {
                if matches!(event, WriterEvent::Sealed) {
                    fs::rename(&replacement, &archive).unwrap();
                    replaced.store(true, Ordering::Release);
                }
            };
            let error = extract(
                &archive,
                &directory.path().join("current/vfs"),
                &previous_cache,
                &cache,
                None,
                &options,
                None,
                None,
                Some(&observer),
            )
            .unwrap_err();
            assert!(replaced.load(Ordering::Acquire));
            assert!(format!("{error:#}").contains("archive inputs changed"));
            assert_eq!(fs::read(&previous_pack).unwrap(), previous_bytes);
            assert_eq!(packs(&cache).len(), usize::from(existing == "verified"));
            assert_eq!(
                fs::read_dir(&checkpoints).unwrap().count(),
                usize::from(existing == "verified")
            );
            if existing == "verified" {
                assert_eq!(fs::read(&current_pack).unwrap(), previous_bytes);
            }
            fs::write(&archive, original).unwrap();
            options.reuse_cache = true;
            let resumed = ArchiveExtractor::extract_batched(
                &archive,
                &directory.path().join("resumed"),
                &cache,
                &cache,
                None,
                true,
                &options,
                None,
                None,
            )
            .unwrap();
            assert_eq!(resumed.cache_hit, existing == "verified");
            assert_eq!(
                fs::read(directory.path().join("resumed/textures/a.dds")).unwrap(),
                b"original"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn source_verification_invalidates_new_packs_before_returning_a_worker_error() {
        for source_exists in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let archive = fixture(
                directory.path(),
                "assets.bsa",
                &[("textures/a.dds", b"original")],
            );
            let replacement = fixture(
                directory.path(),
                "replacement.bsa",
                &[("textures/a.dds", b"modified")],
            );
            let output = directory.path().join("blocked");
            fs::write(&output, b"not a directory").unwrap();
            let observer = |event| {
                if matches!(event, WriterEvent::Sealed) {
                    fs::rename(&replacement, &archive).unwrap();
                    if !source_exists {
                        fs::remove_file(&archive).unwrap();
                    }
                }
            };
            let cache = directory.path().join("cache");
            let error = extract(
                &archive,
                &output,
                Path::new("unused"),
                &cache,
                None,
                &ExtractOptions::converter(1, 1, None),
                None,
                None,
                Some(&observer),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(if source_exists {
                "archive inputs changed"
            } else {
                "cannot verify archive checkpoint source"
            }));
            assert!(error.downcast_ref::<std::io::Error>().is_some());
            assert!(packs(&cache).is_empty());
        }
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_discards_a_replaced_source_generation() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "assets.bsa",
            &[("textures/a.dds", b"original")],
        );
        let replacement = fixture(
            directory.path(),
            "replacement.bsa",
            &[("textures/a.dds", b"modified")],
        );
        let stopped = AtomicBool::new(false);
        let observer = |event| {
            if matches!(event, WriterEvent::Sealed) {
                fs::rename(&replacement, &archive).unwrap();
                stopped.store(true, Ordering::Release);
            }
        };
        let stop = || stopped.load(Ordering::Acquire);
        let cache = directory.path().join("cache");
        let error = extract(
            &archive,
            &directory.path().join("vfs"),
            Path::new("unused"),
            &cache,
            None,
            &ExtractOptions::converter(1, 1, None),
            None,
            Some(&stop),
            Some(&observer),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("archive inputs changed"));
        assert!(error.downcast_ref::<Interrupted>().is_some());
        let stop = Interrupted::after(error);
        assert!(format!("{:#}", stop.cause().unwrap()).contains("archive inputs changed"));
        assert!(packs(&cache).is_empty());
    }

    fn packs(root: &Path) -> Vec<PathBuf> {
        walkdir::WalkDir::new(root.join("batches"))
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "pack")
            })
            .map(|entry| entry.into_path())
            .collect()
    }

    #[test]
    fn sealing_a_relative_cache_flushes_its_current_directory_ancestor() {
        // Avoid changing the process-wide working directory in parallel tests.
        let directory = tempfile::tempdir_in(".").unwrap();
        let relative = PathBuf::from(directory.path().file_name().unwrap());
        let cache = relative.join("cache");
        let pack_directory = cache.join("batches/source/recipe");
        fs::create_dir_all(&pack_directory).unwrap();
        assert!(cache_directory_anchor(&cache).as_os_str().is_empty());
        sync_directory_chain(&pack_directory, cache_directory_anchor(&cache)).unwrap();
    }

    #[test]
    fn consumer_selection_filters_legacy_inventory_and_keeps_string_tables() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "assets.bsa",
            &[
                ("textures/a.dds", b"DDS texture"),
                ("meshes/a.nif", b"mesh"),
                ("scripts/a.pex", b"script"),
                ("lodsettings/Tamriel.lod", b"origin"),
                ("strings/English.STRINGS", b"strings"),
                ("strings/English.ilstrings", b"dialog"),
                ("strings/English.dlstrings", b"descriptions"),
                ("sound/a.fuz", b"voice"),
                ("docs/a.txt", b"unused"),
            ],
        );
        let old_cache = directory.path().join("old/cache");
        let old = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("old/vfs"),
            Path::new("unused"),
            &old_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(old.files.len(), 9);
        let output = directory.path().join("new/vfs");
        let cache = directory.path().join("new/cache");
        let current = ArchiveExtractor::extract_batched(
            &archive,
            &output,
            &old_cache,
            &cache,
            Some(&old.cache_entry),
            false,
            &options(),
            None,
            None,
        )
        .unwrap();
        assert!(current.cache_hit);
        assert_eq!(current.files.len(), 7);
        assert!(!output.join("sound/a.fuz").exists());
        assert!(!output.join("docs/a.txt").exists());
        assert_eq!(
            current.cache_entry.recipe,
            ArchiveSelection::ConverterInputsV1.recipe()
        );
        assert!(!packs(&cache).is_empty());
        // A filtered inventory must never be treated as proof of a full archive.
        let all = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("all/vfs"),
            &cache,
            &directory.path().join("all/cache"),
            Some(&current.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(
            all.cache_hit,
            "the retained all-files pack, rather than the restricted inventory, proves full coverage"
        );
        assert_eq!(all.files.len(), 9);
    }

    #[test]
    fn corrupt_derived_blob_is_recovered_even_when_legacy_verification_is_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "assets.bsa",
            &[("textures/a.dds", b"correct")],
        );
        let cache = directory.path().join("cache");
        let first = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("first"),
            Path::new("unused"),
            &cache,
            None,
            false,
            &options(),
            None,
            None,
        )
        .unwrap();
        let blob = blob_path(&cache, &first.files[0].sha256).unwrap();
        fs::write(&blob, b"corrupt").unwrap();
        let second = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("second"),
            &cache,
            &cache,
            Some(&first.cache_entry),
            false,
            &options(),
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);
        assert_eq!(
            fs::read(directory.path().join("second/textures/a.dds")).unwrap(),
            b"correct"
        );
        assert_eq!(fs::read(blob).unwrap(), b"correct");
    }

    #[test]
    fn a_sealed_interrupted_batch_resumes_without_archive_completion_or_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let archive = many(directory.path(), 600);
        let cache = directory.path().join("cache");
        let stopped = AtomicBool::new(false);
        let stop = || stopped.load(Ordering::Acquire);
        let observer = |event| {
            if matches!(event, WriterEvent::Sealed) {
                stopped.store(true, Ordering::Release);
            }
        };
        let mut options = options();
        options.cpu_jobs = 1;
        options.io_jobs = 1;
        options.checkpoint_dir = Some(directory.path().join("checkpoints"));
        let error = extract(
            &archive,
            &directory.path().join("first"),
            Path::new("unused"),
            &cache,
            None,
            &options,
            None,
            Some(&stop),
            Some(&observer),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(packs(&cache).len(), 1);
        assert!(!cache.join("sha256").exists());
        // Temporary/invalid checkpoint artifacts cannot certify additional files.
        let pack = packs(&cache).pop().unwrap();
        fs::write(pack.with_file_name("abandoned.partial"), b"partial").unwrap();
        fs::write(
            pack.with_file_name(format!("{}.pack", "f".repeat(64))),
            b"bad pack",
        )
        .unwrap();
        // Marker loss is safe: the sealed packs are the authoritative inventory.
        fs::remove_dir_all(options.checkpoint_dir.as_ref().unwrap()).unwrap();
        let recovered = restore_pack_inventory(
            &cache,
            &cache,
            &hash_file(&archive).unwrap(),
            archive_reader_version(&fs::read(&archive).unwrap()),
            ArchiveSelection::ConverterInputsV1,
            IngestionSync::Archive,
            &SyncMeasurements::default(),
            None,
        )
        .unwrap();
        assert_eq!(recovered.len(), BATCH_FILES);
        let resumed = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("resumed"),
            &cache,
            &cache,
            None,
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        assert_eq!(resumed.files.len(), 600);
        assert!(!resumed.cache_hit); // Only the uncommitted remainder was decoded.
        let repeat = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("repeat"),
            &cache,
            &cache,
            Some(&resumed.cache_entry),
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        assert!(repeat.cache_hit);
        assert_eq!(repeat.cache_entry, resumed.cache_entry);
    }

    #[test]
    fn corrupt_pack_and_missing_blobs_force_reextraction() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "assets.bsa",
            &[("textures/a.dds", b"source bytes")],
        );
        let cache = directory.path().join("cache");
        let first = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("first"),
            Path::new("unused"),
            &cache,
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap();
        let pack = packs(&cache).pop().unwrap();
        let mut bytes = fs::read(&pack).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&pack, bytes).unwrap();
        fs::remove_dir_all(cache.join("sha256")).unwrap();
        let rebuilt = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("rebuilt"),
            &cache,
            &cache,
            Some(&first.cache_entry),
            false,
            &options(),
            None,
            None,
        )
        .unwrap();
        assert!(!rebuilt.cache_hit);
        assert_eq!(
            fs::read(directory.path().join("rebuilt/textures/a.dds")).unwrap(),
            b"source bytes"
        );
    }

    #[test]
    fn overlay_replaces_only_vfs_names_and_preserves_earlier_cache_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let first = fixture(
            directory.path(),
            "first.bsa",
            &[("textures/shared.dds", b"first")],
        );
        let second = fixture(
            directory.path(),
            "second.bsa",
            &[("textures/shared.dds", b"second")],
        );
        let cache = directory.path().join("cache");
        let output = directory.path().join("vfs");
        let first = ArchiveExtractor::extract_batched(
            &first,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap();
        ArchiveExtractor::extract_batched(
            &second,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join("textures/shared.dds")).unwrap(),
            b"second"
        );
        assert_eq!(
            fs::read(blob_path(&cache, &first.files[0].sha256).unwrap()).unwrap(),
            b"first"
        );
    }

    #[test]
    fn both_cold_extraction_and_warm_restore_obey_the_writer_bound() {
        let directory = tempfile::tempdir().unwrap();
        let archive = many(directory.path(), 600);
        let first_cache = directory.path().join("first/cache");
        let options = ExtractOptions::converter(4, 2, None);
        let mut prior = None;
        for run in 0..2 {
            let active = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let started = AtomicUsize::new(0);
            let barrier = Barrier::new(2);
            let observer = |event| match event {
                WriterEvent::Started => {
                    let count = active.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(count, Ordering::AcqRel);
                    if started.fetch_add(1, Ordering::AcqRel) < 2 {
                        barrier.wait();
                    }
                }
                WriterEvent::Finished => {
                    active.fetch_sub(1, Ordering::AcqRel);
                }
                WriterEvent::Sealed | WriterEvent::ProducerFailed => {}
            };
            let cache = if run == 0 {
                first_cache.clone()
            } else {
                directory.path().join("second/cache")
            };
            let result = extract(
                &archive,
                &directory.path().join(format!("vfs{run}")),
                &first_cache,
                &cache,
                prior.as_ref(),
                &options,
                None,
                None,
                Some(&observer),
            )
            .unwrap();
            assert_eq!(peak.load(Ordering::Acquire), options.io_jobs);
            assert_eq!(active.load(Ordering::Acquire), 0);
            assert_eq!(result.cache_hit, run != 0);
            prior = Some(result.cache_entry);
        }
    }

    #[test]
    fn writer_failure_wakes_byte_budget_waiters_without_dropping_existing_reservations() {
        let budget = Arc::new(ByteBudget {
            limit: 1,
            used: Mutex::new(0),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let permit = budget.reserve(1).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&budget);
        let worker = std::thread::spawn(move || {
            sent.send(waiting.reserve(1).is_err()).unwrap();
        });
        budget.close();
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        assert_eq!(*budget.used.lock().unwrap(), 1);
        drop(permit);
        worker.join().unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    #[test]
    fn a_writer_error_unblocks_queued_producers_and_preserves_recoverable_packs() {
        let directory = tempfile::tempdir().unwrap();
        let archive = many(directory.path(), 1200);
        let cache = directory.path().join("cache");
        let output = directory.path().join("blocked");
        fs::write(&output, b"not a directory").unwrap();
        let mut options = options();
        options.cpu_jobs = 4;
        options.io_jobs = 1;
        let (sent, received) = std::sync::mpsc::channel();
        let task_archive = archive.clone();
        let task_cache = cache.clone();
        let worker = std::thread::spawn(move || {
            sent.send(
                ArchiveExtractor::extract_batched(
                    &task_archive,
                    &output,
                    Path::new("unused"),
                    &task_cache,
                    None,
                    true,
                    &options,
                    None,
                    None,
                )
                .is_err(),
            )
            .unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        );
        worker.join().unwrap();
        assert_eq!(packs(&cache).len(), 1);
        let resumed = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("resumed"),
            &cache,
            &cache,
            None,
            true,
            &self::options(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(resumed.files.len(), 1200);
    }

    #[test]
    fn explicit_invalidation_decodes_even_when_sealed_packs_and_blobs_are_valid() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "assets.bsa",
            &[("textures/a.dds", b"source")],
        );
        let cache = directory.path().join("cache");
        let first = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("first"),
            Path::new("unused"),
            &cache,
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap();
        let mut options = options();
        options.reuse_cache = false;
        let fresh = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("fresh"),
            &cache,
            &cache,
            Some(&first.cache_entry),
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        assert!(!fresh.cache_hit);
        assert_eq!(fresh.cache_entry, first.cache_entry);
    }

    #[test]
    fn warm_duplicate_payloads_across_batches_share_one_atomic_blob() {
        let directory = tempfile::tempdir().unwrap();
        let archive = many(directory.path(), 1025);
        let first_cache = directory.path().join("first/cache");
        let options = ExtractOptions::converter(8, 4, None);
        let first = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("first/vfs"),
            Path::new("unused"),
            &first_cache,
            None,
            true,
            &options,
            None,
            None,
        )
        .unwrap();
        let second_cache = directory.path().join("second/cache");
        let output = directory.path().join("second/vfs");
        let second = ArchiveExtractor::extract_batched(
            &archive,
            &output,
            &first_cache,
            &second_cache,
            Some(&first.cache_entry),
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);
        assert_eq!(second.files.len(), 1025);
        let hash = &first.files[0].sha256;
        assert_eq!(
            hash_file(&blob_path(&second_cache, hash).unwrap()).unwrap(),
            *hash
        );
        for file in &second.files {
            assert_eq!(fs::read(output.join(&file.path)).unwrap(), b"DDS payload");
        }
        // Publishing the second cache never mutates the earlier pack's links.
        assert_eq!(
            hash_file(&blob_path(&first_cache, hash).unwrap()).unwrap(),
            *hash
        );
    }

    #[test]
    fn filtered_extraction_rejects_unsafe_unused_paths_before_materialization() {
        let directory = tempfile::tempdir().unwrap();
        let archive = fixture(
            directory.path(),
            "unsafe.bsa",
            &[("xx/unused.txt", b"escape")],
        );
        // Fixture generators deliberately reject traversal. Mutate only the
        // same-width folder name so the archive metadata remains parseable.
        let mut bytes = fs::read(&archive).unwrap();
        let folder = bytes
            .windows(3)
            .position(|window| window == b"xx\0")
            .unwrap();
        bytes[folder..folder + 2].copy_from_slice(b"..");
        // The folder hash is verified before the path is validated; store the hash of the
        // mutated name so the walk reaches the unsafe-path check this test is about.
        let hash = crate::archive::bsa_name::folder_hash(b"..");
        bytes[36..44].copy_from_slice(&hash.to_le_bytes());
        fs::write(&archive, bytes).unwrap();
        let error = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("vfs"),
            Path::new("unused"),
            &directory.path().join("cache"),
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsafe path"));
    }

    #[test]
    fn a_stop_keeps_a_real_worker_error_in_every_collection_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut retained = None;
            for kind in order {
                let error: color_eyre::Report = match kind {
                    0 => WriterQueueStopped.into(),
                    1 => Interrupted::new().into(),
                    _ => std::io::Error::from(std::io::ErrorKind::PermissionDenied).into(),
                };
                retain_error(&mut retained, error);
            }
            let stop = Interrupted::after(retained.unwrap());
            assert_eq!(
                stop.cause()
                    .unwrap()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::PermissionDenied,
            );
        }
        // Queue errors caused by a plain cancellation are not user-facing causes.
        let mut retained = None;
        retain_error(&mut retained, WriterQueueStopped.into());
        retain_error(&mut retained, Interrupted::new().into());
        retain_error(&mut retained, WriterQueueStopped.into());
        assert!(Interrupted::after(retained.unwrap()).cause().is_none());
    }

    #[test]
    fn a_cancelled_writer_keeps_an_in_flight_producer_decompression_error() {
        let directory = tempfile::tempdir().unwrap();
        let names: Vec<_> = (0..512)
            .map(|index| format!("textures/{index}.dds"))
            .collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| dummy_content::Entry::new(name, b"DDS payload"))
            .collect();
        let mut bytes =
            dummy_content::ba2::general(&entries, dummy_content::ba2::Compression::Zlib).unwrap();
        let broken = 24 + BATCH_FILES * 36;
        let offset =
            u64::from_le_bytes(bytes[broken + 16..broken + 24].try_into().unwrap()) as usize;
        let length =
            u32::from_le_bytes(bytes[broken + 24..broken + 28].try_into().unwrap()) as usize;
        // Keep the declared size and namespace valid; only decoding fails.
        bytes[offset..offset + length].fill(0xff);
        let archive = directory.path().join("bad-payload.ba2");
        fs::write(&archive, bytes).unwrap();

        let gate = (Mutex::new((false, false)), Condvar::new());
        let stopped = AtomicBool::new(false);
        let stop = || stopped.load(Ordering::Acquire);
        let observer = |event| match event {
            WriterEvent::Started => {
                let mut state = gate.0.lock().unwrap();
                state.0 = true;
                gate.1.notify_all();
                let (state, _) = gate
                    .1
                    .wait_timeout_while(state, std::time::Duration::from_secs(10), |state| !state.1)
                    .unwrap();
                assert!(
                    state.1,
                    "producer did not fail while the writer was in flight"
                );
            }
            WriterEvent::ProducerFailed => {
                let state = gate.0.lock().unwrap();
                let (mut state, _) = gate
                    .1
                    .wait_timeout_while(state, std::time::Duration::from_secs(10), |state| !state.0)
                    .unwrap();
                assert!(state.0, "valid producer did not start its writer");
                stopped.store(true, Ordering::Release);
                state.1 = true;
                gate.1.notify_all();
            }
            _ => {}
        };
        let error = extract(
            &archive,
            &directory.path().join("vfs"),
            Path::new("unused"),
            &directory.path().join("cache"),
            None,
            &ExtractOptions::converter(2, 1, None),
            None,
            Some(&stop),
            Some(&observer),
        )
        .unwrap_err();
        assert!(stopped.load(Ordering::Acquire));
        let stop = Interrupted::after(error);
        assert!(
            format!("{:#}", stop.cause().unwrap())
                .contains("failed to decompress BA2 payload: textures/256.dds")
        );
    }

    #[test]
    fn compressed_ba2_inputs_decode_on_cpu_without_decoding_filtered_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let texture = b"DDS payload repeated DDS payload repeated DDS payload repeated";
        let entries = [
            dummy_content::Entry::new("textures/a.dds", texture),
            dummy_content::Entry::new("sound/ignored.wav", b"compressed unused audio"),
        ];
        let mut bytes =
            dummy_content::ba2::general(&entries, dummy_content::ba2::Compression::Zlib).unwrap();
        // Corrupt only the filtered member's compressed checksum. Its metadata
        // stays valid, but eagerly decoding every payload would reject it.
        let second = 24 + 36;
        let offset =
            u64::from_le_bytes(bytes[second + 16..second + 24].try_into().unwrap()) as usize;
        let length =
            u32::from_le_bytes(bytes[second + 24..second + 28].try_into().unwrap()) as usize;
        bytes[offset + length - 1] ^= 1;
        let archive = directory.path().join("compressed.ba2");
        fs::write(&archive, bytes).unwrap();
        let output = directory.path().join("vfs");
        let result = ArchiveExtractor::extract_batched(
            &archive,
            &output,
            Path::new("unused"),
            &directory.path().join("cache"),
            None,
            true,
            &options(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(fs::read(output.join("textures/a.dds")).unwrap(), texture);
        assert!(!output.join("sound/ignored.wav").exists());
    }

    /// Explicit real-data timing; fixture creation and retail decompression are
    /// outside the reported extraction intervals. Run this test binary under
    /// `/usr/bin/time -l` to capture process CPU and physical I/O separately.
    #[test]
    #[ignore = "requires EXTRACTION_BENCH_SOURCE and an idle conversion host"]
    fn real_entry_extraction_timing() {
        let source = PathBuf::from(
            std::env::var_os("EXTRACTION_BENCH_SOURCE")
                .expect("set EXTRACTION_BENCH_SOURCE to the real Skyrim - Misc.bsa"),
        );
        let base = std::env::var_os("EXTRACTION_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/extraction-bench"));
        fs::create_dir_all(&base).unwrap();
        let directory = tempfile::tempdir_in(&base).unwrap();
        let source_file = File::open(&source).unwrap();
        let source_bytes = unsafe { Mmap::map(&source_file) }.unwrap();
        let raw = bsa::iter_raw_entries(&source_bytes).unwrap();
        let sample: Vec<_> = raw
            .iter()
            .take(2048)
            .map(|entry| {
                (
                    safe_relative_path(&entry.name)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    entry.decompress().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            sample.len(),
            2048,
            "benchmark source must contain at least 2048 entries"
        );
        let entries: Vec<_> = sample
            .iter()
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        let archive = directory.path().join("real-sample.bsa");
        fs::write(
            &archive,
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();
        let legacy_cache = directory.path().join("legacy/cache");
        let legacy_start = std::time::Instant::now();
        let legacy = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("legacy/vfs"),
            Path::new("unused"),
            &legacy_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let legacy_seconds = legacy_start.elapsed().as_secs_f64();

        let mut options = ExtractOptions::converter(4, 4, None);
        options.selection = ArchiveSelection::All;
        options.reuse_cache = false;
        let batch_cache = directory.path().join("batched/cache");
        let batch_start = std::time::Instant::now();
        let batch = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("batched/vfs"),
            Path::new("unused"),
            &batch_cache,
            None,
            true,
            &options,
            None,
            None,
        )
        .unwrap();
        let batch_seconds = batch_start.elapsed().as_secs_f64();
        let inventory = |outcome: &ExtractionOutcome| {
            let mut files: Vec<_> = outcome
                .files
                .iter()
                .map(|file| (file.path.clone(), file.bytes_written, file.sha256.clone()))
                .collect();
            files.sort();
            files
        };
        let expected = inventory(&legacy);
        assert_eq!(inventory(&batch), expected);
        assert!(!batch.cache_hit);

        options.reuse_cache = true;
        let warm_cache = directory.path().join("warm/cache");
        let warm_start = std::time::Instant::now();
        let warm = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("warm/vfs"),
            &batch_cache,
            &warm_cache,
            Some(&batch.cache_entry),
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        let warm_seconds = warm_start.elapsed().as_secs_f64();
        assert_eq!(inventory(&warm), expected);
        assert!(warm.cache_hit);

        let damaged = &warm.files[0];
        let blob = blob_path(&warm_cache, &damaged.sha256).unwrap();
        fs::write(&blob, vec![0xff; damaged.bytes_written as usize]).unwrap();
        let repair_start = std::time::Instant::now();
        let repaired = ArchiveExtractor::extract_batched(
            &archive,
            &directory.path().join("repaired/vfs"),
            &warm_cache,
            &directory.path().join("repaired/cache"),
            Some(&warm.cache_entry),
            false,
            &options,
            None,
            None,
        )
        .unwrap();
        let repair_seconds = repair_start.elapsed().as_secs_f64();
        assert_eq!(inventory(&repaired), expected);
        assert!(repaired.cache_hit);
        for file in &repaired.files {
            assert_eq!(
                hash_file(&directory.path().join("repaired/vfs").join(&file.path)).unwrap(),
                file.sha256
            );
        }
        let report = serde_json::json!({
            "source": source, "source_sha256": hash_file(&source).unwrap(),
            "sample_fixture_sha256": hash_file(&archive).unwrap(),
            "sample_entries": sample.len(),
            "sample_decoded_bytes": sample.iter().map(|(_, data)| data.len()).sum::<usize>(),
            "cpu_jobs": options.cpu_jobs, "io_jobs": options.io_jobs,
            "selection": "all-v1", "checkpoint_markers": false,
            "wall_seconds": { "legacy_per_file_sync": legacy_seconds,
                "durable_batches_cold": batch_seconds, "durable_batches_warm": warm_seconds,
                "corrupted_blob_restore": repair_seconds },
            "cold_speedup": legacy_seconds / batch_seconds,
            "sealed_packs": packs(&batch_cache).len(),
            "inventories_equal": true, "restored_file_hashes_verified": true,
            "limitations": ["First 2048 real entries rewritten into an uncompressed disposable fixture",
                "Preparation excluded from per-run wall times", "Use external /usr/bin/time -l for CPU and physical I/O",
                "Derived corruption deliberately modifies a shared blob in disposable test caches only"],
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        if let Some(path) = std::env::var_os("EXTRACTION_BENCH_REPORT") {
            let path = PathBuf::from(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
    }
}
