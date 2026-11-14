//! Finite sequencing, capture protocol validation, and PNG manifest support.
//!
//! The caller owns scene loading, pose sampling, and screenshot readback. A capture is
//! complete only after `ScreenshotCaptured` and an exclusive PNG write have both finished.

pub mod encoder;

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub const PROTOCOL: &str = "mudcrab-actor-idle-step-v1";
pub const MAX_SECONDS: u32 = 60;
pub const MAX_FPS: u32 = 60;
pub const MAX_FRAMES: u32 = 3_600;
pub const ANIMALS_MAX_PIXELS: u64 = 4_096_000;
pub const ANIMALS_MAX_PNG_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub numerator: u64,
    pub denominator: u64,
}

impl Rational {
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, ExportError> {
        if denominator == 0 {
            return Err(ExportError::Invalid("zero rational denominator"));
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    /// Generate exact logical sample times n/fps in [0, duration).
    pub fn samples(duration: Self, fps: u32) -> Result<Vec<Self>, ExportError> {
        if fps == 0 || duration.denominator == 0 || duration.numerator == 0 {
            return Err(ExportError::Invalid("duration and fps must be positive"));
        }
        let scaled = duration
            .numerator
            .checked_mul(u64::from(fps))
            .ok_or(ExportError::Overflow)?;
        let count = scaled / duration.denominator + u64::from(scaled % duration.denominator != 0);
        if count > u64::from(MAX_FRAMES) {
            return Err(ExportError::Limit("frame count exceeds 3600"));
        }
        let endpoint = duration.numerator as f64 / duration.denominator as f64;
        (0..count)
            .map(|n| {
                let seek = n as f64 / f64::from(fps);
                if (seek as f32) as f64 >= endpoint {
                    return Err(ExportError::Invalid("f32 seek rounds to clip endpoint"));
                }
                Ok(Self {
                    numerator: n,
                    denominator: u64::from(fps),
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportRequest {
    pub duration: Rational,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub png_budget: u64,
}

impl ExportRequest {
    pub fn validate(self) -> Result<Vec<Rational>, ExportError> {
        if self.duration.denominator == 0 || self.duration.numerator == 0 {
            return Err(ExportError::Invalid("duration must be positive"));
        }
        let max_duration = u64::from(MAX_SECONDS)
            .checked_mul(self.duration.denominator)
            .ok_or(ExportError::Overflow)?;
        if self.duration.numerator > max_duration {
            return Err(ExportError::Limit("duration exceeds 60 seconds"));
        }
        if self.fps == 0 || self.fps > MAX_FPS {
            return Err(ExportError::Limit("fps must be between 1 and 60"));
        }
        let pixels = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .ok_or(ExportError::Overflow)?;
        if self.width == 0 || self.height == 0 || pixels > ANIMALS_MAX_PIXELS {
            return Err(ExportError::Limit(
                "dimensions exceed Animals 4096000-pixel limit",
            ));
        }
        if self.png_budget == 0 || self.png_budget > ANIMALS_MAX_PNG_BYTES {
            return Err(ExportError::Limit(
                "PNG budget exceeds Animals 512 MiB limit",
            ));
        }
        Rational::samples(self.duration, self.fps)
    }
}

#[derive(Debug)]
pub enum ExportError {
    Invalid(&'static str),
    Limit(&'static str),
    Overflow,
    Sequence(&'static str),
    Io(std::io::Error),
}
impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid export request: {m}"),
            Self::Limit(m) => write!(f, "export limit exceeded: {m}"),
            Self::Overflow => f.write_str("integer overflow while calculating export bounds"),
            Self::Sequence(m) => write!(f, "capture sequence error: {m}"),
            Self::Io(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ExportError {}
impl From<std::io::Error> for ExportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureIdentity {
    pub protocol: String,
    pub version: u32,
    pub source_identity: String,
    pub width: u32,
    pub height: u32,
}
impl CaptureIdentity {
    pub fn validate(&self) -> Result<(), ExportError> {
        if self.protocol != PROTOCOL || self.version != 1 {
            return Err(ExportError::Invalid(
                "unsupported capture protocol or version",
            ));
        }
        if self.source_identity.is_empty() {
            return Err(ExportError::Invalid("empty source identity"));
        }
        let pixels = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .ok_or(ExportError::Overflow)?;
        if self.width == 0 || self.height == 0 || pixels > ANIMALS_MAX_PIXELS {
            return Err(ExportError::Limit(
                "capture dimensions exceed 4096000 pixels",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Step,
    Capture,
    Finish,
}

/// Wire command. Sequence starts at one and is independent of the zero-based frame index.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WireCommand {
    pub sequence: u32,
    pub op: Operation,
    pub frame_index: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub capture: Option<bool>,
}
impl WireCommand {
    pub fn validate(&self) -> Result<(), ExportError> {
        if self.sequence == 0 {
            return Err(ExportError::Sequence("sequence must start at one"));
        }
        if self.op == Operation::Finish && self.capture == Some(true) {
            return Err(ExportError::Sequence(
                "finish command cannot request capture",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckState {
    PoseReady,
    Captured,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureAck {
    pub protocol: String,
    pub version: u32,
    pub source_identity: String,
    pub sequence: u32,
    pub frame_index: u32,
    pub state: AckState,
    pub png_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub png_readback_and_write_completed: bool,
    pub seek: Rational,
}
impl CaptureAck {
    pub fn validate_captured(
        &self,
        identity: &CaptureIdentity,
        command: &WireCommand,
        expected_seek: Rational,
        expected_png_path: &Path,
    ) -> Result<(), ExportError> {
        if self.protocol != identity.protocol
            || self.version != identity.version
            || self.source_identity != identity.source_identity
            || self.sequence != command.sequence
            || self.frame_index != command.frame_index
        {
            return Err(ExportError::Sequence(
                "acknowledgement identity, sequence, or frame index mismatch",
            ));
        }
        if self.state != AckState::Captured || !self.png_readback_and_write_completed {
            return Err(ExportError::Sequence(
                "pose-ready is not a captured acknowledgement",
            ));
        }
        let seek_matches = self.seek.denominator != 0
            && u128::from(self.seek.numerator) * u128::from(expected_seek.denominator)
                == u128::from(expected_seek.numerator) * u128::from(self.seek.denominator);
        if self.png_path.as_deref() != Some(expected_png_path.to_string_lossy().as_ref())
            || self.width != identity.width
            || self.height != identity.height
            || !seek_matches
        {
            return Err(ExportError::Sequence(
                "acknowledgement path, dimensions, or seek mismatch",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestFrame {
    pub index: u32,
    pub time_numerator: u64,
    pub time_denominator: u64,
    pub duration_numerator: u64,
    pub duration_denominator: u64,
    pub png: String,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct CaptureSequence {
    directory: PathBuf,
    request: ExportRequest,
    times: Vec<Rational>,
    next: u32,
    next_command_sequence: u32,
    bytes: u64,
    identity: CaptureIdentity,
    manifest: Vec<ManifestFrame>,
    finish_command_issued: bool,
}

impl CaptureSequence {
    pub fn new(
        directory: impl Into<PathBuf>,
        request: ExportRequest,
        identity: CaptureIdentity,
    ) -> Result<Self, ExportError> {
        let times = request.validate()?;
        identity.validate()?;
        if identity.width != request.width || identity.height != request.height {
            return Err(ExportError::Invalid(
                "capture dimensions do not match request",
            ));
        }
        Ok(Self {
            directory: directory.into(),
            request,
            times,
            next: 0,
            next_command_sequence: 1,
            bytes: 0,
            identity,
            manifest: Vec::new(),
            finish_command_issued: false,
        })
    }
    pub fn next_index(&self) -> Option<u32> {
        (self.next < self.times.len() as u32).then_some(self.next)
    }
    pub fn identity(&self) -> &CaptureIdentity {
        &self.identity
    }
    /// Shorten a sequence to its completed prefix after any in-flight readback ends.
    pub fn truncate_to_captured(&mut self) -> Result<(), ExportError> {
        if self.next == 0 || self.next > self.times.len() as u32 {
            return Err(ExportError::Sequence(
                "cannot shorten before a frame is captured",
            ));
        }
        let frames = u64::from(self.next);
        self.times.truncate(self.next as usize);
        self.request.duration = Rational::new(frames, u64::from(self.request.fps))?;
        self.finish_command_issued = false;
        Ok(())
    }
    pub fn command(&mut self, operation: Operation) -> Result<WireCommand, ExportError> {
        let frame_index = match operation {
            Operation::Capture if self.next == 0 => 0,
            Operation::Step if self.next > 0 && self.next_index().is_some() => self.next,
            Operation::Finish if self.next_index().is_none() && !self.finish_command_issued => {
                self.finish_command_issued = true;
                self.next
            }
            _ => {
                return Err(ExportError::Sequence(
                    "operation is invalid for current frame",
                ));
            }
        };
        let cmd = WireCommand {
            sequence: self.next_command_sequence,
            op: operation,
            frame_index,
            capture: (operation == Operation::Step).then_some(true),
        };
        cmd.validate()?;
        self.next_command_sequence = self
            .next_command_sequence
            .checked_add(1)
            .ok_or(ExportError::Overflow)?;
        Ok(cmd)
    }
    pub fn complete_capture(
        &mut self,
        command: &WireCommand,
        ack: &CaptureAck,
        path: &Path,
    ) -> Result<(), ExportError> {
        command.validate()?;
        let index = self
            .next_index()
            .ok_or(ExportError::Sequence("capture sequence already complete"))?;
        if !matches!(command.op, Operation::Capture | Operation::Step)
            || command.capture == Some(false)
            || command.sequence.checked_add(1) != Some(self.next_command_sequence)
            || command.frame_index != index
        {
            return Err(ExportError::Sequence(
                "capture command is missing, duplicated, or out of order",
            ));
        }
        let expected = self.directory.join(format!("frame-{index:06}.png"));
        if path != expected || fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(ExportError::Sequence(
                "PNG path is foreign or not the expected numbered path",
            ));
        }
        ack.validate_captured(
            &self.identity,
            command,
            self.times[index as usize],
            &expected,
        )?;
        let bytes = fs::metadata(path)?.len();
        if bytes == 0 || bytes > self.request.png_budget.saturating_sub(self.bytes) {
            return Err(ExportError::Limit(
                "aggregate PNG budget exceeded or PNG is empty",
            ));
        }
        // Decode one frame at a time; this checks the complete PNG data and bounds retained memory.
        let max_alloc = u64::from(self.identity.width)
            .checked_mul(u64::from(self.identity.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ExportError::Overflow)?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(self.identity.width);
        limits.max_image_height = Some(self.identity.height);
        limits.max_alloc = Some(max_alloc);
        let mut dimension_reader = image::ImageReader::open(path)?.with_guessed_format()?;
        if dimension_reader.format() != Some(image::ImageFormat::Png) {
            return Err(ExportError::Sequence("captured file is not a PNG"));
        }
        dimension_reader.limits(limits.clone());
        let (width, height) = dimension_reader
            .into_dimensions()
            .map_err(|_| ExportError::Sequence("malformed PNG"))?;
        if width != self.identity.width || height != self.identity.height {
            return Err(ExportError::Sequence(
                "PNG dimensions differ from capture identity",
            ));
        }
        let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| ExportError::Sequence("malformed PNG"))?;
        drop(image);
        let time = self.times[index as usize];
        let (duration_numerator, duration_denominator) = if index + 1 < self.times.len() as u32 {
            (1, u64::from(self.request.fps))
        } else {
            let left = self
                .request
                .duration
                .numerator
                .checked_mul(u64::from(self.request.fps))
                .ok_or(ExportError::Overflow)?;
            let start = u64::from(index)
                .checked_mul(self.request.duration.denominator)
                .ok_or(ExportError::Overflow)?;
            (
                left.checked_sub(start).ok_or(ExportError::Overflow)?,
                u64::from(self.request.fps)
                    .checked_mul(self.request.duration.denominator)
                    .ok_or(ExportError::Overflow)?,
            )
        };
        self.bytes = self.bytes.checked_add(bytes).ok_or(ExportError::Overflow)?;
        self.manifest.push(ManifestFrame {
            index,
            time_numerator: time.numerator,
            time_denominator: time.denominator,
            duration_numerator,
            duration_denominator,
            png: expected.file_name().unwrap().to_string_lossy().into_owned(),
            bytes,
        });
        self.next += 1;
        Ok(())
    }
    pub fn finish(&self) -> Result<(), ExportError> {
        if self.next != self.times.len() as u32 || self.manifest.len() != self.times.len() {
            return Err(ExportError::Sequence(
                "expected contiguous complete PNG sequence",
            ));
        }
        let mut names = Vec::new();
        for item in fs::read_dir(&self.directory)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with("frame-") && name.ends_with(".png") {
                names.push(name);
            }
        }
        let expected: Vec<_> = (0..self.times.len() as u32)
            .map(|i| format!("frame-{i:06}.png"))
            .collect();
        names.sort();
        if names != expected {
            return Err(ExportError::Sequence("PNG set is not exact and contiguous"));
        }
        let payload = serde_json::to_vec_pretty(
            &serde_json::json!({"identity": self.identity, "frames": self.manifest}),
        )
        .map_err(|_| ExportError::Sequence("manifest serialization failed"))?;
        let path = self.directory.join("manifest.json");
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }
}

/// Exclusively create and flush a numbered PNG. Call only from the ScreenshotCaptured handler.
pub fn write_captured_png(
    directory: &Path,
    index: u32,
    bytes: &[u8],
) -> Result<PathBuf, ExportError> {
    let path = directory.join(format!("frame-{index:06}.png"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn req(duration: Rational, fps: u32) -> ExportRequest {
        ExportRequest {
            duration,
            fps,
            width: 4,
            height: 4,
            png_budget: ANIMALS_MAX_PNG_BYTES,
        }
    }
    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            protocol: PROTOCOL.into(),
            version: 1,
            source_identity: "fixture".into(),
            width: 4,
            height: 4,
        }
    }
    fn png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&[0u8; 64], 4, 4, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn rational_samples_are_half_open_and_keep_final_hold() {
        assert_eq!(
            Rational::samples(Rational::new(5, 2).unwrap(), 2).unwrap(),
            (0..5)
                .map(|n| Rational {
                    numerator: n,
                    denominator: 2
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            req(Rational::new(60, 1).unwrap(), 60)
                .validate()
                .unwrap()
                .len(),
            3600
        );
        assert!(Rational::new(1, 0).is_err());
    }
    #[test]
    fn rejects_overflow_and_bounds() {
        assert!(Rational::samples(Rational::new(u64::MAX, 1).unwrap(), 60).is_err());
        assert!(req(Rational::new(61, 1).unwrap(), 30).validate().is_err());
        let mut r = req(Rational::new(1, 1).unwrap(), 1);
        r.width = 4_097;
        r.height = 1_000;
        assert!(r.validate().is_err());
    }
    #[test]
    fn wire_command_has_only_requested_keys_and_rejects_unknown_fields() {
        let c = WireCommand {
            sequence: 1,
            op: Operation::Capture,
            frame_index: 0,
            capture: None,
        };
        let value = serde_json::to_value(&c).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"sequence":1,"op":"capture","frame_index":0})
        );
        WireCommand {
            sequence: 2,
            op: Operation::Step,
            frame_index: 1,
            capture: Some(true),
        }
        .validate()
        .unwrap();
        assert!(
            serde_json::from_str::<WireCommand>(
                r#"{"sequence":1,"op":"step","frame_index":0,"identity":"x"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn captured_ack_and_actual_png_are_required() {
        let dir = tempdir().unwrap();
        let mut seq =
            CaptureSequence::new(dir.path(), req(Rational::new(2, 1).unwrap(), 1), identity())
                .unwrap();
        let command = seq.command(Operation::Capture).unwrap();
        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            serde_json::json!({"sequence":1,"op":"capture","frame_index":0})
        );
        let path = write_captured_png(dir.path(), 0, &png()).unwrap();
        let ack = CaptureAck {
            protocol: PROTOCOL.into(),
            version: 1,
            source_identity: "fixture".into(),
            sequence: 1,
            frame_index: 0,
            state: AckState::PoseReady,
            png_path: Some(path.to_string_lossy().into_owned()),
            width: 4,
            height: 4,
            png_readback_and_write_completed: true,
            seek: Rational::new(0, 2).unwrap(),
        };
        assert!(seq.complete_capture(&command, &ack, &path).is_err());
        let ack = CaptureAck {
            state: AckState::Captured,
            ..ack
        };
        seq.complete_capture(&command, &ack, &path).unwrap();
        let next = seq.command(Operation::Step).unwrap();
        assert_eq!(next.sequence, 2);
        assert_eq!(next.frame_index, 1);
        assert_eq!(next.capture, Some(true));
        let path = write_captured_png(dir.path(), 1, &png()).unwrap();
        let ack = CaptureAck {
            protocol: PROTOCOL.into(),
            version: 1,
            source_identity: "fixture".into(),
            sequence: 2,
            frame_index: 1,
            state: AckState::Captured,
            png_path: Some(path.to_string_lossy().into_owned()),
            width: 4,
            height: 4,
            png_readback_and_write_completed: true,
            seek: Rational::new(1, 1).unwrap(),
        };
        seq.complete_capture(&next, &ack, &path).unwrap();
        let finish = seq.command(Operation::Finish).unwrap();
        assert_eq!(
            finish,
            WireCommand {
                sequence: 3,
                op: Operation::Finish,
                frame_index: 2,
                capture: None
            }
        );
        seq.finish().unwrap();
        assert!(dir.path().join("manifest.json").is_file());
    }

    #[test]
    fn shortened_sequence_finishes_completed_prefix_with_fixed_hold() {
        let dir = tempdir().unwrap();
        let mut seq =
            CaptureSequence::new(dir.path(), req(Rational::new(4, 1).unwrap(), 2), identity())
                .unwrap();
        let command = seq.command(Operation::Capture).unwrap();
        let path = write_captured_png(dir.path(), 0, &png()).unwrap();
        let ack = CaptureAck {
            protocol: PROTOCOL.into(),
            version: 1,
            source_identity: "fixture".into(),
            sequence: 1,
            frame_index: 0,
            state: AckState::Captured,
            png_path: Some(path.to_string_lossy().into_owned()),
            width: 4,
            height: 4,
            png_readback_and_write_completed: true,
            seek: Rational::new(0, 2).unwrap(),
        };
        seq.complete_capture(&command, &ack, &path).unwrap();
        seq.truncate_to_captured().unwrap();
        assert_eq!(seq.next_index(), None);
        seq.finish().unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["frames"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["frames"][0]["duration_numerator"], 1);
        assert_eq!(manifest["frames"][0]["duration_denominator"], 2);
    }

    #[test]
    fn cannot_shorten_before_first_capture() {
        let dir = tempdir().unwrap();
        let mut seq =
            CaptureSequence::new(dir.path(), req(Rational::new(2, 1).unwrap(), 1), identity())
                .unwrap();
        assert!(seq.truncate_to_captured().is_err());
    }
}
