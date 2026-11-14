//! Interactive inspection through the existing glTF/NIF material loader; no world startup.
use bevy::{
    camera::{Exposure, RenderTarget, primitives::Aabb},
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    gltf::GltfAssetLabel,
    prelude::*,
    render::{
        RenderPlugin,
        settings::{Backends, RenderCreation, WgpuFeatures, WgpuSettings},
        view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk},
    },
    world_serialization::{WorldAsset, WorldAssetRoot},
};
use color_eyre::eyre::{Result, ensure, eyre};
use engine::nif_material::{EffectPaletteMaterialPlugin, NifMaterialPlugin};
use rusqlite::{Connection, OpenFlags};
use std::{io::Cursor, path::PathBuf, time::Instant};
#[path = "model_inspector_export/mod.rs"]
mod export;
struct Input {
    model: String,
    label: String,
}
struct Options {
    assets: PathBuf,
    output: PathBuf,
    inputs: Vec<Input>,
    asset_lock: shared::asset_lock::AssetLock,
    export: Option<ExportOptions>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExportKind {
    Frames,
    Gif,
    Video,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Background {
    Transparent,
    Solid([u8; 3]),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoFormat {
    Mp4,
    Mov,
}
impl VideoFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExportOptions {
    kind: ExportKind,
    seconds: u32,
    fps: u32,
    width: u32,
    height: u32,
    orbit: bool,
    background: Background,
    video_format: VideoFormat,
}
fn arguments(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut args = args.into_iter();
    let mut assets = None;
    let mut output = None;
    let mut catalog = None;
    let mut models = Vec::new();
    let mut records = Vec::new();
    let mut export_kind = None;
    let mut seconds = 3u32;
    let mut fps = 30u32;
    let mut size = (1280u32, 800u32);
    let mut orbit = false;
    let mut background = Background::Solid([6, 8, 10]);
    let mut video_format = VideoFormat::Mp4;
    while let Some(flag) = args.next() {
        if flag == "--orbit" {
            ensure!(!orbit, "duplicate --orbit");
            orbit = true;
            continue;
        }
        if flag == "--export-frames" || flag == "--export-gif" || flag == "--export-video" {
            ensure!(export_kind.is_none(), "select only one export mode");
            export_kind = Some(match flag.as_str() {
                "--export-frames" => ExportKind::Frames,
                "--export-gif" => ExportKind::Gif,
                _ => ExportKind::Video,
            });
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| eyre!("{flag} requires a value"))?;
        match flag.as_str() {
            "--assets" => {
                ensure!(assets.is_none(), "duplicate --assets");
                assets = Some(PathBuf::from(value));
            }
            "--output" => {
                ensure!(output.is_none(), "duplicate --output");
                output = Some(PathBuf::from(value));
            }
            "--catalog" => {
                ensure!(catalog.is_none(), "duplicate --catalog");
                catalog = Some(PathBuf::from(value));
            }
            "--model" => models.push(value),
            "--record" => records.push(value),
            "--seconds" => seconds = value.parse()?,
            "--fps" => fps = value.parse()?,
            "--size" => {
                let (w, h) = value
                    .split_once('x')
                    .ok_or_else(|| eyre!("--size must be WIDTHxHEIGHT"))?;
                size = (w.parse()?, h.parse()?);
            }
            "--background" => {
                background = if value == "transparent" {
                    Background::Transparent
                } else {
                    let hex = value
                        .strip_prefix('#')
                        .ok_or_else(|| eyre!("background must be transparent or #RRGGBB"))?;
                    ensure!(
                        hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
                        "background must be #RRGGBB"
                    );
                    Background::Solid([
                        u8::from_str_radix(&hex[0..2], 16)?,
                        u8::from_str_radix(&hex[2..4], 16)?,
                        u8::from_str_radix(&hex[4..6], 16)?,
                    ])
                }
            }
            "--video-format" => {
                video_format = match value.as_str() {
                    "mp4" => VideoFormat::Mp4,
                    "mov" => VideoFormat::Mov,
                    _ => return Err(eyre!("--video-format must be mp4 or mov")),
                }
            }
            _ => return Err(eyre!("unknown option {flag}")),
        }
    }
    let assets = assets.ok_or_else(|| eyre!("--assets is required"))?;
    let output = output.ok_or_else(|| eyre!("--output is required"))?;
    ensure!(
        assets.is_absolute() && assets.is_dir(),
        "assets must be an existing absolute directory"
    );
    ensure!(
        output.is_absolute() && !output.exists(),
        "output must be a new absolute directory"
    );
    ensure!(
        !models.is_empty() || !records.is_empty(),
        "provide --model or --record"
    );
    ensure!(models.len() + records.len() <= 8, "at most eight models");
    let export = export_kind.map(|kind| ExportOptions {
        kind,
        seconds,
        fps,
        width: size.0,
        height: size.1,
        orbit,
        background,
        video_format,
    });
    if let Some(e) = export {
        ensure!(e.seconds > 0 && e.seconds <= 60, "seconds must be 1..=60");
        ensure!(e.fps > 0 && e.fps <= 60, "fps must be 1..=60");
        let pixels = u64::from(e.width)
            .checked_mul(u64::from(e.height))
            .ok_or_else(|| eyre!("size overflows"))?;
        ensure!(
            e.width > 0 && e.height > 0 && pixels <= export::ANIMALS_MAX_PIXELS,
            "size exceeds shared 4096000-pixel limit"
        );
        let frames = u64::from(e.seconds)
            .checked_mul(u64::from(e.fps))
            .ok_or_else(|| eyre!("frame count overflows"))?;
        let worst_case_png_bytes = pixels
            .checked_mul(4)
            .and_then(|n| n.checked_mul(frames))
            .ok_or_else(|| eyre!("PNG staging estimate overflows"))?;
        ensure!(
            frames <= u64::from(export::MAX_FRAMES)
                && worst_case_png_bytes <= export::ANIMALS_MAX_PNG_BYTES,
            "worst-case PNG staging exceeds shared 512 MiB limit"
        );
        if e.kind == ExportKind::Video {
            ensure!(
                matches!(e.background, Background::Solid(_)),
                "video requires a solid background"
            );
            ensure!(
                e.width % 2 == 0 && e.height % 2 == 0,
                "H.264 requires even physical dimensions; no padding is applied"
            );
        }
        export::ExportRequest {
            duration: export::Rational {
                numerator: u64::from(e.seconds),
                denominator: 1,
            },
            fps: e.fps,
            width: e.width,
            height: e.height,
            png_budget: export::ANIMALS_MAX_PNG_BYTES,
        }
        .validate()
        .map_err(|error| eyre!("{error}"))?;
    }
    let asset_lock = shared::asset_lock::AssetLock::acquire_shared(&assets)?;
    let assets = assets.canonicalize()?;
    let output_parent = output
        .parent()
        .ok_or_else(|| eyre!("output needs a parent"))?
        .canonicalize()?;
    ensure!(
        !output_parent.starts_with(&assets),
        "output cannot write into the asset directory"
    );
    let mut inputs = Vec::new();
    for model in models {
        inputs.push(Input {
            label: model.clone(),
            model,
        });
    }
    if !records.is_empty() {
        let database = catalog.unwrap_or_else(|| assets.join("skyrim_world.db"));
        ensure!(database.is_absolute(), "catalog must be absolute");
        let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        for record in records {
            let record = record
                .strip_prefix("0x")
                .or_else(|| record.strip_prefix("0X"))
                .unwrap_or(&record);
            let id = u32::from_str_radix(record, 16)?;
            let model: String = connection.query_row(
                "SELECT model_path FROM statics WHERE id=?1",
                [id],
                |row| row.get(0),
            )?;
            let model = converted_model_path(&model)
                .ok_or_else(|| eyre!("record {id:08X} has no safe converted model"))?;
            let has_swaps: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='base_texture_swaps')", [], |row| row.get(0))?;
            if has_swaps {
                let count: u32 = connection.query_row(
                    "SELECT count(*) FROM base_texture_swaps WHERE base_id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                ensure!(
                    count == 0,
                    "record {id:08X} has per-record texture swaps; this main-based tool cannot preview that appearance"
                );
            }
            inputs.push(Input {
                model,
                label: format!("base {id:08X}"),
            });
        }
    }
    for input in &inputs {
        ensure!(
            shared::streaming_costs::canonical_resource_key(&input.model)
                .is_ok_and(|key| key == input.model),
            "noncanonical model path: {}",
            input.model
        );
        ensure!(
            input.model.ends_with(".glb") || input.model.ends_with(".gltf"),
            "model must be glb/gltf"
        );
        let path = assets.join(&input.model).canonicalize()?;
        ensure!(
            path.starts_with(assets.canonicalize()?),
            "model escapes asset directory"
        );
    }
    Ok(Options {
        assets,
        output,
        inputs,
        asset_lock,
        export,
    })
}
/// Resolve a catalog MODL path using the ordinary lowercase meshes/*.glb convention.
fn converted_model_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let relative = normalized.strip_prefix("meshes/").unwrap_or(&normalized);
    if relative.is_empty() {
        return None;
    }
    let mut result = PathBuf::from("meshes").join(relative);
    result.set_extension("glb");
    let result = result.to_string_lossy().replace('\\', "/");
    shared::streaming_costs::canonical_resource_key(&result)
        .ok()
        .filter(|canonical| canonical == &result)
}

#[derive(Component)]
struct Slot(usize);
#[derive(Resource)]
struct Viewer {
    capture_width: u32,
    capture_height: u32,
    capture_fps: u32,
    scenes: Vec<Handle<WorldAsset>>,
    output: PathBuf,
    labels: String,
    center: Vec3,
    radius: f32,
    yaw: f32,
    pitch: f32,
    layout_done: bool,
    framed: bool,
    frames: u32,
    captures: u32,
    started: Instant,
    export: Option<ExportOptions>,
    capture_target: Option<Handle<Image>>,
    sequence: Option<export::CaptureSequence>,
    pending: bool,
    capture_error: Option<String>,
    pending_command: Option<export::WireCommand>,
    live_pending: bool,
    warmup_rendered: bool,
    pending_since: Option<Instant>,
    sequence_dir: PathBuf,
    recording_active: bool,
    recording_stop_requested: bool,
    recording_was_paused: Option<bool>,
}
const RECORDING_WIDTH: u32 = 640;
const RECORDING_HEIGHT: u32 = 400;
const RECORDING_FPS: u32 = 30;
const RECORDING_MAX_FRAMES: u32 = 120;
fn main() -> Result<()> {
    let options = arguments(std::env::args().skip(1))?;
    let export_request = options.export;
    let output_dir = options.output.clone();
    let _pack_lock = options.asset_lock;
    std::fs::create_dir(&options.output)?;
    let mut app = App::new();
    app.add_plugins(DefaultPlugins
  .set(AssetPlugin{file_path:options.assets.to_string_lossy().into_owned(),..default()})
  .set(WindowPlugin{primary_window:Some(Window{title:"Mudcrab model inspector | arrows orbit/zoom, W/S elevation, P pause, F12 capture, R record/stop, Esc close".into(),resolution:(1280,800).into(),..default()}),..default()})
  .set(RenderPlugin{render_creation:RenderCreation::Automatic(Box::new(WgpuSettings{backends:Some(Backends::METAL),features:WgpuFeatures::TEXTURE_COMPRESSION_BC,..default()})),..default()})
  .disable::<bevy::audio::AudioPlugin>())
  .add_plugins((EffectPaletteMaterialPlugin,NifMaterialPlugin))
  .insert_resource(ClearColor(Color::srgb(0.025,0.03,0.04)))
  .insert_resource(GlobalAmbientLight{brightness:40.0,..default()});
    let mut scenes = Vec::new();
    let mut labels = Vec::new();
    for (index, input) in options.inputs.into_iter().enumerate() {
        println!(
            "INSPECTOR_INPUT slot={index} model={} label={}",
            input.model, input.label
        );
        let scene = app
            .world()
            .resource::<AssetServer>()
            .load(GltfAssetLabel::Scene(0).from_asset(input.model));
        app.world_mut().spawn((
            WorldAssetRoot(scene.clone()),
            Transform::default(),
            Slot(index),
        ));
        scenes.push(scene);
        labels.push(input.label);
    }
    let (capture_width, capture_height) = export_request.map_or(
        (recording_settings().width, recording_settings().height),
        |e| (e.width, e.height),
    );
    let target = Some(app.world_mut().resource_mut::<Assets<Image>>().add(
        Image::new_target_texture(
            capture_width,
            capture_height,
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            None,
        ),
    ));
    let sequence = export_request.map(|e| {
        export::CaptureSequence::new(
            options.output.clone(),
            export::ExportRequest {
                duration: export::Rational {
                    numerator: u64::from(e.seconds),
                    denominator: 1,
                },
                fps: e.fps,
                width: e.width,
                height: e.height,
                png_budget: export::ANIMALS_MAX_PNG_BYTES,
            },
            export::CaptureIdentity {
                protocol: export::PROTOCOL.into(),
                version: 1,
                source_identity: "model-inspector-local-adapter".into(),
                width: e.width,
                height: e.height,
            },
        )
        .expect("validated export options")
    });
    app.insert_resource(Viewer {
        scenes,
        output: options.output.clone(),
        labels: labels.join(" | "),
        center: Vec3::ZERO,
        radius: 300.0,
        yaw: 0.6,
        pitch: 0.15,
        layout_done: false,
        framed: false,
        frames: 0,
        captures: 0,
        started: Instant::now(),
        export: export_request,
        capture_width,
        capture_height,
        capture_fps: export_request.map_or(RECORDING_FPS, |e| e.fps),
        capture_target: target,
        sequence,
        pending: false,
        capture_error: None,
        pending_command: None,
        live_pending: false,
        warmup_rendered: false,
        pending_since: None,
        sequence_dir: options.output.clone(),
        recording_active: false,
        recording_stop_requested: false,
        recording_was_paused: None,
    })
    .add_systems(Startup, setup)
    .add_systems(Update, update);
    ensure!(app.run().is_success(), "inspector failed; see native log");
    if let Some(export) = export_request {
        let encoded_by = match export.kind {
            ExportKind::Frames => None,
            ExportKind::Gif => Some(export::encoder::encode_gif(
                &output_dir,
                export.fps,
                export.seconds,
            )?),
            ExportKind::Video => Some(export::encoder::encode_video(
                &output_dir,
                export.fps,
                export.seconds,
                export.video_format.as_str(),
            )?),
        };
        if let Some(encoder) = encoded_by {
            export::encoder::write_encoding_manifest(
                &output_dir,
                export.fps,
                export.seconds,
                if export.kind == ExportKind::Gif {
                    "gif"
                } else {
                    "video"
                },
                (export.kind == ExportKind::Video).then_some(export.video_format.as_str()),
                encoder,
            )?;
        }
    }
    Ok(())
}
fn setup(mut commands: Commands, viewer: Res<Viewer>) {
    commands.spawn((
        Camera3d::default(),
        Exposure { ev100: 9.7 },
        Tonemapping::TonyMcMapface,
        DebandDither::Disabled,
        Transform::from_xyz(200., 100., 250.).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    if let Some(target) = &viewer.capture_target {
        let clear = match viewer
            .export
            .map(|opts| opts.background)
            .unwrap_or(Background::Solid([6, 8, 10]))
        {
            Background::Transparent => Color::NONE,
            Background::Solid([r, g, b]) => Color::srgb_u8(r, g, b),
        };
        commands.spawn((
            Camera3d::default(),
            Exposure { ev100: 9.7 },
            Tonemapping::TonyMcMapface,
            DebandDither::Disabled,
            RenderTarget::Image(target.clone().into()),
            Camera {
                clear_color: bevy::camera::ClearColorConfig::Custom(clear),
                ..default()
            },
            Transform::from_xyz(200., 100., 250.).looking_at(Vec3::ZERO, Vec3::Y),
        ));
    }
    commands.spawn((
        DirectionalLight {
            illuminance: 5000.,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(100., 200., 150.).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((Text::new(format!("{}\nArrows: orbit/zoom  W/S: elevation  Space: fill  P: pause  F12: capture  R: record/stop (640x400, 30 fps, 120 frame cap)  Esc: close",viewer.labels)),TextFont{font_size:FontSize::Px(18.),..default()},TextColor(Color::WHITE),Node{position_type:PositionType::Absolute,left:px(16),top:px(16),..default()}));
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecordingSettings {
    width: u32,
    height: u32,
    fps: u32,
    max_frames: u32,
}
const fn recording_settings() -> RecordingSettings {
    RecordingSettings {
        width: RECORDING_WIDTH,
        height: RECORDING_HEIGHT,
        fps: RECORDING_FPS,
        max_frames: RECORDING_MAX_FRAMES,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingStopAction {
    WaitForReadback,
    DiscardEmpty,
    FinishCapturedPrefix,
}
fn recording_stop_action(captured_frames: u32, readback_pending: bool) -> RecordingStopAction {
    if readback_pending {
        RecordingStopAction::WaitForReadback
    } else if captured_frames == 0 {
        RecordingStopAction::DiscardEmpty
    } else {
        RecordingStopAction::FinishCapturedPrefix
    }
}
fn can_start_readback(export_pending: bool, live_pending: bool) -> bool {
    !export_pending && !live_pending
}
fn fixed_pose_controls_locked(export: bool, recording: bool) -> bool {
    export || recording
}
fn restore_recording_pause(viewer: &mut Viewer, virtual_time: &mut Time<Virtual>) {
    if let Some(was_paused) = viewer.recording_was_paused.take() {
        if was_paused {
            virtual_time.pause();
        } else {
            virtual_time.unpause();
        }
    }
}
fn create_recording_sequence(root: &std::path::Path) -> Result<(PathBuf, export::CaptureSequence)> {
    let settings = recording_settings();
    let mut directory = None;
    for index in 0u64.. {
        let candidate = root.join(format!("recording-{index:03}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                directory = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let directory =
        directory.ok_or_else(|| eyre!("could not allocate a unique recording directory"))?;
    let request = export::ExportRequest {
        duration: export::Rational::new(u64::from(settings.max_frames), u64::from(settings.fps))?,
        fps: settings.fps,
        width: settings.width,
        height: settings.height,
        png_budget: export::ANIMALS_MAX_PNG_BYTES,
    };
    let identity = export::CaptureIdentity {
        protocol: export::PROTOCOL.into(),
        version: 1,
        source_identity: "model-inspector-r-recording".into(),
        width: settings.width,
        height: settings.height,
    };
    Ok((
        directory.clone(),
        export::CaptureSequence::new(directory, request, identity)?,
    ))
}
#[allow(clippy::too_many_arguments)]
fn update(
    mut commands: Commands,
    mut viewer: ResMut<Viewer>,
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time<Real>>,
    mut virtual_time: ResMut<Time<Virtual>>,
    server: Res<AssetServer>,
    bounds: Query<(&Aabb, &GlobalTransform), With<Mesh3d>>,
    mut roots: Query<(&Slot, &mut Transform), Without<Camera3d>>,
    mut cameras: Query<&mut Transform, With<Camera3d>>,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut close_requests: MessageReader<bevy::window::WindowCloseRequested>,
    mut exit: MessageWriter<AppExit>,
) {
    if viewer.capture_error.is_some() {
        restore_recording_pause(&mut viewer, &mut virtual_time);
        exit.write(AppExit::error());
        return;
    }
    if viewer
        .pending_since
        .is_some_and(|since| since.elapsed() >= std::time::Duration::from_secs(30))
    {
        viewer.capture_error = Some("capture readback timed out after 30 seconds".into());
        eprintln!("INSPECTOR_CAPTURE_TIMEOUT: partial PNGs retained");
        exit.write(AppExit::error());
        return;
    }
    if viewer.started.elapsed().as_secs() >= 1800 {
        if viewer.export.is_some() || viewer.pending {
            eprintln!(
                "INSPECTOR_WATCHDOG_TIMEOUT: active capture did not finish within 30 minutes"
            );
            exit.write(AppExit::error());
        } else {
            exit.write(AppExit::Success);
        }
        return;
    }
    let closing = close_requests.read().next().is_some();
    if keys.just_pressed(KeyCode::Escape) || closing {
        if viewer.export.is_some() || viewer.recording_active {
            eprintln!(
                "INSPECTOR_CAPTURE_CANCELLED: PNG sequence is incomplete; partial frames retained"
            );
            exit.write(AppExit::error());
        } else {
            exit.write(AppExit::Success);
        }
        return;
    }
    if viewer.export.is_some() {
        virtual_time.pause();
    }
    for scene in &viewer.scenes {
        if let bevy::asset::LoadState::Failed(reason) = server.load_state(scene.id()) {
            eprintln!("INSPECTOR_LOAD_FAILED: {reason}");
            exit.write(AppExit::error());
            return;
        }
    }
    if !viewer.framed
        && viewer
            .scenes
            .iter()
            .all(|scene| server.is_loaded_with_dependencies(scene.id()))
        && !bounds.is_empty()
    {
        let mut low = Vec3::splat(f32::INFINITY);
        let mut high = Vec3::splat(f32::NEG_INFINITY);
        for (aabb, global) in &bounds {
            let center: Vec3 = aabb.center.into();
            let half: Vec3 = aabb.half_extents.into();
            for x in [-1., 1.] {
                for y in [-1., 1.] {
                    for z in [-1., 1.] {
                        let p = global.transform_point(center + half * Vec3::new(x, y, z));
                        low = low.min(p);
                        high = high.max(p);
                    }
                }
            }
        }
        if low.is_finite() && high.is_finite() {
            if !viewer.layout_done {
                let spacing = (high - low).length().max(0.01) * 1.2;
                let midpoint = (viewer.scenes.len() - 1) as f32 * 0.5;
                for (slot, mut transform) in &mut roots {
                    transform.translation.x = (slot.0 as f32 - midpoint) * spacing;
                }
                viewer.layout_done = true;
            } else {
                viewer.center = (low + high) * 0.5;
                viewer.radius = (high - low).length().max(0.01) * 1.1;
                viewer.framed = true;
                println!(
                    "INSPECTOR_READY models={} meshes={} center={:?} radius={}",
                    viewer.scenes.len(),
                    bounds.iter().count(),
                    viewer.center,
                    viewer.radius
                );
            }
        }
    }
    let dt = time.delta_secs();
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::ArrowLeft)
    {
        viewer.yaw -= dt;
    }
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::ArrowRight)
    {
        viewer.yaw += dt;
    }
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::KeyW)
    {
        viewer.pitch = (viewer.pitch + dt).min(1.5);
    }
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::KeyS)
    {
        viewer.pitch = (viewer.pitch - dt).max(-1.5);
    }
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::ArrowUp)
    {
        viewer.radius *= (-dt).exp();
    }
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.pressed(KeyCode::ArrowDown)
    {
        viewer.radius *= dt.exp();
    }
    viewer.radius = viewer.radius.clamp(0.01, 100000.);
    if !fixed_pose_controls_locked(viewer.export.is_some(), viewer.recording_active)
        && keys.just_pressed(KeyCode::Space)
    {
        ambient.brightness = if ambient.brightness == 0. { 40. } else { 0. };
    }
    if viewer.export.is_none() && !viewer.recording_active && keys.just_pressed(KeyCode::KeyP) {
        if virtual_time.is_paused() {
            virtual_time.unpause();
        } else {
            virtual_time.pause();
        }
    }
    if keys.just_pressed(KeyCode::KeyR) && viewer.export.is_none() {
        if viewer.recording_active {
            viewer.recording_stop_requested = true;
            println!("INSPECTOR_RECORDING_STOP_REQUESTED: waiting for the in-flight frame");
        } else if viewer.sequence.is_none() {
            match create_recording_sequence(&viewer.output) {
                Ok((directory, sequence)) => {
                    viewer.sequence = Some(sequence);
                    viewer.sequence_dir = directory.clone();
                    viewer.recording_active = true;
                    viewer.recording_stop_requested = false;
                    viewer.recording_was_paused = Some(virtual_time.is_paused());
                    println!(
                        "INSPECTOR_RECORDING_STARTED path={} fps={} max_frames={}",
                        directory.display(),
                        RECORDING_FPS,
                        RECORDING_MAX_FRAMES
                    );
                }
                Err(error) => {
                    viewer.capture_error = Some(error.to_string());
                    eprintln!("INSPECTOR_RECORDING_FAILED: {error}");
                }
            }
        }
    }
    if viewer.export.is_some() || viewer.recording_active {
        virtual_time.pause();
    }
    if let (Some(opts), Some(index)) = (
        viewer.export,
        viewer
            .sequence
            .as_ref()
            .and_then(export::CaptureSequence::next_index),
    ) && opts.orbit
    {
        viewer.yaw = 0.6 + std::f32::consts::TAU * index as f32 / (opts.fps * opts.seconds) as f32;
    }
    for mut camera in &mut cameras {
        let offset = Vec3::new(
            viewer.yaw.sin() * viewer.pitch.cos(),
            viewer.pitch.sin(),
            viewer.yaw.cos() * viewer.pitch.cos(),
        );
        *camera = Transform::from_translation(viewer.center + offset * viewer.radius)
            .looking_at(viewer.center, Vec3::Y);
    }
    if viewer.framed {
        viewer.frames += 1;
        if !viewer.warmup_rendered {
            viewer.warmup_rendered = true;
            return;
        }
        if viewer.export.is_some() || viewer.recording_active {
            if viewer.capture_error.is_none() {
                if viewer.recording_active && viewer.recording_stop_requested {
                    let captured = viewer.sequence.as_ref().map_or(0, |sequence| {
                        sequence.next_index().unwrap_or(RECORDING_MAX_FRAMES)
                    });
                    match recording_stop_action(captured, viewer.pending) {
                        RecordingStopAction::WaitForReadback => {}
                        RecordingStopAction::DiscardEmpty => {
                            println!(
                                "INSPECTOR_RECORDING_CANCELLED_EMPTY path={}",
                                viewer.sequence_dir.display()
                            );
                            viewer.sequence = None;
                            viewer.recording_active = false;
                            viewer.recording_stop_requested = false;
                            restore_recording_pause(&mut viewer, &mut virtual_time);
                        }
                        RecordingStopAction::FinishCapturedPrefix => {
                            let result = if let Some(sequence) = viewer.sequence.as_mut() {
                                sequence
                                    .truncate_to_captured()
                                    .and_then(|()| sequence.finish())
                            } else {
                                Err(export::ExportError::Sequence(
                                    "recording sequence is missing",
                                ))
                            };
                            match result {
                                Ok(()) => println!(
                                    "INSPECTOR_RECORDING_COMPLETE path={}",
                                    viewer.sequence_dir.display()
                                ),
                                Err(error) => {
                                    viewer.capture_error = Some(error.to_string());
                                    eprintln!(
                                        "INSPECTOR_RECORDING_PARTIAL path={} error={error}",
                                        viewer.sequence_dir.display()
                                    );
                                }
                            }
                            viewer.sequence = None;
                            viewer.recording_active = false;
                            viewer.recording_stop_requested = false;
                            if viewer.capture_error.is_none() {
                                restore_recording_pause(&mut viewer, &mut virtual_time);
                            }
                        }
                    }
                } else if can_start_readback(viewer.pending, viewer.live_pending) {
                    let sequence = viewer.sequence.as_mut().unwrap();
                    if let Some(index) = sequence.next_index() {
                        let command = sequence
                            .command(if index == 0 {
                                export::Operation::Capture
                            } else {
                                export::Operation::Step
                            })
                            .unwrap();
                        viewer.pending_command = Some(command);
                        viewer.pending = true;
                        viewer.pending_since = Some(Instant::now());
                        virtual_time.pause();
                        if let Some(target) = &viewer.capture_target {
                            commands
                                .spawn(Screenshot::image(target.clone()))
                                .observe(export_capture_complete);
                        }
                    } else {
                        match sequence.finish() {
                            Ok(()) if viewer.recording_active => {
                                println!(
                                    "INSPECTOR_RECORDING_COMPLETE path={}",
                                    viewer.sequence_dir.display()
                                );
                                viewer.sequence = None;
                                viewer.recording_active = false;
                                restore_recording_pause(&mut viewer, &mut virtual_time);
                            }
                            Ok(()) => {
                                println!(
                                    "INSPECTOR_EXPORT_FRAMES_COMPLETE path={}",
                                    viewer.output.display()
                                );
                                exit.write(AppExit::Success);
                            }
                            Err(error) => {
                                viewer.capture_error = Some(error.to_string());
                                eprintln!("INSPECTOR_EXPORT_FAILED partial PNGs retained: {error}");
                                exit.write(AppExit::error());
                            }
                        }
                    }
                }
            }
        } else if can_start_readback(viewer.pending, viewer.live_pending)
            && (viewer.frames == 180 || viewer.frames == 480 || keys.just_pressed(KeyCode::F12))
        {
            viewer.live_pending = true;
            viewer.pending_since = Some(Instant::now());
            let path = viewer
                .output
                .join(format!("model-{:03}.png", viewer.captures));
            viewer.captures += 1;
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path))
                .observe(normal_capture_complete);
        }
    } else if viewer.started.elapsed().as_secs() > 90 {
        eprintln!("INSPECTOR_LOAD_TIMEOUT");
        exit.write(AppExit::error());
    }
}

fn normal_capture_complete(_event: On<ScreenshotCaptured>, mut viewer: ResMut<Viewer>) {
    viewer.live_pending = false;
    viewer.pending_since = None;
}

fn export_capture_complete(event: On<ScreenshotCaptured>, mut viewer: ResMut<Viewer>) {
    let result = (|| -> Result<()> {
        let command = viewer
            .pending_command
            .take()
            .ok_or_else(|| eyre!("capture completed without a pending command"))?;
        let image = &event.image;
        let desc = &image.texture_descriptor;
        let (width, height) = (desc.size.width, desc.size.height);
        let output = viewer.sequence_dir.clone();
        ensure!(
            (width, height) == (viewer.capture_width, viewer.capture_height),
            "offscreen physical dimensions differ from active capture settings"
        );
        let capture_fps = viewer.capture_fps;
        let seq = viewer
            .sequence
            .as_mut()
            .ok_or_else(|| eyre!("missing export sequence"))?;
        let dynamic = image
            .clone()
            .try_into_dynamic()
            .map_err(|e| eyre!("screenshot image conversion failed: {e}"))?;
        let mut png = Cursor::new(Vec::new());
        dynamic.write_to(&mut png, image::ImageFormat::Png)?;
        let index = command.frame_index;
        let path = export::write_captured_png(&output, index, png.get_ref())?;
        let seek = export::Rational {
            numerator: u64::from(index),
            denominator: u64::from(capture_fps),
        };
        let ack = export::CaptureAck {
            protocol: export::PROTOCOL.into(),
            version: 1,
            source_identity: seq.identity().source_identity.clone(),
            sequence: command.sequence,
            frame_index: index,
            state: export::AckState::Captured,
            png_path: Some(path.to_string_lossy().into_owned()),
            width,
            height,
            png_readback_and_write_completed: true,
            seek,
        };
        seq.complete_capture(&command, &ack, &path)?;
        Ok(())
    })();
    viewer.pending = false;
    viewer.pending_since = None;
    if let Err(error) = result {
        viewer.capture_error = Some(error.to_string());
        eprintln!("INSPECTOR_EXPORT_FAILED partial PNGs retained: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets");
        fs::create_dir_all(assets.join("meshes")).unwrap();
        fs::write(
            assets.join("meshes/test.glb"),
            b"parser fixture, not a decoded model",
        )
        .unwrap();
        let output = dir.path().join("captures");
        (dir, assets, output)
    }

    fn args(assets: &std::path::Path, output: &std::path::Path, tail: &[&str]) -> Vec<String> {
        [
            vec![
                "--assets".into(),
                assets.display().to_string(),
                "--output".into(),
                output.display().to_string(),
            ],
            tail.iter().map(|s| (*s).to_owned()).collect(),
        ]
        .concat()
    }

    #[test]
    fn paths_refuse_traversal_uri_labels_symlinks_and_output_in_assets() {
        let (_dir, assets, output) = fixture();
        for path in [
            "../outside.glb",
            "/absolute.glb",
            "meshes/test.glb#Scene0",
            "meshes/../test.glb",
        ] {
            assert!(
                arguments(args(&assets, &output, &["--model", path])).is_err(),
                "{path}"
            );
        }
        assert!(
            arguments(args(
                &assets,
                &assets.join("capture"),
                &["--model", "meshes/test.glb"]
            ))
            .is_err()
        );
        #[cfg(unix)]
        {
            fs::write(assets.parent().unwrap().join("outside.glb"), b"outside").unwrap();
            std::os::unix::fs::symlink(
                assets.parent().unwrap().join("outside.glb"),
                assets.join("meshes/link.glb"),
            )
            .unwrap();
            assert!(arguments(args(&assets, &output, &["--model", "meshes/link.glb"])).is_err());
        }
        assert!(!output.exists());
    }

    #[test]
    fn model_and_hex_record_lookup_share_the_same_read_only_inputs() {
        let (_dir, assets, output) = fixture();
        let connection = Connection::open(assets.join("skyrim_world.db")).unwrap();
        connection.execute_batch("CREATE TABLE statics(id INTEGER PRIMARY KEY, model_path TEXT); INSERT INTO statics VALUES(42,'Test.NIF');").unwrap();
        drop(connection);
        let by_path = arguments(args(&assets, &output, &["--model", "meshes/test.glb"])).unwrap();
        let by_record = arguments(args(&assets, &output, &["--record", "0x2A"])).unwrap();
        assert_eq!(by_path.inputs[0].model, by_record.inputs[0].model);
        assert_eq!(by_record.inputs[0].label, "base 0000002A");
        assert!(!output.exists());
    }

    #[test]
    fn unknown_or_variant_record_is_explicit_and_never_creates_output() {
        let (_dir, assets, output) = fixture();
        let connection = Connection::open(assets.join("skyrim_world.db")).unwrap();
        connection.execute_batch("CREATE TABLE statics(id INTEGER PRIMARY KEY, model_path TEXT); INSERT INTO statics VALUES(1,'Test.NIF'); CREATE TABLE base_texture_swaps(base_id INTEGER); INSERT INTO base_texture_swaps VALUES(1);").unwrap();
        drop(connection);
        assert!(arguments(args(&assets, &output, &["--record", "2"])).is_err());
        let result = arguments(args(&assets, &output, &["--record", "1"]));
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("per-record texture swaps")
        );
        assert!(!output.exists());
    }

    #[test]
    fn counts_duplicate_options_and_existing_output_refuse() {
        let (_dir, assets, output) = fixture();
        assert!(arguments(args(&assets, &output, &[])).is_err());
        let mut many = Vec::new();
        for _ in 0..9 {
            many.extend(["--model", "meshes/test.glb"]);
        }
        assert!(arguments(args(&assets, &output, &many)).is_err());
        assert!(
            arguments(args(
                &assets,
                &output,
                &["--assets", "second", "--model", "meshes/test.glb"]
            ))
            .is_err()
        );
        fs::create_dir(&output).unwrap();
        assert!(arguments(args(&assets, &output, &["--model", "meshes/test.glb"])).is_err());
    }

    #[test]
    fn export_cli_parses_bounds_and_rejects_invalid_modes() {
        let (_dir, assets, output) = fixture();
        let parsed = arguments(args(
            &assets,
            &output,
            &[
                "--model",
                "meshes/test.glb",
                "--export-frames",
                "--seconds",
                "5",
                "--fps",
                "24",
                "--size",
                "640x480",
                "--orbit",
                "--background",
                "#102030",
            ],
        ))
        .unwrap();
        assert_eq!(
            parsed.export,
            Some(ExportOptions {
                kind: ExportKind::Frames,
                seconds: 5,
                fps: 24,
                width: 640,
                height: 480,
                orbit: true,
                background: Background::Solid([16, 32, 48]),
                video_format: VideoFormat::Mp4
            })
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-gif",
                    "--export-video"
                ]
            ))
            .is_err()
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-video",
                    "--background",
                    "transparent"
                ]
            ))
            .is_err()
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-video",
                    "--size",
                    "641x480"
                ]
            ))
            .is_err()
        );
        assert_eq!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-video",
                    "--video-format",
                    "mov"
                ]
            ))
            .unwrap()
            .export
            .unwrap()
            .video_format,
            VideoFormat::Mov
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-frames",
                    "--size",
                    "640x480"
                ]
            ))
            .is_ok()
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-frames",
                    "--fps",
                    "61"
                ]
            ))
            .is_err()
        );
        assert!(
            arguments(args(
                &assets,
                &output,
                &[
                    "--model",
                    "meshes/test.glb",
                    "--export-frames",
                    "--size",
                    "2024x2024",
                    "--seconds",
                    "60",
                    "--fps",
                    "60"
                ]
            ))
            .is_err()
        );
    }

    #[test]
    fn recording_settings_match_offscreen_sequence_configuration() {
        let settings = recording_settings();
        assert_eq!(
            (
                settings.width,
                settings.height,
                settings.fps,
                settings.max_frames
            ),
            (640, 400, 30, 120)
        );
        let dir = tempfile::tempdir().unwrap();
        let (_, sequence) = create_recording_sequence(dir.path()).unwrap();
        assert_eq!(
            (sequence.identity().width, sequence.identity().height),
            (settings.width, settings.height)
        );
        assert_eq!(sequence.next_index(), Some(0));
    }

    #[test]
    fn recording_stop_waits_for_in_flight_frame_and_finishes_only_captured_prefix() {
        assert_eq!(
            recording_stop_action(0, false),
            RecordingStopAction::DiscardEmpty
        );
        assert_eq!(
            recording_stop_action(0, true),
            RecordingStopAction::WaitForReadback
        );
        assert_eq!(
            recording_stop_action(1, true),
            RecordingStopAction::WaitForReadback
        );
        assert_eq!(
            recording_stop_action(1, false),
            RecordingStopAction::FinishCapturedPrefix
        );
    }

    #[test]
    fn readback_guard_allows_only_one_live_or_offscreen_capture() {
        assert!(can_start_readback(false, false));
        assert!(!can_start_readback(true, false));
        assert!(!can_start_readback(false, true));
        assert!(!can_start_readback(true, true));
    }

    #[test]
    fn export_and_r_recording_lock_camera_and_fill_controls() {
        assert!(!fixed_pose_controls_locked(false, false));
        assert!(fixed_pose_controls_locked(true, false));
        assert!(fixed_pose_controls_locked(false, true));
    }

    #[test]
    fn recording_stop_empty_and_partial_paths_do_not_claim_missing_frames() {
        let dir = tempfile::tempdir().unwrap();
        let (empty_dir, empty) = create_recording_sequence(dir.path()).unwrap();
        assert_eq!(
            recording_stop_action(0, false),
            RecordingStopAction::DiscardEmpty
        );
        drop(empty);
        assert!(!empty_dir.join("manifest.json").exists());

        // The pure stop decision waits while readback is active. The caller finishes
        // only after sequence completion advances the contiguous captured prefix.
        assert_eq!(
            recording_stop_action(0, true),
            RecordingStopAction::WaitForReadback
        );
        assert_eq!(
            recording_stop_action(1, false),
            RecordingStopAction::FinishCapturedPrefix
        );
    }

    #[test]
    fn recording_allocates_exclusive_bounded_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let (first_dir, first) = create_recording_sequence(dir.path()).unwrap();
        let (second_dir, second) = create_recording_sequence(dir.path()).unwrap();
        assert_ne!(first_dir, second_dir);
        assert_eq!(first.next_index(), Some(0));
        assert_eq!(second.next_index(), Some(0));
        assert_eq!(first_dir.file_name().unwrap(), "recording-000");
        assert_eq!(second_dir.file_name().unwrap(), "recording-001");
        assert_eq!(RECORDING_MAX_FRAMES, 120);
    }
}
