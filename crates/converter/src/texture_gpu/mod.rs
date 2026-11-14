//! GPU texture encoder: DDS or authored RGBA mips -> UASTC KTX2 with a wgpu compute shader
//! (`uastc_encode.wgsl`), producing the same container and format as the CPU
//! Basis Universal path in `texture.rs`. The converter uses it for the
//! textures it neither preserves as native blocks nor has to encode on the
//! CPU (`takes`); levels get the same Zstandard supercompression afterwards.
//!
//! The pipeline is built so that the GPU, not the CPU, is the limit:
//! - Reader threads only read files and parse DDS headers
//!   (`PreparedTexture::from_dds`). 8-bit BGR(A/X) data is uploaded as stored;
//!   other formats are decoded to RGBA on the reader thread. Queued textures
//!   are bounded by their bytes (`job_channel`), not their count.
//! - One batcher thread streams each texture into a free, reusable GPU slot as
//!   it arrives; a full slot is encoded in a few dispatches over every 4x4
//!   block of every mip and face of many unrelated textures (one GPU thread
//!   per block), each small enough to stay clear of GPU watchdogs.
//! - A readback thread waits for finished slots and builds the KTX2 files on
//!   its own thread pool, then hands the slot back; writer threads validate
//!   and hand them to the caller. With `SLOTS` slots, filling, GPU work and
//!   post-processing all overlap.
//!
//! A GPU that fails (an error outside the encoder's error scopes, or a lost
//! device) is not used again: every texture still queued, in flight or sent
//! later comes back as an error, which callers answer with the CPU encoder.
//!
//! # Third-party notices
//!
//! Parts of this module are ports of, or data from, other projects:
//! - **Basis Universal** (Copyright (C) 2019-2021 Binomial LLC, Apache-2.0,
//!   <https://github.com/BinomialLLC/basis_universal>): the UASTC block
//!   layout and mode properties, the common partition patterns and anchors,
//!   the ASTC endpoint unquantization parameters (`astc_tables.rs`), the BC7
//!   transcode simulation (p-bit selection, weight mappings) and the mode
//!   selection thresholds in `uastc_encode.wgsl`.
//! - **ComputeASTC** (Copyright (c) 2021 niedap, MIT,
//!   <https://github.com/niepp/astc_encoder>): the PCA-axis-plus-projection
//!   endpoint fit the encoder is modelled on.

mod astc_tables;
use crate::texture_ktx2 as ktx2;

use crate::texture::{
    TextureEncoding, decode_packed_rgba8_mips, has_tight_packed_rows, inspect_ktx2, max_mip_levels,
    preserves_native_blocks, supercompress_ktx2_levels,
};
use color_eyre::{
    Report, Result,
    eyre::{ensure, eyre},
};
use crossbeam_channel::{Receiver, Sender};
use ddsfile::{Caps2, D3DFormat, Dds, Header, Header10, MiscFlag};
use rayon::prelude::*;
use std::{
    fs::File,
    future::Future,
    io::{Cursor, Read},
    path::Path,
    pin::pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

const WORKGROUP_SIZE: u32 = 64;
/// Blocks encoded per dispatch. Each dispatch is its own submission, so no
/// single one runs long enough to trip a GPU watchdog (TDR on Windows, the
/// kernel driver's job timeout on Linux) on a slower GPU.
const DISPATCH_BLOCKS: u32 = 1 << 18;
const _: () = assert!(DISPATCH_BLOCKS / WORKGROUP_SIZE <= 65_535);
const IMAGE_DESC_BYTES: usize = 32;
/// Reusable upload/dispatch/readback slots; one being filled, the others on
/// the GPU or being copied out.
const SLOTS: usize = 3;
/// Images a default slot has room for.
const SLOT_IMAGES: u64 = 16 * 1024;
/// Source bytes per 4x4 block of the densest format the shader reads (BGR8).
const DENSEST_BLOCK_BYTES: u64 = 48;
/// Blocks a default slot has room for beyond its densest source: small mips
/// round up to whole blocks.
const SLOT_SPARE_BLOCKS: u64 = 1024;
/// Batches' worth of source bytes allowed to wait for the batcher.
const QUEUED_BATCHES: u64 = 2;
/// Built KTX2 bytes allowed to wait for the writer threads.
const PENDING_WRITE_BYTES: u64 = 2 << 30;
/// Bytes per UASTC 4x4 block.
const UASTC_BLOCK_BYTES: usize = 16;
/// Default refinement passes; the author's out-of-tree benchmark put q2
/// within ~0.4 dB of the CPU encoder's UASTC level 2 at ~1/130 of the time.
pub const DEFAULT_QUALITY: u32 = 2;
/// Source (DDS) megabytes packed into one GPU batch.
pub const DEFAULT_BATCH_MB: u64 = 256;
/// Changes whenever the shader's output does, so cached GPU encodings from an
/// older shader are converted again.
const ENCODER_VERSION: u32 = 1;

/// Suffix of the cache hash of textures the GPU encodes at `quality`.
pub(crate) fn cache_label(quality: u32) -> String {
    format!(":gpu-uastc-v{ENCODER_VERSION}-q{quality}")
}

/// How an image's bytes are stored; decoded by the shader. Values match the
/// `FMT_*` constants in `uastc_encode.wgsl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum SourceFormat {
    Rgba8 = 0,
    Bgra8 = 1,
    /// X8R8G8B8: alpha is forced to 255.
    Bgrx8 = 2,
    Bgr8 = 3,
}

impl SourceFormat {
    /// Bytes per texel.
    fn texel_bytes(self) -> u32 {
        match self {
            Self::Bgr8 => 3,
            _ => 4,
        }
    }
}

/// One mip level of one face, as stored in `PreparedTexture::upload`.
#[derive(Debug, Clone)]
pub(crate) struct SourceImage {
    width: u32,
    height: u32,
    pub(crate) format: SourceFormat,
    /// Byte offset in the texture's upload bytes.
    offset: usize,
    /// Bytes per texel row.
    pitch: u32,
}

impl SourceImage {
    /// Number of 4x4 blocks covering the image (partial blocks count as whole).
    fn block_count(&self) -> usize {
        self.width.div_ceil(4) as usize * self.height.div_ceil(4) as usize
    }

    /// Bytes the image occupies in the source data, or `None` when the size
    /// does not fit (dimensions come straight from an untrusted header).
    fn byte_len(&self) -> Option<usize> {
        (self.pitch as usize).checked_mul(self.height as usize)
    }
}

/// Where each face's mip chain sits in DDS data of a format the shader
/// decodes: the images in DDS order (face after face) and the total size.
/// `None` when the header's dimensions make any size overflow; such a file
/// cannot be valid and is left to the CPU path to report.
fn stored_layout(
    format: SourceFormat,
    width: u32,
    height: u32,
    faces: u32,
    mips: usize,
) -> Option<(Vec<SourceImage>, usize)> {
    let mut images = Vec::with_capacity(faces as usize * mips);
    let mut offset = 0usize;
    for _ in 0..faces {
        for mip in 0..mips {
            let (w, h) = (
                width.checked_shr(mip as u32).unwrap_or(0).max(1),
                height.checked_shr(mip as u32).unwrap_or(0).max(1),
            );
            let image = SourceImage {
                width: w,
                height: h,
                format,
                offset,
                pitch: w.checked_mul(format.texel_bytes())?,
            };
            offset = offset.checked_add(image.byte_len()?)?;
            images.push(image);
        }
    }
    Some((images, offset))
}

/// A texture ready for the GPU: its bytes as uploaded plus where each image
/// lives in them.
pub(crate) struct PreparedTexture {
    width: u32,
    height: u32,
    faces: u32,
    /// `images[mip * faces + face]`.
    pub(crate) images: Vec<SourceImage>,
    /// Exactly the bytes to upload, so the queue's byte budget counts
    /// everything a queued texture holds.
    bytes: Vec<u8>,
}

impl PreparedTexture {
    /// Prepares an authored 2D RGBA mip chain without regenerating its mips.
    /// The caller can retain the original levels for CPU fallback.
    pub(crate) fn from_rgba_mips(width: u32, height: u32, levels: &[Vec<u8>]) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "RGBA texture has an empty dimension"
        );
        ensure!(!levels.is_empty(), "RGBA mip chain is empty");
        ensure!(
            levels.len() <= max_mip_levels(width, height, 1) as usize,
            "RGBA mip chain exceeds its dimensions"
        );
        let mut images = Vec::with_capacity(levels.len());
        let mut total = 0usize;
        for (mip, rgba) in levels.iter().enumerate() {
            let mip_width = (width >> mip).max(1);
            let mip_height = (height >> mip).max(1);
            let pitch = mip_width
                .checked_mul(4)
                .ok_or_else(|| eyre!("RGBA row size overflows"))?;
            let len = (pitch as usize)
                .checked_mul(mip_height as usize)
                .ok_or_else(|| eyre!("RGBA mip size overflows"))?;
            ensure!(
                rgba.len() == len,
                "RGBA mip {mip} has an invalid byte length"
            );
            images.push(SourceImage {
                width: mip_width,
                height: mip_height,
                format: SourceFormat::Rgba8,
                offset: total,
                pitch,
            });
            total = total
                .checked_add(len)
                .ok_or_else(|| eyre!("RGBA mip chain size overflows"))?;
        }
        ensure!(
            total <= isize::MAX as usize,
            "RGBA mip chain exceeds allocation limits"
        );
        let mut bytes = Vec::with_capacity(total);
        for level in levels {
            bytes.extend_from_slice(level);
        }
        Ok(Self {
            width,
            height,
            faces: 1,
            images,
            bytes,
        })
    }

    /// Bytes to upload (the DDS payload, or RGBA for CPU-decoded formats).
    pub(crate) fn upload(&self) -> &[u8] {
        &self.bytes
    }

    /// Frees the upload bytes once they are on their way to the GPU; the
    /// layout is all the readback needs.
    fn release_upload(&mut self) {
        self.bytes = Vec::new();
    }

    /// Number of 4x4 blocks across every mip and face.
    fn block_count(&self) -> usize {
        self.images.iter().map(SourceImage::block_count).sum()
    }

    /// Takes ownership of a whole DDS file. Formats the shader decodes are
    /// referenced in place; anything else `image_dds` can read is decoded to
    /// RGBA on the CPU. Volumes and arrays return an error (callers use the
    /// CPU converter for them).
    pub(crate) fn from_dds(mut bytes: Vec<u8>, encoding: TextureEncoding) -> Result<Self> {
        let dds = read_header(&bytes)?;
        let faces = gpu_faces(&dds, encoding)?;
        let data_start = 4 + 124 + if dds.header10.is_some() { 20 } else { 0 };
        ensure!(bytes.len() >= data_start, "truncated DDS header");
        let (width, height) = (dds.get_width(), dds.get_height());
        let mips = dds.get_num_mipmap_levels().max(1) as usize;
        if let Some(format) = gpu_format(&dds)
            && let Some((face_major, len)) = stored_layout(format, width, height, faces, mips)
            // A truncated payload is left to the CPU decoder, which decides what is usable.
            && let Some(data_end) = data_start.checked_add(len)
            && data_end <= bytes.len()
            // 24-bit rows may be DWORD-aligned and a 32-bit mip 0 may be padded
            // to a larger header pitch, neither of which `stored_layout` models:
            // a chain is read in place only when the CPU decoder would read it
            // with tight rows; the rest is decoded on the CPU. Cubemaps of the
            // 32-bit formats are always read in place (the CPU packed path does
            // not take them).
            && ((faces > 1 && format != SourceFormat::Bgr8)
                || has_tight_packed_rows(&dds, bytes.len() - data_start))
        {
            // KTX2 wants mip-major order: every face of mip 0 first.
            let images = (0..mips)
                .flat_map(|mip| {
                    let face_major = &face_major;
                    (0..faces as usize).map(move |face| face_major[face * mips + mip].clone())
                })
                .collect();
            // Keep only the payload: the header is not uploaded, and data
            // after the payload would otherwise sit in the queue unbudgeted.
            bytes.truncate(data_end);
            bytes.drain(..data_start);
            bytes.shrink_to_fit();
            return Ok(Self {
                width,
                height,
                faces,
                images,
                bytes,
            });
        }
        let decoded = decode_dds_rgba(&bytes)?;
        let mut rgba = Vec::with_capacity(decoded.pixels.iter().map(|p| p.2.len()).sum());
        let mut images = Vec::with_capacity(decoded.pixels.len());
        for (width, height, pixels) in &decoded.pixels {
            images.push(SourceImage {
                width: *width,
                height: *height,
                format: SourceFormat::Rgba8,
                offset: rgba.len(),
                pitch: width * 4,
            });
            rgba.extend_from_slice(pixels);
        }
        Ok(Self {
            width: decoded.width,
            height: decoded.height,
            faces: decoded.faces,
            images,
            bytes: rgba,
        })
    }
}

/// What the DDS header alone says about the GPU path: an error for textures
/// that are copied natively or need the CPU encoder (volumes, arrays,
/// impossible mip counts), otherwise the face count.
fn gpu_faces(dds: &Dds, encoding: TextureEncoding) -> Result<u32> {
    ensure!(
        !preserves_native_blocks(dds, encoding),
        "native block formats are copied, not encoded"
    );
    ensure!(dds.get_depth() <= 1, "volume textures use the CPU encoder");
    ensure!(
        dds.get_width() > 0 && dds.get_height() > 0,
        "DDS has an empty dimension"
    );
    ensure!(
        dds.get_num_mipmap_levels() <= max_mip_levels(dds.get_width(), dds.get_height(), 1),
        "DDS declares more mip levels than its dimensions allow"
    );
    let faces = if is_cubemap(dds) { 6 } else { 1 };
    ensure!(
        faces == 1 || dds.get_num_array_layers() == 6,
        "cubemap does not contain six faces"
    );
    ensure!(
        faces == 6 || dds.get_num_array_layers() <= 1,
        "texture arrays are not supported"
    );
    Ok(faces)
}

/// Whether the GPU path takes this DDS, judged from its header with the same
/// checks `PreparedTexture::from_dds` applies. Natively preserved textures,
/// volumes and arrays are not taken; an unreadable file is not either and is
/// reported by the conversion itself.
pub(crate) fn takes(source: &Path, encoding: TextureEncoding) -> bool {
    // 4-byte magic + 124-byte header + 20-byte DX10 extension.
    let mut header = [0u8; 148];
    let Ok(read) = File::open(source).and_then(|mut file| {
        let mut filled = 0;
        while filled < header.len() {
            match file.read(&mut header[filled..])? {
                0 => break,
                count => filled += count,
            }
        }
        Ok(filled)
    }) else {
        return false;
    };
    read_header(&header[..read]).is_ok_and(|dds| gpu_faces(&dds, encoding).is_ok())
}

/// Parses the DDS header (and DX10 extension) without copying the pixel data.
fn read_header(bytes: &[u8]) -> Result<Dds> {
    ensure!(
        bytes.len() >= 128 && bytes[..4] == *b"DDS ",
        "not a DDS file"
    );
    let mut cursor = Cursor::new(&bytes[4..]);
    let header = Header::read(&mut cursor).map_err(|error| eyre!("invalid DDS header: {error}"))?;
    let header10 = if header.spf.fourcc == Some(ddsfile::FourCC(ddsfile::FourCC::DX10)) {
        Some(Header10::read(&mut cursor).map_err(|error| eyre!("invalid DX10 header: {error}"))?)
    } else {
        None
    };
    Ok(Dds {
        header,
        header10,
        data: Vec::new(),
    })
}

/// Formats the shader reads as stored, interpreted exactly as `image_dds`
/// (and the CPU converter's X8R8G8B8 path) interpret them. DXT1-DXT5 never
/// get here: the converter always copies their blocks natively.
fn gpu_format(dds: &Dds) -> Option<SourceFormat> {
    if dds.header10.is_some() {
        return None;
    }
    Some(match dds.get_d3d_format()? {
        D3DFormat::A8R8G8B8 => SourceFormat::Bgra8,
        D3DFormat::X8R8G8B8 => SourceFormat::Bgrx8,
        D3DFormat::A8B8G8R8 => SourceFormat::Rgba8,
        D3DFormat::R8G8B8 => SourceFormat::Bgr8,
        _ => return None,
    })
}

/// Whether the DDS is a six-face cubemap.
fn is_cubemap(dds: &Dds) -> bool {
    dds.header.caps2.contains(Caps2::CUBEMAP)
        || dds
            .header10
            .as_ref()
            .is_some_and(|header| header.misc_flag.contains(MiscFlag::TEXTURECUBE))
}

/// Every face and mip of a 2D or cubemap DDS as RGBA8, as
/// `pixels[mip * faces + face]` = (width, height, rgba).
struct DecodedTexture {
    width: u32,
    height: u32,
    faces: u32,
    pixels: Vec<(u32, u32, Vec<u8>)>,
}

/// CPU decode for formats the shader does not read.
fn decode_dds_rgba(bytes: &[u8]) -> Result<DecodedTexture> {
    let dds = Dds::read(Cursor::new(bytes)).map_err(|error| eyre!("invalid DDS: {error}"))?;
    let faces = if is_cubemap(&dds) { 6 } else { 1 };
    // `image_dds` has no X8R8G8B8 decoder and assumes tight rows; the packed
    // decoder reads DWORD-aligned 24-bit rows and a padded 32-bit mip 0 too.
    if let Some(format) = dds.get_d3d_format()
        && (matches!(format, D3DFormat::R8G8B8 | D3DFormat::X8R8G8B8)
            || (faces == 1 && matches!(format, D3DFormat::A8R8G8B8 | D3DFormat::A8B8G8R8)))
    {
        ensure!(faces == 1, "{format:?} cubemaps use the CPU encoder");
        return Ok(DecodedTexture {
            width: dds.get_width(),
            height: dds.get_height(),
            faces: 1,
            pixels: decode_packed_rgba8_mips(&dds)?,
        });
    }
    let mips = dds.get_num_mipmap_levels().max(1);
    let surface = image_dds::SurfaceRgba8::decode_layers_mipmaps_dds(&dds, 0..faces, 0..mips)
        .map_err(|error| eyre!("DDS cannot be decoded: {error}"))?;
    let mut pixels = Vec::with_capacity((faces * mips) as usize);
    for mip in 0..mips {
        for face in 0..faces {
            let image = surface
                .get_image(face, 0, mip)
                .ok_or_else(|| eyre!("face {face} mip {mip} is missing"))?;
            pixels.push((image.width(), image.height(), image.into_raw()));
        }
    }
    Ok(DecodedTexture {
        width: dds.get_width(),
        height: dds.get_height(),
        faces,
        pixels,
    })
}

/// A finished texture: KTX2 bytes plus their SHA-256 (already computed by
/// the validation, so callers need not hash again).
pub(crate) struct EncodedTexture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) sha256: String,
}

/// KTX2 bytes copied out of a finished slot, not yet validated.
struct BuiltKtx2 {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    faces: u32,
    levels: u32,
}

/// Builds the KTX2 for a texture from its raw UASTC blocks (one slice per
/// image, in `PreparedTexture::images` order). This is the only copy out of
/// the mapped readback, so the slot can be reused right after.
fn build_ktx2(
    texture: &PreparedTexture,
    blocks: &[&[u8]],
    first_image_has_alpha: bool,
    encoding: TextureEncoding,
) -> Result<BuiltKtx2> {
    ensure!(
        blocks.len() == texture.images.len(),
        "encoder returned {} images, expected {}",
        blocks.len(),
        texture.images.len()
    );
    let faces = texture.faces as usize;
    let levels: Vec<Vec<u8>> = blocks.chunks(faces).map(|faces| faces.concat()).collect();
    let bytes = ktx2::write_uastc(
        texture.width,
        texture.height,
        texture.faces,
        &levels,
        encoding == TextureEncoding::ColorSrgb,
        // The CPU path takes the KTX2 header (and so the RGB/RGBA channel
        // choice) from the first face's full-size image; mirror that.
        first_image_has_alpha,
        "mudcrab texture_gpu",
    );
    Ok(BuiltKtx2 {
        bytes,
        width: texture.width,
        height: texture.height,
        faces: texture.faces,
        levels: levels.len() as u32,
    })
}

/// Validates a built KTX2 the same way the CPU path does and hashes it.
fn validate_ktx2(
    built: BuiltKtx2,
    encoding: TextureEncoding,
    zstd_level: i32,
) -> Result<EncodedTexture> {
    // Same per-level Zstandard supercompression as the CPU path (lossless).
    let bytes = supercompress_ktx2_levels(&built.bytes, zstd_level)?;
    let metadata = inspect_ktx2(&bytes, encoding)?;
    ensure!(
        metadata.width == built.width
            && metadata.height == built.height
            && metadata.faces == built.faces
            && metadata.levels == built.levels,
        "GPU KTX2 layout does not match the DDS"
    );
    Ok(EncodedTexture {
        bytes,
        sha256: metadata.sha256,
    })
}

/// Caps bytes in a queue, so a slow consumer throttles its producers instead
/// of filling memory. Closing it releases every waiting producer.
struct ByteBudget {
    state: Mutex<BudgetState>,
    freed: Condvar,
    limit: u64,
}

#[derive(Default)]
struct BudgetState {
    used: u64,
    closed: bool,
}

impl ByteBudget {
    fn new(limit: u64) -> Self {
        Self {
            state: Mutex::default(),
            freed: Condvar::new(),
            limit,
        }
    }

    /// Blocks until `bytes` more fit under the limit, then reserves them.
    /// Returns `false`, reserving nothing, once the budget is closed.
    fn acquire(&self, bytes: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        // Always admit one item, however large, when nothing is pending.
        while !state.closed && state.used > 0 && state.used + bytes > self.limit {
            state = self.freed.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.used += bytes;
        true
    }

    /// Returns `bytes` to the budget and wakes waiting producers.
    fn release(&self, bytes: u64) {
        self.state.lock().unwrap().used -= bytes;
        self.freed.notify_all();
    }

    /// Refuses every later `acquire` and wakes those waiting.
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.freed.notify_all();
    }
}

/// Whether the GPU still works. Set from wgpu's callbacks, which run on
/// whichever thread notices the failure.
#[derive(Default)]
struct Health {
    failed: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl Health {
    /// Marks the GPU as failed; the first reason is kept.
    fn fail(&self, reason: String) {
        self.reason.lock().unwrap().get_or_insert(reason);
        self.failed.store(true, Ordering::Release);
    }

    fn check(&self) -> Result<()> {
        if !self.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        let reason = self.reason.lock().unwrap();
        Err(eyre!(
            "GPU stopped working: {}",
            reason.as_deref().unwrap_or("unknown error")
        ))
    }
}

/// wgpu's native futures resolve without an executor; poll until ready.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::yield_now();
    }
}

pub(crate) struct GpuUastc {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    /// Trit endpoint tables (astc_tables.rs), uploaded once.
    tables: wgpu::Buffer,
    /// 0 = fastest; each step adds an endpoint refinement pass.
    pub(crate) quality: u32,
    /// Zstandard level for the KTX2 levels (0 = stored), as the CPU path.
    pub(crate) zstd_level: i32,
    /// Source bytes a slot is filled up to before it is dispatched.
    pub(crate) batch_bytes: u64,
    /// Largest buffer the device can bind; a single texture may exceed
    /// `batch_bytes` but never this.
    binding_limit: u64,
    /// Slots allocated by `new`, so a GPU that cannot provide them is known
    /// before any texture is queued; `run_batcher` takes them.
    slots: Mutex<Vec<Slot>>,
    /// A shared encoder owns one stream of uploads, submissions and error scopes.
    batcher_lock: Mutex<()>,
    health: Arc<Health>,
    pub(crate) adapter_name: String,
}

/// Reusable GPU buffers for one batch.
struct Slot {
    source: wgpu::Buffer,
    descs: wgpu::Buffer,
    params: wgpu::Buffer,
    output: wgpu::Buffer,
    alpha: wgpu::Buffer,
    readback: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    source_cap: u64,
    images_cap: u64,
    blocks_cap: u64,
}

/// A texture placed in a slot.
struct Placed<T> {
    texture: PreparedTexture,
    encoding: TextureEncoding,
    tag: T,
    first_image: usize,
    first_block: usize,
}

struct Batch<T> {
    slot: Slot,
    textures: Vec<Placed<T>>,
    descs: Vec<u8>,
    source_len: u64,
    images: usize,
    blocks: usize,
}

impl<T> Batch<T> {
    /// An empty batch that fills `slot`.
    fn new(slot: Slot) -> Self {
        Self {
            slot,
            textures: Vec::new(),
            descs: Vec::new(),
            source_len: 0,
            images: 0,
            blocks: 0,
        }
    }

    /// Bytes of the readback buffer in use: the blocks, then one alpha flag per image.
    fn readback_len(&self) -> u64 {
        self.blocks as u64 * UASTC_BLOCK_BYTES as u64 + (self.images as u64 * 4).max(4)
    }
}

struct InFlight<T> {
    batch: Batch<T>,
    submission: wgpu::SubmissionIndex,
    /// An error wgpu reported while the batch was recorded or submitted.
    error: Option<String>,
}

impl GpuUastc {
    /// Encodes one caller-bounded batch through the existing three GPU slots.
    /// Results retain input order; the producer closes the queue to flush a
    /// partial final slot before this call returns.
    pub(crate) fn encode_prepared_batch(
        &self,
        textures: Vec<PreparedTexture>,
        encoding: TextureEncoding,
        post_threads: usize,
    ) -> Vec<Result<EncodedTexture>> {
        if textures.is_empty() {
            return Vec::new();
        }
        let results = Mutex::new(
            (0..textures.len())
                .map(|_| None)
                .collect::<Vec<Option<Result<EncodedTexture>>>>(),
        );
        // This batch is already bounded and owned by the caller. Queue its existing bytes
        // directly, avoiding an extra producer thread that could fail to start.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let budget = Arc::new(ByteBudget::new(u64::MAX));
        for (tag, texture) in textures.into_iter().enumerate() {
            let job = GpuJob {
                texture,
                encoding,
                tag,
            };
            budget.acquire(job.upload_len());
            // The receiver is held here until run_batcher takes it.
            let _ = sender.send(job);
        }
        drop(sender);
        run_batcher(
            self,
            JobReceiver {
                jobs: receiver,
                budget,
            },
            post_threads,
            || false,
            |tag, result| {
                results.lock().unwrap()[tag] = Some(result);
            },
        );
        results
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|result| result.unwrap_or_else(|| Err(eyre!("GPU batch returned no result"))))
            .collect()
    }

    /// Opens the GPU, compiles the encoder shader and runs a warm-up dispatch. Fails when no
    /// hardware GPU is available or its buffer limits are too small; callers then use the CPU encoder.
    pub(crate) fn new(quality: u32, batch_mb: u64) -> Result<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|error| eyre!("no GPU adapter: {error}"))?;
        let info = adapter.get_info();
        ensure!(
            info.device_type != wgpu::DeviceType::Cpu,
            "only a software GPU ({}) is available",
            info.name
        );
        Self::with_adapter(quality, batch_mb, adapter)
    }

    #[cfg(test)]
    pub(crate) fn new_software_for_test() -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        }))?;
        ensure!(
            adapter.get_info().device_type == wgpu::DeviceType::Cpu,
            "the test requires a software Vulkan adapter"
        );
        Self::with_adapter(2, 1, adapter)
    }

    fn with_adapter(quality: u32, batch_mb: u64, adapter: wgpu::Adapter) -> Result<Self> {
        let info = adapter.get_info();
        let limits = adapter.limits();
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("texture_gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|error| eyre!("cannot open GPU device: {error}"))?;
        // wgpu's default handlers panic. Errors outside the encoder's scopes
        // and a lost device instead retire the GPU; what it had not finished
        // goes to the CPU encoder.
        let health = Arc::new(Health::default());
        device.on_uncaptured_error({
            let health = Arc::clone(&health);
            Arc::new(move |error| health.fail(format!("{error}")))
        });
        device.set_device_lost_callback({
            let health = Arc::clone(&health);
            move |reason, message| health.fail(format!("device lost ({reason:?}): {message}"))
        });
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("uastc_encode"),
            source: wgpu::ShaderSource::Wgsl(include_str!("uastc_encode.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("uastc_encode"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(error) = block_on(validation.pop()) {
            return Err(eyre!("the encoder shader does not compile: {error}"));
        }
        let table_bytes: Vec<u8> = astc_tables::shader_tables()
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let tables = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("astc_tables"),
            size: table_bytes.len() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&tables, 0, &table_bytes);
        let binding_limit = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        // A full default slot writes 16 bytes per block for as little as 48
        // source bytes (BGR8), so the source buffer is the largest binding.
        let slot_overhead = SLOT_SPARE_BLOCKS * UASTC_BLOCK_BYTES as u64 + SLOT_IMAGES * 4;
        let batch_limit = binding_limit.saturating_sub(slot_overhead);
        ensure!(
            batch_limit >= 1 << 20,
            "the GPU's buffer size limit ({binding_limit} bytes) is too small"
        );
        let gpu = Self {
            device,
            queue,
            pipeline,
            tables,
            quality,
            zstd_level: 0,
            // Storage buffer sizes must be multiples of 4.
            batch_bytes: (batch_mb << 20).clamp(1 << 20, batch_limit) & !3,
            binding_limit,
            slots: Mutex::new(Vec::new()),
            batcher_lock: Mutex::new(()),
            health,
            adapter_name: format!("{} ({:?})", info.name, info.backend),
        };
        // Warm-up: surfaces shader/driver errors now and keeps driver
        // compilation out of the first real batch.
        let warm_up = PreparedTexture {
            width: 4,
            height: 4,
            faces: 1,
            images: vec![SourceImage {
                width: 4,
                height: 4,
                format: SourceFormat::Rgba8,
                offset: 0,
                pitch: 16,
            }],
            bytes: vec![0; 64],
        };
        let mut batch = Batch::new(gpu.new_slot(1 << 10, 16, 16)?);
        gpu.place(&mut batch, warm_up, TextureEncoding::ColorSrgb, ())
            .map_err(|(_, error)| error)?;
        let flight = gpu.dispatch(batch);
        gpu.wait(&flight)?;
        flight.batch.slot.readback.unmap();
        let slots = (0..SLOTS)
            .map(|_| gpu.default_slot())
            .collect::<Result<Vec<_>>>()?;
        *gpu.slots.lock().unwrap() = slots;
        Ok(gpu)
    }

    /// Allocates the buffers and bind group of a slot with the given capacities. Validation and
    /// out-of-memory failures are returned instead of reaching wgpu's error handler.
    fn new_slot(&self, source_cap: u64, images_cap: u64, blocks_cap: u64) -> Result<Slot> {
        let out_of_memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let buffer = |label, size: u64, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                // Storage buffer sizes must be multiples of 4.
                size: size.max(16).next_multiple_of(4),
                usage,
                mapped_at_creation: false,
            })
        };
        let storage_in = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let storage_out = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC;
        let source = buffer("texture_source", source_cap, storage_in);
        let descs = buffer("images", images_cap * IMAGE_DESC_BYTES as u64, storage_in);
        let params = buffer(
            "params",
            16,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let output = buffer("blocks", blocks_cap * UASTC_BLOCK_BYTES as u64, storage_out);
        let alpha = buffer(
            "alpha_flags",
            images_cap * 4,
            storage_out | wgpu::BufferUsages::COPY_DST,
        );
        let readback = buffer(
            "readback",
            blocks_cap * UASTC_BLOCK_BYTES as u64 + images_cap * 4,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let entries = [&source, &descs, &params, &output, &self.tables, &alpha];
        let entries: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(binding, buffer)| wgpu::BindGroupEntry {
                binding: binding as u32,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uastc_encode"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        // Scopes pop in reverse order of their push.
        let errors = [block_on(validation.pop()), block_on(out_of_memory.pop())];
        if let Some(error) = errors.into_iter().flatten().next() {
            return Err(eyre!(
                "GPU buffers for a {} MiB batch could not be created: {error}",
                source_cap >> 20
            ));
        }
        Ok(Slot {
            source,
            descs,
            params,
            output,
            alpha,
            readback,
            bind_group,
            source_cap,
            images_cap,
            blocks_cap,
        })
    }

    /// Takes the reusable slots; restores any slots missing after a failed allocation.
    fn take_slots(&self) -> Result<Vec<Slot>> {
        let mut slots = std::mem::take(&mut *self.slots.lock().unwrap());
        while slots.len() < SLOTS {
            slots.push(self.default_slot()?);
        }
        Ok(slots)
    }

    /// A slot for a full batch. Output is sized for the densest source (BGR8:
    /// 48 source bytes per 16-byte UASTC block).
    fn default_slot(&self) -> Result<Slot> {
        self.new_slot(
            self.batch_bytes,
            SLOT_IMAGES,
            self.batch_bytes / DENSEST_BLOCK_BYTES + SLOT_SPARE_BLOCKS,
        )
    }

    /// Whether `texture` fits in `batch` without growing its buffers.
    fn fits<T>(&self, batch: &Batch<T>, texture: &PreparedTexture) -> bool {
        let slot = &batch.slot;
        batch.source_len + aligned_len(texture.upload().len()) <= slot.source_cap
            && (batch.images + texture.images.len()) as u64 <= slot.images_cap
            && (batch.blocks + texture.block_count()) as u64 <= slot.blocks_cap
    }

    /// Uploads one texture into the batch's slot right away (so uploads
    /// overlap with GPU work) and records its image descriptors. On error the
    /// tag is handed back.
    fn place<T>(
        &self,
        batch: &mut Batch<T>,
        mut texture: PreparedTexture,
        encoding: TextureEncoding,
        tag: T,
    ) -> std::result::Result<(), (T, Report)> {
        let needed_source = batch.source_len + aligned_len(texture.upload().len());
        let needed_images = (batch.images + texture.images.len()) as u64;
        let needed_blocks = (batch.blocks + texture.block_count()) as u64;
        // The readback buffer holds the blocks plus one alpha flag per image.
        if needed_source > self.binding_limit
            || needed_blocks
                .saturating_mul(UASTC_BLOCK_BYTES as u64)
                .saturating_add(needed_images.saturating_mul(4))
                > self.binding_limit
        {
            return Err((tag, eyre!("texture exceeds the GPU's buffer size limit")));
        }
        if needed_source > batch.slot.source_cap
            || needed_images > batch.slot.images_cap
            || needed_blocks > batch.slot.blocks_cap
        {
            // Only an empty batch grows (a single oversized texture); callers
            // dispatch a non-empty batch before it would overflow.
            if !batch.textures.is_empty() {
                return Err((tag, eyre!("GPU batch overflow")));
            }
            batch.slot = match self.new_slot(
                needed_source.max(batch.slot.source_cap),
                needed_images.max(batch.slot.images_cap),
                needed_blocks.max(batch.slot.blocks_cap),
            ) {
                Ok(slot) => slot,
                Err(error) => return Err((tag, error)),
            };
        }
        let base = batch.source_len;
        write_padded(&self.queue, &batch.slot.source, base, texture.upload());
        texture.release_upload();
        let mut first_block = batch.blocks;
        for image in &texture.images {
            let fields = [
                (base as usize + image.offset) as u32,
                image.width,
                image.height,
                image.width.div_ceil(4),
                first_block as u32,
                image.format as u32,
                image.pitch,
                0,
            ];
            batch
                .descs
                .extend(fields.iter().flat_map(|field| field.to_le_bytes()));
            first_block += image.block_count();
        }
        batch.textures.push(Placed {
            first_image: batch.images,
            first_block: batch.blocks,
            encoding,
            tag,
            texture,
        });
        batch.source_len = needed_source;
        batch.images = needed_images as usize;
        batch.blocks = needed_blocks as usize;
        Ok(())
    }

    /// Encodes every block of the batch, `DISPATCH_BLOCKS` per submission, and
    /// queues the copy to the readback buffer. Does not wait.
    fn dispatch<T>(&self, batch: Batch<T>) -> InFlight<T> {
        let out_of_memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let slot = &batch.slot;
        self.queue.write_buffer(&slot.descs, 0, &batch.descs);
        let total = batch.blocks as u32;
        let alpha_bytes = (batch.images as u64 * 4).max(4);
        let mut base = 0u32;
        let submission = loop {
            let end = total.min(base.saturating_add(DISPATCH_BLOCKS));
            // Applied before the next submission, so each one sees its own range.
            let params: Vec<u8> = [batch.images as u32, end, self.quality, base]
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            self.queue.write_buffer(&slot.params, 0, &params);
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("uastc_encode"),
                });
            if base == 0 {
                encoder.clear_buffer(&slot.alpha, 0, Some(alpha_bytes));
            }
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("uastc_encode"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.dispatch_workgroups((end - base).div_ceil(WORKGROUP_SIZE).max(1), 1, 1);
            }
            if end < total {
                self.queue.submit([encoder.finish()]);
                base = end;
                continue;
            }
            let output_bytes = batch.blocks as u64 * UASTC_BLOCK_BYTES as u64;
            if output_bytes > 0 {
                encoder.copy_buffer_to_buffer(&slot.output, 0, &slot.readback, 0, output_bytes);
            }
            encoder.copy_buffer_to_buffer(
                &slot.alpha,
                0,
                &slot.readback,
                output_bytes,
                alpha_bytes,
            );
            break self.queue.submit([encoder.finish()]);
        };
        // Scopes pop in reverse order of their push.
        let errors = [block_on(validation.pop()), block_on(out_of_memory.pop())];
        let error = errors
            .into_iter()
            .flatten()
            .next()
            .map(|error| error.to_string());
        InFlight {
            batch,
            submission,
            error,
        }
    }

    /// Blocks until a dispatched batch is done and its readback is mapped. On
    /// an error nothing is left mapped and the slot can be reused.
    fn wait<T>(&self, flight: &InFlight<T>) -> Result<()> {
        if let Some(error) = &flight.error {
            return Err(eyre!("GPU dispatch failed: {error}"));
        }
        self.health.check()?;
        let readback = &flight.batch.slot.readback;
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..flight.batch.readback_len()).map_async(
            wgpu::MapMode::Read,
            move |result| {
                let _ = tx.send(result);
            },
        );
        let outcome = (|| {
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(flight.submission.clone()),
                    timeout: None,
                })
                .map_err(|error| eyre!("GPU poll failed: {error}"))?;
            rx.recv()
                .map_err(|_| eyre!("GPU readback was dropped"))?
                .map_err(|error| eyre!("GPU readback failed: {error}"))?;
            // A device lost while the batch ran leaves the readback undefined.
            self.health.check()
        })();
        if outcome.is_err() {
            // Also cancels a pending map when polling or its callback failed.
            readback.unmap();
        }
        outcome
    }
}

/// `len` rounded up to the 4-byte multiple `queue.write_buffer` requires.
fn aligned_len(len: usize) -> u64 {
    (len as u64).div_ceil(4) * 4
}

/// `queue.write_buffer` needs 4-byte multiples; pad the tail with zeros.
fn write_padded(queue: &wgpu::Queue, buffer: &wgpu::Buffer, offset: u64, bytes: &[u8]) {
    let body = bytes.len() / 4 * 4;
    if body > 0 {
        queue.write_buffer(buffer, offset, &bytes[..body]);
    }
    if body < bytes.len() {
        let mut tail = [0u8; 4];
        tail[..bytes.len() - body].copy_from_slice(&bytes[body..]);
        queue.write_buffer(buffer, offset + body as u64, &tail);
    }
}

/// A texture queued for the GPU, with the caller's bookkeeping.
pub(crate) struct GpuJob<T> {
    pub(crate) texture: PreparedTexture,
    pub(crate) encoding: TextureEncoding,
    pub(crate) tag: T,
}

impl<T> GpuJob<T> {
    fn upload_len(&self) -> u64 {
        self.texture.upload().len() as u64
    }
}

/// The batcher's end of `job_channel`.
pub(crate) struct JobReceiver<T> {
    jobs: Receiver<GpuJob<T>>,
    budget: Arc<ByteBudget>,
}

impl<T> Drop for JobReceiver<T> {
    fn drop(&mut self) {
        // Senders waiting for room give up instead of waiting forever.
        self.budget.close();
    }
}

/// The readers' end of `job_channel`.
pub(crate) struct JobSender<T> {
    jobs: Sender<GpuJob<T>>,
    budget: Arc<ByteBudget>,
    health: Arc<Health>,
}

impl<T> JobSender<T> {
    /// Queues a texture for the GPU, waiting while the queue holds its
    /// limit in bytes. Hands the job back when the GPU has failed or the
    /// batcher is gone; the caller then encodes it on the CPU.
    pub(crate) fn send(&self, job: GpuJob<T>) -> std::result::Result<(), GpuJob<T>> {
        if self.health.check().is_err() {
            return Err(job);
        }
        let bytes = job.upload_len();
        if !self.budget.acquire(bytes) {
            return Err(job);
        }
        self.jobs.send(job).map_err(|error| {
            self.budget.release(bytes);
            error.into_inner()
        })
    }
}

/// A queue of textures for `run_batcher`, holding at most `QUEUED_BATCHES`
/// batches' worth of source bytes.
pub(crate) fn job_channel<T>(gpu: &GpuUastc) -> (JobSender<T>, JobReceiver<T>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let budget = Arc::new(ByteBudget::new(QUEUED_BATCHES * gpu.batch_bytes));
    (
        JobSender {
            jobs: sender,
            budget: Arc::clone(&budget),
            health: Arc::clone(&gpu.health),
        },
        JobReceiver {
            jobs: receiver,
            budget,
        },
    )
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BatchStats {
    pub(crate) batches: usize,
    pub(crate) textures: usize,
    /// Bytes uploaded (DDS payloads, or RGBA for CPU-decoded formats).
    pub(crate) source_bytes: u64,
}

/// Item for the writer threads: a built KTX2, or why the texture was not encoded.
type Pending<T> = (T, TextureEncoding, Result<BuiltKtx2>);

/// Return every queued job to the caller's CPU fallback, releasing queue reservations first.
/// Keep draining until producers finish; a startup failure must not lose work or block senders.
fn reject_jobs<T>(
    jobs: &JobReceiver<T>,
    stopped: &impl Fn() -> bool,
    done: &impl Fn(T, Result<EncodedTexture>),
    reason: &str,
) {
    for job in jobs.jobs.iter() {
        jobs.budget.release(job.upload_len());
        if !stopped() {
            done(job.tag, Err(eyre!("GPU worker unavailable: {reason}")));
        }
    }
}

fn post_pool_or_fallback<T>(
    pool: std::result::Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>,
    jobs: &JobReceiver<T>,
    stopped: &impl Fn() -> bool,
    done: &impl Fn(T, Result<EncodedTexture>),
) -> Option<rayon::ThreadPool> {
    match pool {
        Ok(pool) => Some(pool),
        Err(error) => {
            reject_jobs(jobs, stopped, done, &error.to_string());
            None
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAILED_WORKER: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

fn spawn_scoped_worker<'scope, 'env, F, T>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    name: String,
    work: F,
) -> std::io::Result<std::thread::ScopedJoinHandle<'scope, T>>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    #[cfg(test)]
    if FAILED_WORKER.with(|failed| failed.get().is_some_and(|failed| failed == name)) {
        return Err(std::io::Error::other("injected GPU worker startup failure"));
    }
    std::thread::Builder::new()
        .name(name)
        .spawn_scoped(scope, work)
}

/// Encodes every job arriving on `jobs` until all senders are dropped, and
/// calls `done` (from `threads` writer threads, in any order) with each job's
/// tag and its validated KTX2, or the error that kept the GPU from encoding
/// it. Once `stopped` returns true, jobs are dropped without calling `done`.
/// See the module docs for the thread layout.
pub(crate) fn run_batcher<T: Send>(
    gpu: &GpuUastc,
    jobs: JobReceiver<T>,
    threads: usize,
    stopped: impl Fn() -> bool + Sync,
    done: impl Fn(T, Result<EncodedTexture>) + Sync,
) -> BatchStats {
    let _batcher = gpu.batcher_lock.lock().unwrap();
    let threads = threads.max(1);
    let Some(post_pool) = post_pool_or_fallback(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("texture-gpu-post-{index}"))
            .build(),
        &jobs,
        &stopped,
        &done,
    ) else {
        return BatchStats::default();
    };
    let (flight_tx, flight_rx) = crossbeam_channel::bounded::<InFlight<T>>(SLOTS);
    let (slot_tx, slot_rx) = crossbeam_channel::bounded::<Slot>(SLOTS);
    let stats = Mutex::new(BatchStats::default());
    let budget = ByteBudget::new(PENDING_WRITE_BYTES);
    let (write_tx, write_rx) = crossbeam_channel::unbounded::<Pending<T>>();
    let (done, stopped, stats_ref, post_pool, budget) =
        (&done, &stopped, &stats, &post_pool, &budget);
    let mut took_slots = false;

    std::thread::scope(|scope| {
        // Writers: validate, hash and hand each texture to `done` (which
        // writes it), in parallel and decoupled from the GPU slots.
        for index in 0..threads {
            let write_rx = write_rx.clone();
            let spawned =
                spawn_scoped_worker(scope, format!("texture-gpu-write-{index}"), move || {
                    for (tag, encoding, built) in write_rx {
                        let len = built.as_ref().map_or(0, |built| built.bytes.len() as u64);
                        if !stopped() {
                            done(
                                tag,
                                built.and_then(|built| {
                                    validate_ktx2(built, encoding, gpu.zstd_level)
                                }),
                            );
                        }
                        budget.release(len);
                    }
                });
            if let Err(error) = spawned {
                eprintln!("GPU texture writer unavailable ({error}); using {index} writer(s)");
                if index == 0 {
                    drop(write_tx);
                    reject_jobs(&jobs, stopped, done, &error.to_string());
                    return;
                }
                break;
            }
        }
        drop(write_rx);

        let mut slots = match gpu.take_slots() {
            Ok(slots) => slots,
            Err(error) => {
                // Without slots nothing can be encoded here; every job goes back with the reason.
                for job in jobs.jobs.iter() {
                    jobs.budget.release(job.upload_len());
                    let failed = Err(eyre!("{error:#}"));
                    let _ = write_tx.send((job.tag, job.encoding, failed));
                }
                return;
            }
        };
        took_slots = true;
        // One slot starts in the batcher's hands, the rest wait here.
        let first_slot = slots.pop().expect("take_slots returns SLOTS slots");
        for slot in slots {
            let _ = slot_tx.send(slot);
        }

        // Readback: wait for each batch in order, copy its blocks out into
        // KTX2 files in parallel, return the slot at once, then queue the
        // files for the writers.
        let readback_write_tx = write_tx.clone();
        let return_slot_tx = slot_tx.clone();
        let readback = spawn_scoped_worker(scope, "texture-gpu-readback".into(), move || {
            for flight in flight_rx {
                let waited = gpu.wait(&flight);
                let readback_len = flight.batch.readback_len();
                let Batch {
                    slot,
                    textures,
                    blocks,
                    ..
                } = flight.batch;
                let count = textures.len();
                let built: Vec<Pending<T>> = match waited {
                    Ok(()) => {
                        let built = {
                            let mapped = slot.readback.slice(..readback_len).get_mapped_range();
                            let (block_bytes, alpha_bytes) =
                                mapped.split_at(blocks * UASTC_BLOCK_BYTES);
                            post_pool.install(|| {
                                textures
                                    .into_par_iter()
                                    .map(|placed| {
                                        let mut offset = placed.first_block * UASTC_BLOCK_BYTES;
                                        let slices: Vec<&[u8]> = placed
                                            .texture
                                            .images
                                            .iter()
                                            .map(|image| {
                                                let len = image.block_count() * UASTC_BLOCK_BYTES;
                                                offset += len;
                                                &block_bytes[offset - len..offset]
                                            })
                                            .collect();
                                        let flag = placed.first_image * 4;
                                        let has_alpha = alpha_bytes[flag..flag + 4] != [0; 4];
                                        let built = build_ktx2(
                                            &placed.texture,
                                            &slices,
                                            has_alpha,
                                            placed.encoding,
                                        );
                                        (placed.tag, placed.encoding, built)
                                    })
                                    .collect()
                            })
                        };
                        slot.readback.unmap();
                        built
                    }
                    Err(error) => textures
                        .into_iter()
                        .map(|placed| {
                            let failed = Err(eyre!("GPU batch failed: {error:#}"));
                            (placed.tag, placed.encoding, failed)
                        })
                        .collect(),
                };
                // A slot grown for one oversized texture stays in rotation: it
                // already exists and is within the device limits.
                let _ = slot_tx.send(slot);
                for item in built {
                    budget.acquire(item.2.as_ref().map_or(0, |built| built.bytes.len() as u64));
                    let _ = readback_write_tx.send(item);
                }
                let mut stats = stats_ref.lock().unwrap();
                stats.batches += 1;
                stats.textures += count;
            }
        });
        if let Err(error) = readback {
            // No dispatch has started. Return every slot, stop the idle writers, and hand
            // queued textures back to CPU encoding without waiting on a missing reader.
            let _ = return_slot_tx.send(first_slot);
            drop(flight_tx);
            drop(write_tx);
            reject_jobs(&jobs, stopped, done, &error.to_string());
            return;
        }

        // Batcher: stream textures into the current slot, dispatch when full.
        let mut source_bytes = 0u64;
        let mut batch = Batch::<T>::new(first_slot);
        for job in jobs.jobs.iter() {
            let bytes = job.upload_len();
            if stopped() {
                jobs.budget.release(bytes);
                continue;
            }
            // A failed GPU takes nothing more; the writers hand the job back.
            if let Err(error) = gpu.health.check() {
                jobs.budget.release(bytes);
                let _ = write_tx.send((job.tag, job.encoding, Err(error)));
                continue;
            }
            if !batch.textures.is_empty() && !gpu.fits(&batch, &job.texture) {
                let next = slot_rx.recv().expect("readback thread returns slots");
                let full = std::mem::replace(&mut batch, Batch::new(next));
                let _ = flight_tx.send(gpu.dispatch(full));
            }
            source_bytes += bytes;
            let encoding = job.encoding;
            let placed = gpu.place(&mut batch, job.texture, encoding, job.tag);
            jobs.budget.release(bytes);
            if let Err((tag, error)) = placed {
                let _ = write_tx.send((tag, encoding, Err(error)));
            }
        }
        if !batch.textures.is_empty() && !stopped() {
            let _ = flight_tx.send(gpu.dispatch(batch));
        } else {
            let _ = return_slot_tx.send(batch.slot);
        }
        drop(flight_tx);
        drop(write_tx);
        stats.lock().unwrap().source_bytes += source_bytes;
    });
    if took_slots {
        *gpu.slots.lock().unwrap() = slot_rx.try_iter().collect();
    }
    stats.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_startup_failure_returns_every_job_and_unblocks_producers() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let budget = Arc::new(ByteBudget::new(64));
        let sender = JobSender {
            jobs: tx,
            budget: Arc::clone(&budget),
            health: Arc::new(Health::default()),
        };
        let receiver = JobReceiver {
            jobs: rx,
            budget: Arc::clone(&budget),
        };
        let producer = std::thread::spawn(move || {
            for tag in 0..4 {
                let texture = PreparedTexture::from_rgba_mips(4, 4, &[vec![255; 64]]).unwrap();
                assert!(
                    sender
                        .send(GpuJob {
                            texture,
                            encoding: TextureEncoding::ColorSrgb,
                            tag
                        })
                        .is_ok()
                );
            }
        });
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .spawn_handler(|_| Err(std::io::Error::other("injected thread limit")))
            .build();
        let mut returned = Vec::new();
        let returned = Mutex::new(&mut returned);
        assert!(
            post_pool_or_fallback(pool, &receiver, &|| false, &|tag, result| {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("injected thread limit")
                );
                returned.lock().unwrap().push(tag);
            })
            .is_none()
        );
        producer.join().unwrap();
        assert_eq!(**returned.lock().unwrap(), [0, 1, 2, 3]);
        assert_eq!(budget.state.lock().unwrap().used, 0);
    }

    #[test]
    fn stopped_worker_fallback_releases_jobs_without_running_callbacks() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let budget = Arc::new(ByteBudget::new(64));
        budget.acquire(64);
        let texture = PreparedTexture::from_rgba_mips(4, 4, &[vec![255; 64]]).unwrap();
        assert!(
            tx.send(GpuJob {
                texture,
                encoding: TextureEncoding::ColorSrgb,
                tag: ()
            })
            .is_ok()
        );
        drop(tx);
        reject_jobs(
            &JobReceiver {
                jobs: rx,
                budget: Arc::clone(&budget),
            },
            &|| true,
            &|_, _| panic!("cancelled work ran"),
            "thread limit",
        );
        assert_eq!(budget.state.lock().unwrap().used, 0);
    }

    #[test]
    fn prepared_rgba_mips_preserve_authored_levels_and_layout() {
        let levels: Vec<_> = [128, 32, 8]
            .into_iter()
            .enumerate()
            .map(|(mip, len)| vec![mip as u8 + 1; len])
            .collect();
        let texture = PreparedTexture::from_rgba_mips(8, 4, &levels).unwrap();
        assert_eq!((texture.width, texture.height, texture.faces), (8, 4, 1));
        assert_eq!(texture.upload(), levels.concat());
        assert_eq!(
            texture
                .images
                .iter()
                .map(|image| (
                    image.width,
                    image.height,
                    image.offset,
                    image.pitch,
                    image.format
                ))
                .collect::<Vec<_>>(),
            [
                (8, 4, 0, 32, SourceFormat::Rgba8),
                (4, 2, 128, 16, SourceFormat::Rgba8),
                (2, 1, 160, 8, SourceFormat::Rgba8)
            ]
        );
    }

    #[test]
    fn prepared_rgba_mips_reject_invalid_dimensions_and_levels() {
        assert!(PreparedTexture::from_rgba_mips(0, 4, &[vec![]]).is_err());
        assert!(PreparedTexture::from_rgba_mips(4, 4, &[]).is_err());
        assert!(PreparedTexture::from_rgba_mips(4, 4, &[vec![0; 63]]).is_err());
        assert!(PreparedTexture::from_rgba_mips(4, 4, &[vec![0; 64], vec![0; 15]]).is_err());
        assert!(PreparedTexture::from_rgba_mips(1, 1, &[vec![0; 4], vec![0; 4]]).is_err());
        assert!(PreparedTexture::from_rgba_mips(u32::MAX, 1, &[vec![]]).is_err());
    }

    #[test]
    #[ignore = "requires an idle hardware GPU"]
    fn worker_startup_failures_return_jobs_and_keep_gpu_slots_reusable() {
        struct ResetWorker;
        impl Drop for ResetWorker {
            fn drop(&mut self) {
                FAILED_WORKER.with(|failed| failed.set(None));
            }
        }
        let _reset = ResetWorker;
        let gpu = GpuUastc::new(DEFAULT_QUALITY, 1).unwrap();
        let levels = vec![vec![255; 8 * 8 * 4]];
        for worker in [
            "texture-gpu-write-0",
            "texture-gpu-write-1",
            "texture-gpu-readback",
        ] {
            FAILED_WORKER.with(|failed| failed.set(Some(worker)));
            let outputs = gpu.encode_prepared_batch(
                (0..4)
                    .map(|_| PreparedTexture::from_rgba_mips(8, 8, &levels).unwrap())
                    .collect(),
                TextureEncoding::ColorSrgb,
                2,
            );
            assert_eq!(outputs.len(), 4);
            for output in outputs {
                if worker == "texture-gpu-write-1" {
                    output.unwrap(); // The first writer still drains every job.
                } else {
                    assert!(
                        output
                            .err()
                            .unwrap()
                            .to_string()
                            .contains("injected GPU worker startup failure")
                    );
                }
            }
            assert_eq!(gpu.slots.lock().unwrap().len(), SLOTS);
        }
        FAILED_WORKER.with(|failed| failed.set(None));
        assert!(
            gpu.encode_prepared_batch(
                vec![PreparedTexture::from_rgba_mips(8, 8, &levels).unwrap()],
                TextureEncoding::ColorSrgb,
                1,
            )
            .pop()
            .unwrap()
            .is_ok()
        );
    }

    #[test]
    #[ignore = "requires an idle hardware GPU"]
    fn prepared_gpu_batches_serialize_and_return_all_slots() {
        let gpu = GpuUastc::new(DEFAULT_QUALITY, 1).unwrap();
        let levels = [
            [21, 45, 87, 255].repeat(64),
            [21, 45, 87, 255].repeat(16),
            [21, 45, 87, 255].repeat(4),
        ];
        let encode = || {
            gpu.encode_prepared_batch(
                vec![PreparedTexture::from_rgba_mips(8, 8, &levels).unwrap()],
                TextureEncoding::ColorSrgb,
                1,
            )
            .pop()
            .unwrap()
            .unwrap()
        };
        let expected = encode().bytes;
        assert_eq!(gpu.slots.lock().unwrap().len(), SLOTS);
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(encode);
            let second = scope.spawn(encode);
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_eq!(first.bytes, expected);
        assert_eq!(second.bytes, expected);
        assert_eq!(gpu.slots.lock().unwrap().len(), SLOTS);
        // Error cleanup must cancel a pending native map before a slot returns
        // to rotation. Exercise wgpu directly, without a production test hook.
        let slot = gpu.slots.lock().unwrap().pop().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        slot.readback
            .slice(..16)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        slot.readback.unmap();
        gpu.device.poll(wgpu::PollType::Poll).unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .is_err(),
            "unmap must cancel the pending readback"
        );
        gpu.slots.lock().unwrap().push(slot);
        assert_eq!(encode().bytes, expected);
        assert_eq!(gpu.slots.lock().unwrap().len(), SLOTS);
        assert!(
            gpu.encode_prepared_batch(Vec::new(), TextureEncoding::ColorSrgb, 1)
                .is_empty()
        );
        assert_eq!(gpu.slots.lock().unwrap().len(), SLOTS);
    }

    /// A minimal uncompressed A8R8G8B8 DDS header for `width` x `height` with `mips` levels,
    /// followed by `payload` bytes of pixel data.
    fn bgra_dds(width: u32, height: u32, mips: u32, payload: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(128 + payload);
        bytes.extend_from_slice(b"DDS ");
        let mut header = [0u32; 31];
        header[0] = 124; // dwSize
        header[1] = 0x1 | 0x2 | 0x4 | 0x1000 | 0x20000; // caps, height, width, pixel format, mips
        header[2] = height;
        header[3] = width;
        header[6] = mips;
        header[18] = 32; // pixel format size
        header[19] = 0x40 | 0x1; // RGB | alpha pixels
        header[21] = 32; // bits per pixel
        header[22] = 0x00FF_0000;
        header[23] = 0x0000_FF00;
        header[24] = 0x0000_00FF;
        header[25] = 0xFF00_0000;
        header[26] = 0x1000 | 0x400000; // texture | mipmap
        for word in header {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.resize(128 + payload, 0);
        bytes
    }

    /// The stored layout lists each mip after the previous one and sums their sizes.
    #[test]
    fn stored_layout_places_each_mip_after_the_previous() {
        let (images, len) = stored_layout(SourceFormat::Bgra8, 8, 4, 1, 3).unwrap();
        let placed: Vec<_> = images
            .iter()
            .map(|image| (image.width, image.height, image.offset, image.pitch))
            .collect();
        assert_eq!(placed, [(8, 4, 0, 32), (4, 2, 128, 16), (2, 1, 160, 8)]);
        assert_eq!(len, 168);
        // BGR8 rows are 3 bytes per texel; partial blocks still count as whole.
        let (images, len) = stored_layout(SourceFormat::Bgr8, 6, 6, 1, 1).unwrap();
        assert_eq!((images[0].block_count(), len), (4, 108));
    }

    /// Sizes that overflow are reported, not wrapped: a header can claim any dimensions.
    #[test]
    fn stored_layout_rejects_dimensions_whose_sizes_overflow() {
        assert!(stored_layout(SourceFormat::Bgra8, u32::MAX, 1, 1, 1).is_none());
        assert!(stored_layout(SourceFormat::Bgra8, 1 << 30, u32::MAX, 1, 1).is_none());
        assert!(stored_layout(SourceFormat::Bgr8, u32::MAX, u32::MAX, 6, 1).is_none());
    }

    /// A well-formed uncompressed DDS keeps only its payload, mip by mip:
    /// trailing data is dropped, so it never sits in the queue unbudgeted.
    #[test]
    fn prepares_an_uncompressed_dds_from_its_payload_only() {
        let mut dds = bgra_dds(4, 4, 2, 64 + 16);
        dds[128] = 7;
        dds.extend(std::iter::repeat_n(0xAB, 1 << 20));
        let texture = PreparedTexture::from_dds(dds, TextureEncoding::ColorSrgb).unwrap();
        assert_eq!((texture.width, texture.height, texture.faces), (4, 4, 1));
        assert_eq!(texture.upload().len(), 80);
        assert_eq!(texture.upload()[0], 7);
        assert!(texture.bytes.capacity() < 1 << 20);
        assert_eq!(texture.images.len(), 2);
        assert_eq!(texture.block_count(), 2);
    }

    /// Absurd header dimensions end in an error for the CPU path to report, without a panic.
    #[test]
    fn rejects_a_dds_whose_header_claims_impossible_sizes() {
        for (width, height) in [(u32::MAX, u32::MAX), (u32::MAX, 1), (0, 16)] {
            let bytes = bgra_dds(width, height, 1, 64);
            assert!(PreparedTexture::from_dds(bytes, TextureEncoding::ColorSrgb).is_err());
        }
    }

    /// The KTX2 header and data format descriptor the GPU path writes match
    /// Basis Universal's for the same texture, so the two encoders' files are
    /// interchangeable at runtime.
    #[test]
    fn ktx2_container_matches_basis_universal() {
        let cases = [
            (TextureEncoding::ColorSrgb, 255u8),
            (TextureEncoding::NormalLinear, 255),
            (TextureEncoding::DataLinear, 128),
        ];
        for (encoding, alpha) in cases {
            let mut dds = bgra_dds(8, 8, 1, 8 * 8 * 4);
            for (index, texel) in dds[128..].chunks_mut(4).enumerate() {
                texel.copy_from_slice(&[index as u8 * 4, 64, 255 - index as u8, alpha]);
            }
            // The CPU default block-compresses uncompressed RGBA to native BC,
            // so the UASTC reference is encoded from the decoded surface.
            let surface =
                image_dds::SurfaceRgba8::decode_dds(&Dds::read(Cursor::new(&dds[..])).unwrap())
                    .unwrap();
            let cpu = crate::texture::encode_2d_surface(
                &surface,
                encoding,
                crate::texture::ETC1S_QUALITY_DEFAULT,
                crate::texture::UASTC_LEVEL_DEFAULT,
            )
            .unwrap();
            let gpu = ktx2::write_uastc(
                8,
                8,
                1,
                &[vec![0; 4 * UASTC_BLOCK_BYTES]],
                encoding == TextureEncoding::ColorSrgb,
                alpha != 255,
                "mudcrab texture_gpu",
            );
            let dfd = |bytes: &[u8]| {
                let index = ::ktx2::Reader::new(bytes).unwrap().header().index;
                let start = index.dfd_byte_offset as usize;
                bytes[start..start + index.dfd_byte_length as usize].to_vec()
            };
            assert_eq!(dfd(&gpu), dfd(&cpu), "{encoding:?}");
            let (gpu_header, cpu_header) = (
                ::ktx2::Reader::new(&gpu[..]).unwrap().header(),
                ::ktx2::Reader::new(&cpu[..]).unwrap().header(),
            );
            assert_eq!(gpu_header.format, cpu_header.format);
            assert_eq!(gpu_header.type_size, cpu_header.type_size);
            assert_eq!(
                (gpu_header.pixel_width, gpu_header.pixel_height),
                (cpu_header.pixel_width, cpu_header.pixel_height)
            );
            assert_eq!(gpu_header.pixel_depth, cpu_header.pixel_depth);
            assert_eq!(gpu_header.layer_count, cpu_header.layer_count);
            assert_eq!(gpu_header.face_count, cpu_header.face_count);
            assert_eq!(gpu_header.level_count, cpu_header.level_count);
            inspect_ktx2(&gpu, encoding).unwrap();
        }
    }

    /// CI runs this explicitly with Mesa's software Vulkan adapter. Missing support fails.
    #[test]
    #[ignore = "requires a software Vulkan adapter; CI runs explicitly"]
    fn v210_software_gpu_encodes_and_decodes_color_alpha_normals_and_mips() {
        use crate::texture::{TextureConverter, tests::decode_uastc_level};
        let gpu = GpuUastc::new_software_for_test().expect("open software Vulkan encoder");
        let (sender, jobs) = job_channel(&gpu);
        let mut cases = Vec::new();
        for encoding in [
            TextureEncoding::ColorSrgb,
            TextureEncoding::DataLinear,
            TextureEncoding::NormalLinear,
        ] {
            for (width, height) in [(8u32, 8u32), (7, 5)] {
                let levels: Vec<Vec<u8>> = (0..3)
                    .map(|mip| {
                        let w = (width >> mip).max(1);
                        let h = (height >> mip).max(1);
                        (0..h)
                            .flat_map(|y| {
                                (0..w).flat_map(move |x| {
                                    if encoding == TextureEncoding::NormalLinear {
                                        if x < w.div_ceil(2) {
                                            [224, 48, 196, 255]
                                        } else {
                                            [40, 208, 180, 255]
                                        }
                                    } else {
                                        [
                                            36 + x as u8 * 8,
                                            61 + y as u8 * 7,
                                            180 - x as u8 * 5,
                                            if x < w.div_ceil(2) { 0 } else { 255 },
                                        ]
                                    }
                                })
                            })
                            .collect()
                    })
                    .collect();
                let payload = levels.iter().map(Vec::len).sum();
                let mut dds = bgra_dds(width, height, 3, payload);
                let mut offset = 128;
                for level in &levels {
                    for pixel in level.as_chunks::<4>().0 {
                        dds[offset..offset + 4]
                            .copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                        offset += 4;
                    }
                }
                let tag = cases.len();
                sender
                    .send(GpuJob {
                        texture: PreparedTexture::from_dds(dds, encoding).unwrap(),
                        encoding,
                        tag,
                    })
                    .unwrap_or_else(|_| panic!("GPU queue rejected synthetic texture"));
                cases.push((width, height, encoding, levels));
            }
        }
        drop(sender);
        let outputs = Mutex::new(std::collections::BTreeMap::new());
        let stats = run_batcher(
            &gpu,
            jobs,
            2,
            || false,
            |tag, result| {
                outputs
                    .lock()
                    .unwrap()
                    .insert(tag, result.expect("GPU encode").bytes);
            },
        );
        assert_eq!(stats.textures, cases.len());
        let outputs = outputs.into_inner().unwrap();
        assert_eq!(outputs.len(), cases.len());
        for (tag, (width, height, encoding, levels)) in cases.iter().enumerate() {
            let bytes = &outputs[&tag];
            let metadata = inspect_ktx2(bytes, *encoding).unwrap();
            assert_eq!(
                (
                    metadata.width,
                    metadata.height,
                    metadata.faces,
                    metadata.levels
                ),
                (*width, *height, 1, 3)
            );
            let cpu =
                TextureConverter::encode_rgba_mips(*width, *height, levels, *encoding).unwrap();
            for (mip, source) in levels.iter().enumerate() {
                let gpu_pixels = decode_uastc_level(bytes, mip);
                let cpu_pixels = decode_uastc_level(&cpu, mip);
                assert_eq!(gpu_pixels.len(), source.len());
                assert_eq!(cpu_pixels.len(), source.len());
                for (label, actual) in [("GPU", &gpu_pixels), ("CPU", &cpu_pixels)] {
                    for channel in 0..4 {
                        let errors: Vec<u32> = actual
                            .iter()
                            .skip(channel)
                            .step_by(4)
                            .zip(source.iter().skip(channel).step_by(4))
                            .map(|(a, b)| u32::from(a.abs_diff(*b)))
                            .collect();
                        let mean = errors.iter().sum::<u32>() as f32 / errors.len() as f32;
                        let max = *errors.iter().max().unwrap();
                        assert!(
                            mean <= 8.0 && max <= 32,
                            "{label} case {tag} ({encoding:?}) mip {mip} channel {channel}: mean={mean}, max={max}"
                        );
                    }
                }
            }
        }
    }

    /// The encoder shader parses and validates, so a broken shader fails here
    /// rather than on the first machine with a GPU.
    #[test]
    fn encoder_shader_validates() {
        use wgpu::naga;
        let module = naga::front::wgsl::parse_str(include_str!("uastc_encode.wgsl"))
            .unwrap_or_else(|error| panic!("{}", error.emit_to_string("uastc_encode.wgsl")));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .unwrap();
    }

    /// A full budget holds producers back until a release, and closing it
    /// releases them without reserving anything.
    #[test]
    fn byte_budget_blocks_until_released_or_closed() {
        let budget = ByteBudget::new(10);
        assert!(budget.acquire(8));
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.acquire(8));
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!waiting.is_finished());
            budget.release(8);
            assert!(waiting.join().unwrap());
        });
        // One item larger than the limit is still admitted when nothing waits.
        budget.release(8);
        assert!(budget.acquire(100));
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.acquire(1));
            std::thread::sleep(std::time::Duration::from_millis(50));
            budget.close();
            assert!(!waiting.join().unwrap());
        });
        assert!(!budget.acquire(0));
    }

    /// GPU-encoded textures get their own cache label, versioned by the shader.
    #[test]
    fn cache_label_names_encoder_version_and_quality() {
        assert_eq!(cache_label(2), format!(":gpu-uastc-v{ENCODER_VERSION}-q2"));
        assert_ne!(cache_label(2), cache_label(3));
    }
}
