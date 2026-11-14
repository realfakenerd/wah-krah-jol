//! GIF and H.264 encoding for a fully validated offscreen PNG sequence.
use color_eyre::eyre::{Result, ensure, eyre};
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_FFMPEG_LOG: u64 = 2 * 1024 * 1024;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FRAMES: usize = 3_600;

struct OwnedEncoder(Child);
impl Drop for OwnedEncoder {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[derive(Clone)]
struct Sequence {
    width: u32,
    height: u32,
    fps: u32,
    seconds: u32,
    frames: Vec<PathBuf>,
}

fn decode_png(path: &Path, width: Option<u32>, height: Option<u32>) -> Result<DynamicImage> {
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    ensure!(
        reader.format() == Some(ImageFormat::Png),
        "{} is not a PNG",
        path.display()
    );
    if let (Some(width), Some(height)) = (width, height) {
        let max_alloc = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| eyre!("PNG allocation limit overflow"))?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some(max_alloc);
        reader.limits(limits);
    }
    let image = reader
        .decode()
        .map_err(|error| eyre!("PNG decode failed for {}: {error}", path.display()))?;
    if let (Some(width), Some(height)) = (width, height) {
        ensure!(
            (image.width(), image.height()) == (width, height),
            "PNG dimensions differ from requested offscreen size"
        );
    }
    Ok(image)
}

fn verify_sequence(dir: &Path, fps: u32, seconds: u32, video: bool) -> Result<Sequence> {
    let manifest_path = dir.join("manifest.json");
    let manifest: Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| eyre!("verified capture manifest is unavailable: {error}"))?,
    )?;
    let identity = manifest
        .get("identity")
        .ok_or_else(|| eyre!("capture manifest has no identity"))?;
    ensure!(
        identity.get("protocol").and_then(Value::as_str) == Some(super::PROTOCOL),
        "capture manifest protocol mismatch"
    );
    ensure!(
        identity.get("version").and_then(Value::as_u64) == Some(1),
        "capture manifest version mismatch"
    );
    ensure!(
        identity
            .get("source_identity")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        "capture manifest source identity is invalid"
    );
    let width = u32::try_from(
        identity
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| eyre!("capture manifest width is invalid"))?,
    )?;
    let height = u32::try_from(
        identity
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| eyre!("capture manifest height is invalid"))?,
    )?;
    ensure!(
        seconds > 0 && seconds <= super::MAX_SECONDS && fps > 0 && fps <= super::MAX_FPS,
        "encoder timing is outside supported limits"
    );
    let requested_count = usize::try_from(
        u64::from(fps)
            .checked_mul(u64::from(seconds))
            .ok_or_else(|| eyre!("frame count overflow"))?,
    )?;
    ensure!(
        requested_count > 0 && requested_count <= MAX_FRAMES,
        "frame count exceeds encoder limit"
    );
    let entries = manifest
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(|| eyre!("capture manifest frame list is invalid"))?;
    ensure!(
        entries.len() == requested_count,
        "capture manifest frame count differs from requested timing"
    );
    ensure!(
        width > 0
            && height > 0
            && u64::from(width) * u64::from(height) <= super::ANIMALS_MAX_PIXELS,
        "capture manifest dimensions are invalid"
    );
    let mut total_bytes = 0u64;
    let mut frames = Vec::with_capacity(requested_count);
    for (index, entry) in entries.iter().enumerate() {
        let name = format!("frame-{index:06}.png");
        ensure!(
            entry.get("index").and_then(Value::as_u64) == Some(index as u64),
            "capture manifest indices are not contiguous"
        );
        ensure!(
            entry.get("png").and_then(Value::as_str) == Some(name.as_str()),
            "capture manifest PNG path is not the expected numbered path"
        );
        ensure!(
            entry.get("time_numerator").and_then(Value::as_u64) == Some(index as u64)
                && entry.get("time_denominator").and_then(Value::as_u64) == Some(u64::from(fps)),
            "capture manifest requested time differs from export timing"
        );
        ensure!(
            entry.get("duration_numerator").and_then(Value::as_u64) == Some(1)
                && entry.get("duration_denominator").and_then(Value::as_u64)
                    == Some(u64::from(fps)),
            "capture manifest frame hold differs from export timing"
        );
        let path = dir.join(&name);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "PNG is not an owned regular file: {name}"
        );
        ensure!(
            entry.get("bytes").and_then(Value::as_u64) == Some(metadata.len())
                && metadata.len() > 0,
            "PNG size differs from capture receipt: {name}"
        );
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| eyre!("PNG byte count overflow"))?;
        ensure!(
            total_bytes <= super::ANIMALS_MAX_PNG_BYTES,
            "verified PNG set exceeds 512 MiB budget"
        );
        let image = decode_png(&path, Some(width), Some(height))?;
        if video {
            ensure!(
                image.to_rgba8().pixels().all(|pixel| pixel[3] == 255),
                "H.264 export requires opaque offscreen frames"
            );
        }
        drop(image);
        frames.push(path);
    }
    let mut actual = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            actual.push(item.file_name());
        }
    }
    actual.sort();
    let expected: Vec<_> = (0..requested_count)
        .map(|index| OsString::from(format!("frame-{index:06}.png")))
        .collect();
    ensure!(
        actual == expected,
        "PNG set is not exactly the contiguous manifest sequence"
    );
    Ok(Sequence {
        width,
        height,
        fps,
        seconds,
        frames,
    })
}

fn ffmpeg_path_from(path: Option<&std::ffi::OsStr>, homebrew_ffmpeg: &Path) -> Option<PathBuf> {
    path.map(OsString::from)
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("ffmpeg"))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| {
            homebrew_ffmpeg
                .is_file()
                .then(|| homebrew_ffmpeg.to_path_buf())
        })
}

fn run_ffmpeg(dir: &Path, ffmpeg: PathBuf, log_name: &str, args: &[OsString]) -> Result<()> {
    let log = dir.join(log_name);
    let log_file = OpenOptions::new().write(true).create_new(true).open(&log)?;
    let mut child = OwnedEncoder(
        Command::new(ffmpeg)
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()?,
    );
    let start = Instant::now();
    loop {
        ensure!(
            fs::metadata(&log)?.len() <= MAX_FFMPEG_LOG,
            "ffmpeg log exceeded 2 MiB; encoder stopped and PNGs were retained"
        );
        if let Some(status) = child.0.try_wait()? {
            ensure!(
                status.success(),
                "ffmpeg failed; verified PNGs were retained"
            );
            break;
        }
        ensure!(
            start.elapsed() < FFMPEG_TIMEOUT,
            "ffmpeg exceeded 120 second timeout; encoder stopped and PNGs were retained"
        );
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn create_exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn gif_delay_centiseconds(index: usize, fps: u32) -> u16 {
    let boundary =
        |frame: usize| ((frame as u64 * 100 + u64::from(fps) / 2) / u64::from(fps)) as u16;
    boundary(index + 1) - boundary(index)
}

fn encode_gif_rust(dir: &Path, sequence: &Sequence) -> Result<()> {
    ensure!(
        sequence.width <= u32::from(u16::MAX) && sequence.height <= u32::from(u16::MAX),
        "GIF dimensions exceed format limits"
    );
    let partial = dir.join("animation.gif.partial");
    let final_path = dir.join("animation.gif");
    ensure!(
        !partial.exists() && !final_path.exists(),
        "GIF output path already exists"
    );
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let mut encoder = GifEncoder::new(file, sequence.width as u16, sequence.height as u16, &[])?;
    encoder.set_repeat(Repeat::Infinite)?;
    for (index, path) in sequence.frames.iter().enumerate() {
        let mut rgba = decode_png(path, Some(sequence.width), Some(sequence.height))?
            .to_rgba8()
            .into_raw();
        let mut frame =
            GifFrame::from_rgba_speed(sequence.width as u16, sequence.height as u16, &mut rgba, 10);
        frame.delay = gif_delay_centiseconds(index, sequence.fps);
        encoder.write_frame(&frame)?;
    }
    drop(encoder);
    fs::rename(partial, final_path)?;
    Ok(())
}

fn gif_ffmpeg_args(sequence: &Sequence) -> Vec<OsString> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-framerate",
    ]
    .into_iter()
    .map(OsString::from)
    .chain(
        [
            sequence.fps.to_string(),
            "-start_number".into(),
            "0".into(),
            "-i".into(),
            "frame-%06d.png".into(),
            "-filter_complex".into(),
            "[0:v]split[a][b];[a]palettegen[p];[b][p]paletteuse".into(),
            "-fps_mode".into(),
            "vfr".into(),
            "-loop".into(),
            "0".into(),
            "-f".into(),
            "gif".into(),
            "animation.gif.partial".into(),
        ]
        .into_iter()
        .map(OsString::from),
    )
    .collect()
}

pub fn encode_gif(dir: &Path, fps: u32, seconds: u32) -> Result<&'static str> {
    encode_gif_with_ffmpeg_search(
        dir,
        fps,
        seconds,
        std::env::var_os("PATH").as_deref(),
        Path::new("/opt/homebrew/bin/ffmpeg"),
    )
}

fn encode_gif_with_ffmpeg_search(
    dir: &Path,
    fps: u32,
    seconds: u32,
    path: Option<&std::ffi::OsStr>,
    homebrew_ffmpeg: &Path,
) -> Result<&'static str> {
    encode_gif_with_ffmpeg(dir, fps, seconds, ffmpeg_path_from(path, homebrew_ffmpeg))
}

fn encode_gif_with_ffmpeg(
    dir: &Path,
    fps: u32,
    seconds: u32,
    ffmpeg: Option<PathBuf>,
) -> Result<&'static str> {
    let sequence = verify_sequence(dir, fps, seconds, false)?;
    let final_path = dir.join("animation.gif");
    let partial = dir.join("animation.gif.partial");
    ensure!(
        !final_path.exists() && !partial.exists(),
        "GIF output path already exists"
    );
    if let Some(ffmpeg) = ffmpeg {
        run_ffmpeg(dir, ffmpeg, "ffmpeg-gif.log", &gif_ffmpeg_args(&sequence))?;
        fs::rename(partial, final_path)?;
        Ok("ffmpeg palettegen/paletteuse")
    } else {
        encode_gif_rust(dir, &sequence)?;
        Ok("pure Rust gif 0.13.3")
    }
}

fn validate_h264_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "H.264 requires even physical dimensions; no padding was applied"
    );
    Ok(())
}

pub fn encode_video(dir: &Path, fps: u32, seconds: u32, format: &str) -> Result<&'static str> {
    encode_video_with_ffmpeg_search(
        dir,
        fps,
        seconds,
        format,
        std::env::var_os("PATH").as_deref(),
        Path::new("/opt/homebrew/bin/ffmpeg"),
    )
}

fn encode_video_with_ffmpeg_search(
    dir: &Path,
    fps: u32,
    seconds: u32,
    format: &str,
    path: Option<&std::ffi::OsStr>,
    homebrew_ffmpeg: &Path,
) -> Result<&'static str> {
    encode_video_with_ffmpeg(
        dir,
        fps,
        seconds,
        format,
        ffmpeg_path_from(path, homebrew_ffmpeg),
    )
}

fn encode_video_with_ffmpeg(
    dir: &Path,
    fps: u32,
    seconds: u32,
    format: &str,
    ffmpeg: Option<PathBuf>,
) -> Result<&'static str> {
    ensure!(
        matches!(format, "mp4" | "mov"),
        "video format must be mp4 or mov"
    );
    let sequence = verify_sequence(dir, fps, seconds, true)?;
    validate_h264_dimensions(sequence.width, sequence.height)?;
    let ffmpeg = ffmpeg.ok_or_else(|| {
        eyre!(
            "ffmpeg unavailable on PATH or at /opt/homebrew/bin/ffmpeg; verified PNGs were retained"
        )
    })?;
    let partial_name = format!("animation.{format}.partial");
    let partial = dir.join(&partial_name);
    let final_path = dir.join(format!("animation.{format}"));
    ensure!(
        !partial.exists() && !final_path.exists(),
        "video output path already exists"
    );
    let args: Vec<OsString> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-framerate",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([
        fps.to_string().into(),
        "-start_number".into(),
        "0".into(),
        "-i".into(),
        "frame-%06d.png".into(),
        "-frames:v".into(),
        (sequence.fps * sequence.seconds).to_string().into(),
        "-an".into(),
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-r".into(),
        fps.to_string().into(),
        "-f".into(),
        format.into(),
        partial_name.into(),
    ])
    .collect();
    run_ffmpeg(dir, ffmpeg, "ffmpeg-video.log", &args)?;
    fs::rename(partial, final_path)?;
    Ok("ffmpeg libx264")
}

pub fn write_encoding_manifest(
    dir: &Path,
    fps: u32,
    seconds: u32,
    kind: &str,
    format: Option<&str>,
    encoder: &str,
) -> Result<()> {
    let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    let frames = manifest
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(|| eyre!("capture manifest is missing frame timing"))?;
    let last = frames
        .last()
        .ok_or_else(|| eyre!("capture manifest has no final hold"))?;
    let requested_final_hold = json!({
        "numerator": last.get("duration_numerator").and_then(Value::as_u64).ok_or_else(|| eyre!("final hold numerator is invalid"))?,
        "denominator": last.get("duration_denominator").and_then(Value::as_u64).ok_or_else(|| eyre!("final hold denominator is invalid"))?
    });
    let encoded = if kind == "gif" {
        let gif_delays: Vec<u16> = (0..frames.len())
            .map(|index| gif_delay_centiseconds(index, fps))
            .collect();
        json!({ "file":"animation.gif", "encoder":encoder, "frame_delay_centiseconds":gif_delays, "timing_quantum_ms":10, "timing_note":"Frame delays are computed from the requested rate with cumulative nearest-tick rounding; encoded timing is not measured from the output file." })
    } else {
        let format = format.ok_or_else(|| eyre!("video manifest has no selected container"))?;
        json!({ "file":format!("animation.{format}"), "encoder":encoder, "codec":"H.264 libx264", "pixel_format":"yuv420p", "nominal_fps":fps, "timing_note":"CFR nominal rate is requested; container time-base quantization may affect timestamps. Encoded timing is not measured from the output file." })
    };
    let payload = json!({
        "schema": 1,
        "capture_manifest": "manifest.json",
        "requested": { "duration_seconds": seconds, "fps": {"numerator":fps,"denominator":1}, "frame_count":frames.len(), "frame_times":"nominal samples for a static pose", "final_hold":requested_final_hold },
        "encoded": encoded
    });
    create_exclusive(
        &dir.join("encoding-manifest.json"),
        &serde_json::to_vec_pretty(&payload)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn fixture(fps: u32, seconds: u32, width: u32, height: u32) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let mut frames = Vec::new();
        for index in 0..fps * seconds {
            let path = temp.path().join(format!("frame-{index:06}.png"));
            image::RgbaImage::from_pixel(width, height, image::Rgba([index as u8, 20, 30, 255]))
                .save(&path)
                .unwrap();
            frames.push(json!({
                "index":index,
                "time_numerator":index,
                "time_denominator":fps,
                "duration_numerator":1,
                "duration_denominator":fps,
                "png":path.file_name().unwrap().to_string_lossy(),
                "bytes":fs::metadata(&path).unwrap().len()
            }));
        }
        let manifest = json!({
            "identity":{"protocol":"mudcrab-actor-idle-step-v1","version":1,"source_identity":"fixture","width":width,"height":height},
            "frames":frames
        });
        fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        temp
    }

    #[test]
    fn missing_ffmpeg_fails_video_encode_and_preserves_verified_pngs() {
        let temp = fixture(2, 1, 4, 2);
        let path = tempfile::tempdir().unwrap();
        let missing_homebrew_ffmpeg = temp.path().join("absent-homebrew/ffmpeg");
        let frame_paths = ["frame-000000.png", "frame-000001.png"];
        let original: Vec<_> = frame_paths
            .iter()
            .map(|name| fs::read(temp.path().join(name)).unwrap())
            .collect();

        let error = encode_video_with_ffmpeg_search(
            temp.path(),
            2,
            1,
            "mp4",
            Some(path.path().as_os_str()),
            &missing_homebrew_ffmpeg,
        )
        .unwrap_err();

        assert!(error.to_string().contains(
            "ffmpeg unavailable on PATH or at /opt/homebrew/bin/ffmpeg; verified PNGs were retained"
        ));
        for (name, bytes) in frame_paths.iter().zip(original) {
            assert_eq!(fs::read(temp.path().join(name)).unwrap(), bytes);
        }
        assert!(!temp.path().join("animation.mp4").exists());
    }

    #[test]
    fn missing_ffmpeg_uses_rust_gif_fallback() {
        let temp = fixture(2, 1, 4, 2);
        let path = tempfile::tempdir().unwrap();
        let missing_homebrew_ffmpeg = temp.path().join("absent-homebrew/ffmpeg");
        let encoder = encode_gif_with_ffmpeg_search(
            temp.path(),
            2,
            1,
            Some(path.path().as_os_str()),
            &missing_homebrew_ffmpeg,
        )
        .unwrap();

        assert_eq!(encoder, "pure Rust gif 0.13.3");
        assert!(temp.path().join("animation.gif").is_file());
        assert!(verify_sequence(temp.path(), 2, 1, false).is_ok());
    }

    #[test]
    fn rust_gif_fallback_decodes_all_frames_and_records_centisecond_quantization() {
        let temp = fixture(30, 1, 4, 2);
        let sequence = verify_sequence(temp.path(), 30, 1, false).unwrap();
        encode_gif_rust(temp.path(), &sequence).unwrap();
        let options = gif::DecodeOptions::new();
        let mut decoder = options
            .read_info(File::open(temp.path().join("animation.gif")).unwrap())
            .unwrap();
        let mut delays = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            delays.push(frame.delay);
        }
        assert_eq!(delays.len(), 30);
        assert_eq!(
            delays.iter().map(|delay| u32::from(*delay)).sum::<u32>(),
            100
        );
        assert!(delays.iter().all(|delay| *delay == 3 || *delay == 4));
        write_encoding_manifest(temp.path(), 30, 1, "gif", None, "pure Rust gif 0.13.3").unwrap();
        let report: Value =
            serde_json::from_slice(&fs::read(temp.path().join("encoding-manifest.json")).unwrap())
                .unwrap();
        assert_eq!(report["requested"]["fps"]["denominator"], 1);
        assert_eq!(report["encoded"]["timing_quantum_ms"], 10);
        assert_eq!(report["requested"]["final_hold"]["numerator"], 1);
    }

    #[test]
    fn encoding_manifest_records_fps_and_final_frame_hold_separately() {
        let temp = fixture(2, 1, 4, 2);
        write_encoding_manifest(temp.path(), 2, 1, "gif", None, "fixture encoder").unwrap();
        let report: Value =
            serde_json::from_slice(&fs::read(temp.path().join("encoding-manifest.json")).unwrap())
                .unwrap();
        assert_eq!(report["requested"]["fps"]["numerator"], 2);
        assert_eq!(report["requested"]["fps"]["denominator"], 1);
        assert_eq!(report["requested"]["final_hold"]["numerator"], 1);
        assert_eq!(report["requested"]["final_hold"]["denominator"], 2);
    }

    #[test]
    fn preflight_decode_rejects_corrupt_or_extra_pngs_before_encoding() {
        let temp = fixture(2, 1, 4, 2);
        let corrupt = temp.path().join("frame-000001.png");
        let length = fs::metadata(&corrupt).unwrap().len() as usize;
        fs::write(corrupt, vec![0u8; length]).unwrap();
        assert!(verify_sequence(temp.path(), 2, 1, false).is_err());
        let temp = fixture(2, 1, 4, 2);
        fs::copy(
            temp.path().join("frame-000000.png"),
            temp.path().join("extra.png"),
        )
        .unwrap();
        assert!(verify_sequence(temp.path(), 2, 1, false).is_err());
    }

    #[test]
    fn h264_requires_even_physical_dimensions_without_padding() {
        assert!(validate_h264_dimensions(640, 480).is_ok());
        assert!(validate_h264_dimensions(641, 480).is_err());
        assert!(validate_h264_dimensions(640, 481).is_err());
    }
}
