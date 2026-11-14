//! Interactive inspection through the existing glTF/NIF material loader; no world startup.
use bevy::{
    camera::{Exposure, primitives::Aabb},
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    gltf::GltfAssetLabel,
    prelude::*,
    render::{
        RenderPlugin,
        settings::{Backends, RenderCreation, WgpuFeatures, WgpuSettings},
        view::screenshot::{Screenshot, save_to_disk},
    },
    world_serialization::{WorldAsset, WorldAssetRoot},
};
use color_eyre::eyre::{Result, ensure, eyre};
use engine::nif_material::{EffectPaletteMaterialPlugin, NifMaterialPlugin};
use rusqlite::{Connection, OpenFlags};
use std::{path::PathBuf, time::Instant};
struct Input {
    model: String,
    label: String,
}
struct Options {
    assets: PathBuf,
    output: PathBuf,
    inputs: Vec<Input>,
    asset_lock: shared::asset_lock::AssetLock,
}
fn arguments(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut args = args.into_iter();
    let mut assets = None;
    let mut output = None;
    let mut catalog = None;
    let mut models = Vec::new();
    let mut records = Vec::new();
    while let Some(flag) = args.next() {
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
}
fn main() -> Result<()> {
    let options = arguments(std::env::args().skip(1))?;
    let _pack_lock = options.asset_lock;
    std::fs::create_dir(&options.output)?;
    let mut app = App::new();
    app.add_plugins(DefaultPlugins
  .set(AssetPlugin{file_path:options.assets.to_string_lossy().into_owned(),..default()})
  .set(WindowPlugin{primary_window:Some(Window{title:"Mudcrab model inspector | arrows orbit/zoom, W/S elevation, P pause, F12 capture, Esc close".into(),resolution:(1280,800).into(),..default()}),..default()})
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
    app.insert_resource(Viewer {
        scenes,
        output: options.output,
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
    })
    .add_systems(Startup, setup)
    .add_systems(Update, update);
    ensure!(app.run().is_success(), "inspector failed; see native log");
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
    commands.spawn((
        DirectionalLight {
            illuminance: 5000.,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(100., 200., 150.).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((Text::new(format!("{}\nArrows: orbit/zoom  W/S: elevation  Space: fill  P: pause  F12: capture  Esc: close",viewer.labels)),TextFont{font_size:FontSize::Px(18.),..default()},TextColor(Color::WHITE),Node{position_type:PositionType::Absolute,left:px(16),top:px(16),..default()}));
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
    mut exit: MessageWriter<AppExit>,
) {
    if keys.just_pressed(KeyCode::Escape) || viewer.started.elapsed().as_secs() >= 1800 {
        exit.write(AppExit::Success);
        return;
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
    if keys.pressed(KeyCode::ArrowLeft) {
        viewer.yaw -= dt;
    }
    if keys.pressed(KeyCode::ArrowRight) {
        viewer.yaw += dt;
    }
    if keys.pressed(KeyCode::KeyW) {
        viewer.pitch = (viewer.pitch + dt).min(1.5);
    }
    if keys.pressed(KeyCode::KeyS) {
        viewer.pitch = (viewer.pitch - dt).max(-1.5);
    }
    if keys.pressed(KeyCode::ArrowUp) {
        viewer.radius *= (-dt).exp();
    }
    if keys.pressed(KeyCode::ArrowDown) {
        viewer.radius *= dt.exp();
    }
    viewer.radius = viewer.radius.clamp(0.01, 100000.);
    if keys.just_pressed(KeyCode::Space) {
        ambient.brightness = if ambient.brightness == 0. { 40. } else { 0. };
    }
    if keys.just_pressed(KeyCode::KeyP) {
        if virtual_time.is_paused() {
            virtual_time.unpause();
        } else {
            virtual_time.pause();
        }
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
        if viewer.frames == 180 || viewer.frames == 480 || keys.just_pressed(KeyCode::F12) {
            let path = viewer
                .output
                .join(format!("model-{:03}.png", viewer.captures));
            viewer.captures += 1;
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path));
        }
    } else if viewer.started.elapsed().as_secs() > 90 {
        eprintln!("INSPECTOR_LOAD_TIMEOUT");
        exit.write(AppExit::error());
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
}
