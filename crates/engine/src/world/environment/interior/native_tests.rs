//! Asset-free Metal oracle. Build and execution each require separate department admission.
//! This checks the current Bevy adapter, not Skyrim's lighting equations or visual parity.

use super::*;
use crate::{capture_frame::CaptureTicket, hdr_probe::ProbePoint};
use bevy::{
    app::PluginsState,
    camera::{Exposure, Hdr, RenderTarget},
    core_pipeline::tonemapping::Tonemapping,
    diagnostic::FrameCount,
    render::{
        Extract, ExtractSchedule, RenderApp, RenderPlugin,
        pipelined_rendering::PipelinedRenderingPlugin,
        render_resource::{PipelineCache, TextureFormat},
        renderer::RenderAdapterInfo,
        settings::{Backends, RenderCreation, WgpuSettings},
        view::screenshot::{Screenshot, ScreenshotCaptured},
    },
    window::ExitCondition,
    winit::WinitPlugin,
};
use converter::esm::{exporter, records::RawRecord};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const TEST: &str = "interior_transition_hdr_readback";

fn save(path: &Path, value: &Value) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
}

fn record(id: u32, tag: &[u8; 4], fields: Vec<(&[u8; 4], Vec<u8>)>) -> RawRecord {
    RawRecord {
        form_id: id,
        record_type: *tag,
        flags: 0,
        subrecords: fields
            .into_iter()
            .map(|(tag, data)| (tag.to_vec(), data))
            .collect(),
        cell_form_id: None,
        worldspace_form_id: None,
        load_order: 0,
    }
}

fn block(ambient: [u8; 3], directional: [u8; 3], fade: f32, mask: u32) -> Vec<u8> {
    let mut bytes = vec![0; 92];
    bytes[..3].copy_from_slice(&ambient);
    bytes[4..7].copy_from_slice(&directional);
    bytes[20..24].copy_from_slice(&180u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&fade.to_le_bytes());
    for face in bytes[40..64].as_chunks_mut::<4>().0 {
        face[..3].copy_from_slice(&ambient);
    }
    bytes[88..92].copy_from_slice(&mask.to_le_bytes());
    bytes
}

fn database(path: &Path, template_ambient: [u8; 3]) -> EnvironmentCatalog {
    let black = [0; 3];
    let red = [255, 0, 0];
    let green = [0, 255, 0];
    let mut dalc = vec![0; 32];
    for face in dalc[..24].as_chunks_mut::<4>().0 {
        face[..3].copy_from_slice(&template_ambient);
    }
    let mut records = HashMap::from([(
        20,
        record(
            20,
            b"LGTM",
            vec![(b"DATA", block(red, black, 0., 0)), (b"DALC", dalc)],
        ),
    )]);
    for (id, flags, data, template) in [
        (1, 1u16, Some(block(black, black, 0., 0)), false),
        (2, 1, Some(block(red, black, 0., 0)), false),
        (3, 1, Some(block(black, green, 1., 0)), false),
        (4, 1, Some(block(red, black, 0., 1)), true),
        (5, 1, Some(block(red, black, 0., 0)), true),
        (6, 129, Some(block(red, black, 0., 0)), false),
        (7, 257, Some(block(red, black, 0., 0)), false),
        (8, 385, Some(block(red, black, 0., 0)), false),
        (9, 1, None, true),
        (10, 1, Some(Vec::new()), true),
    ] {
        let mut fields: Vec<(&[u8; 4], Vec<u8>)> = vec![(b"DATA", flags.to_le_bytes().to_vec())];
        if let Some(data) = data {
            fields.push((b"XCLL", data));
        }
        if template {
            fields.push((b"LTMP", 20u32.to_le_bytes().to_vec()));
        }
        records.insert(id, record(id, b"CELL", fields));
    }
    let connection = Connection::open(path).unwrap();
    exporter::create_tables(&connection).unwrap();
    exporter::export_to_db(&connection, &records).unwrap();
    drop(connection);
    EnvironmentCatalog::open(path).unwrap()
}

/// Same deferred PostUpdate boundary as door crossing, without the unrelated streaming world.
#[derive(Resource, Default)]
struct Transition(Option<Option<u32>>);

fn queue_transition(mut commands: Commands, mut transition: ResMut<Transition>) {
    if let Some(interior) = transition.0.take() {
        commands.queue(move |world: &mut World| {
            world.resource_mut::<ActiveSpace>().interior = interior;
        });
    }
}

#[derive(Resource)]
struct DatabaseIdentity(String);

#[derive(Component, Clone)]
struct Extraction(Arc<Mutex<Option<Value>>>);

/// Sample the selected scene and actual light in the same extraction as the HDR request.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn extract_state(
    tickets: Extract<Query<&Extraction, With<Screenshot>>>,
    frame: Extract<Res<FrameCount>>,
    space: Extract<Res<ActiveSpace>>,
    scene: Extract<Res<SceneEnvironment>>,
    light: Extract<Query<(&DirectionalLight, &Transform), With<WorldEnvironmentLight>>>,
    ambient: Extract<Res<GlobalAmbientLight>>,
    settings: Extract<Res<RenderingSettings>>,
    database: Extract<Res<DatabaseIdentity>>,
    camera: Extract<
        Query<(&Exposure, &Tonemapping, &Msaa), With<crate::world::components::StreamingCamera>>,
    >,
) {
    let (light, transform) = light.single().unwrap();
    let (exposure, tonemapping, msaa) = camera.single().unwrap();
    assert_eq!(exposure.ev100, 8.);
    assert_eq!(*msaa, Msaa::Off);
    for ticket in &tickets {
        let mut state = ticket.0.lock().unwrap();
        if state.is_none() {
            *state = Some(json!({
                "database_sha256": database.0, "settings": &**settings,
                "camera": {"exposure_ev100": exposure.ev100, "tonemapping": format!("{tonemapping:?}"), "msaa": format!("{msaa:?}")},
                "main_frame": frame.0, "active_space": {"interior": space.interior, "worldspace_id": space.worldspace_id},
                "scene_environment": &**scene,
                "directional": {"rgb_linear": light.color.to_linear().to_f32_array(), "illuminance": light.illuminance,
                    "rotation_xyzw": transform.rotation.to_array(), "shadows": light.shadow_maps_enabled},
                "ambient": {"rgb_linear": ambient.color.to_linear().to_f32_array(), "brightness": ambient.brightness},
            }));
        }
    }
}

fn native_app(catalog: EnvironmentCatalog) -> (App, Handle<Image>) {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .set(RenderPlugin {
                render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                    backends: Some(Backends::METAL),
                    ..default()
                })),
                synchronous_pipeline_compilation: true,
                ..default()
            })
            .disable::<WinitPlugin>()
            .disable::<bevy::audio::AudioPlugin>()
            .disable::<PipelinedRenderingPlugin>(),
    )
    .insert_resource(catalog)
    .insert_resource(crate::visual_profile::VisualProfile::Mudcrab)
    .init_resource::<ActiveSpace>()
    .init_resource::<Transition>()
    .init_resource::<crate::config::EngineConfig>()
    .insert_resource(RenderingSettings {
        sun_color: [1., 1., 1.],
        ambient_color: [1., 1., 1.],
        sun_illuminance: 1_000.,
        ambient_brightness: 100.,
        shadows: false,
        exposure_ev100: 8.,
        msaa: 0,
        depth_prepass: false,
        occlusion_culling: false,
        ..default()
    })
    .insert_resource(ClearColor(Color::BLACK))
    .insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 100.,
        ..default()
    })
    .add_plugins((InteriorEnvironmentPlugin, crate::hdr_probe::HdrProbePlugin))
    .add_systems(Update, crate::settings::apply_render_settings)
    .add_systems(
        PostUpdate,
        queue_transition.in_set(crate::door_crossing::DoorCrossingSet),
    );
    app.sub_app_mut(RenderApp)
        .add_systems(ExtractSchedule, extract_state);
    let deadline = Instant::now() + Duration::from_secs(15);
    while app.plugins_state() != PluginsState::Ready {
        assert!(Instant::now() < deadline, "renderer initialization timeout");
        bevy::tasks::tick_global_task_pools_on_main_thread();
        std::thread::sleep(Duration::from_millis(10));
    }
    app.finish();
    app.cleanup();
    assert_eq!(
        app.world().resource::<RenderAdapterInfo>().backend,
        wgpu::Backend::Metal
    );
    let target = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new_target_texture(
            WIDTH,
            HEIGHT,
            TextureFormat::Rgba8UnormSrgb,
            None,
        ));
    app.world_mut().spawn((
        Camera3d::default(),
        Hdr,
        crate::world::components::StreamingCamera,
        RenderTarget::Image(target.clone().into()),
        Transform::from_xyz(0., 0., 4.),
        Msaa::Off,
        Tonemapping::None,
        Exposure { ev100: 8. },
    ));
    let mesh = app
        .world_mut()
        .resource_mut::<Assets<Mesh>>()
        .add(Rectangle::new(4., 2.));
    let material = app
        .world_mut()
        .resource_mut::<Assets<StandardMaterial>>()
        .add(StandardMaterial {
            base_color: Color::srgb(0.5, 0.5, 0.5),
            perceptual_roughness: 1.,
            metallic: 0.,
            reflectance: 0.,
            fog_enabled: false,
            ..default()
        });
    app.world_mut()
        .spawn((Mesh3d(mesh), MeshMaterial3d(material), Transform::default()));
    app.world_mut().spawn((
        WorldEnvironmentLight,
        DirectionalLight {
            illuminance: 1_000.,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::default(),
    ));
    (app, target)
}

fn capture(
    app: &mut App,
    target: &Handle<Image>,
    run: &Path,
    name: &str,
    deadline: Instant,
) -> [f32; 3] {
    let expected_cell = app
        .world()
        .resource::<Transition>()
        .0
        .unwrap_or(app.world().resource::<ActiveSpace>().interior);
    // Fixed settling count. A late pipeline is a failure, never a reason to select a later frame.
    for _ in 0..8 {
        assert!(Instant::now() < deadline);
        app.update();
    }
    assert!(
        app.sub_app(RenderApp)
            .world()
            .resource::<PipelineCache>()
            .waiting_pipelines()
            .next()
            .is_none(),
        "pipelines not ready at capture boundary"
    );
    let ticket = CaptureTicket::default().with_probes(vec![ProbePoint {
        name: "neutral-receiver".into(),
        position: [0.; 3],
        patch_px: 9,
    }]);
    let extraction = Extraction(Arc::default());
    let (sender, receiver) = mpsc::channel();
    app.world_mut()
        .spawn((
            Screenshot::image(target.clone()),
            ticket.clone(),
            extraction.clone(),
        ))
        .observe(move |event: On<ScreenshotCaptured>| {
            sender.send(event.image.clone()).unwrap();
        });
    let mut image = None;
    let mut readback = None;
    for _ in 0..16 {
        assert!(Instant::now() < deadline, "total fixture timeout");
        app.update();
        if let Ok(value) = receiver.try_recv() {
            image = Some(value);
        }
        if let Some(value) = ticket.probes().unwrap().finished() {
            readback = Some(value.unwrap());
        }
        if image.is_some() && readback.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let image = image
        .expect("screenshot readback frame limit")
        .try_into_dynamic()
        .unwrap();
    assert_eq!((image.width(), image.height()), (WIDTH, HEIGHT));
    image.save(run.join(format!("{name}.png"))).unwrap();
    let readback = readback.expect("HDR readback frame limit");
    let state = extraction
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("same-extraction scene state");
    assert_eq!(state["active_space"]["interior"], json!(expected_cell));
    assert_eq!(
        state["scene_environment"],
        serde_json::to_value(app.world().resource::<SceneEnvironment>()).unwrap()
    );
    assert_eq!(readback.viewport, [WIDTH, HEIGHT]);
    let rgba = readback.samples[0]
        .hdr_pre_tonemap
        .expect("finite on-screen HDR sample");
    assert!(rgba.iter().all(|v| v.is_finite()));
    save(
        &run.join(format!("{name}.json")),
        &json!({
            "extraction": state,
            "hdr": readback, "pipeline_waiting": 0, "profile": "mudcrab", "exposure_ev100": 8.,
            "receiver": {"base_color_srgb": [0.5, 0.5, 0.5], "roughness": 1., "metallic": 0., "reflectance": 0.},
        }),
    );
    [rgba[0], rgba[1], rgba[2]]
}

fn close(a: [f32; 3], b: [f32; 3]) {
    for (a, b) in a.into_iter().zip(b) {
        assert!(
            (a - b).abs() <= 0.0001 + 0.02 * a.abs().max(b.abs()),
            "HDR mismatch: {a} vs {b}"
        );
    }
}

fn dominant(value: [f32; 3], channel: usize) {
    assert!(value[channel] > 0.001, "unlit receiver: {value:?}");
    for other in 0..3 {
        if other != channel {
            assert!(
                value[channel] > value[other] * 4. + 0.0001,
                "channel response: {value:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires exact-source build and separate bounded Metal admission via interior_oracle.py"]
fn interior_transition_hdr_readback() {
    assert_eq!(
        std::env::var("MUDCRAB_INTERIOR_NATIVE_ADMITTED").as_deref(),
        Ok("v1")
    );
    let run = std::path::PathBuf::from(
        std::env::var_os("MUDCRAB_INTERIOR_RUN").expect("watchdog output"),
    );
    let database_path = run.join("environment.db");
    let catalog = database(&database_path, [0, 0, 255]);
    let database_hash = format!("{:x}", Sha256::digest(fs::read(&database_path).unwrap()));
    let deadline = Instant::now() + Duration::from_secs(100);
    let (mut app, target) = native_app(catalog);
    app.insert_resource(DatabaseIdentity(database_hash.clone()));
    for _ in 0..24 {
        assert!(Instant::now() < deadline);
        app.update();
    }
    let baseline = capture(&mut app, &target, &run, "exterior-before", deadline);
    assert!(
        baseline.iter().all(|v| *v > 0.001),
        "baseline must light the receiver"
    );
    let mut values = Vec::new();
    for cell in 1..=10 {
        app.world_mut().resource_mut::<Transition>().0 = Some(Some(cell));
        app.update();
        assert_eq!(
            app.world()
                .resource::<SceneEnvironment>()
                .interior
                .as_ref()
                .unwrap()
                .cell_id,
            cell,
            "same-frame deferred selection"
        );
        let scene = app.world().resource::<SceneEnvironment>();
        assert_eq!(scene.show_sky, [6, 8].contains(&cell));
        assert_eq!(scene.use_sky_lighting, [7, 8].contains(&cell));
        let expected_ambient = match cell {
            2 | 5 | 6 => [255, 0, 0],
            4 | 9 => [0, 0, 255],
            7 | 8 => [255; 3],
            _ => [0; 3],
        };
        assert_eq!(
            app.world()
                .resource::<GlobalAmbientLight>()
                .color
                .to_linear(),
            rgb(expected_ambient).to_linear(),
            "ambient must change in the transition frame"
        );
        let mut sun_query = app
            .world_mut()
            .query_filtered::<&DirectionalLight, With<WorldEnvironmentLight>>();
        let sun = sun_query.single(app.world()).unwrap();
        let expected_direct = match cell {
            3 => [0, 255, 0],
            7 | 8 => [255; 3],
            _ => [0; 3],
        };
        assert_eq!(
            sun.color.to_linear(),
            rgb(expected_direct).to_linear(),
            "directional RGB must change in the transition frame"
        );
        assert_eq!(
            sun.illuminance,
            if [3, 7, 8, 10].contains(&cell) {
                1_000.
            } else {
                0.
            }
        );
        let a = capture(&mut app, &target, &run, &format!("cell-{cell}-a"), deadline);
        let b = capture(&mut app, &target, &run, &format!("cell-{cell}-b"), deadline);
        close(a, b);
        values.push(a);
    }
    close(values[0], [0.; 3]);
    dominant(values[1], 0); // CELL ambient only.
    dominant(values[2], 1); // CELL directional only.
    dominant(values[3], 2); // Separate inherited LGTM DALC, not embedded red DATA.
    close(values[4], values[1]); // Mask zero preserves the local ambient cube.
    close(values[5], values[1]); // Show Sky does not select exterior lighting.
    close(values[6], baseline); // Use Sky Lighting does, even with Show Sky clear.
    close(values[7], baseline);
    close(values[8], values[3]); // Absent XCLL inherits template.
    close(values[9], values[0]); // Empty malformed XCLL does not.
    app.world_mut().resource_mut::<Transition>().0 = Some(Some(3));
    app.world_mut().resource_mut::<RenderingSettings>().sun = false;
    close(
        capture(&mut app, &target, &run, "settings-sun-off", deadline),
        [0.; 3],
    );
    app.world_mut().resource_mut::<RenderingSettings>().sun = true;
    close(
        capture(&mut app, &target, &run, "settings-sun-restored", deadline),
        values[2],
    );
    app.world_mut().resource_mut::<Transition>().0 = Some(None);
    app.update();
    assert!(
        app.world()
            .resource::<SceneEnvironment>()
            .interior
            .is_none()
    );
    close(
        capture(&mut app, &target, &run, "exterior-after", deadline),
        baseline,
    );
    let successor_path = run.join("environment-successor.db");
    let successor = database(&successor_path, [0, 255, 0]);
    let successor_hash = format!("{:x}", Sha256::digest(fs::read(&successor_path).unwrap()));
    app.insert_resource(successor);
    app.insert_resource(DatabaseIdentity(successor_hash.clone()));
    app.world_mut().resource_mut::<Transition>().0 = Some(Some(4));
    let changed = capture(&mut app, &target, &run, "template-input-changed", deadline);
    dominant(changed, 1);
    app.world_mut().resource_mut::<Transition>().0 = Some(None);
    close(
        capture(
            &mut app,
            &target,
            &run,
            "successor-exterior-restored",
            deadline,
        ),
        baseline,
    );
    save(
        &run.join("oracle-result.json"),
        &json!({
            "passed": true, "test": TEST, "database_sha256": database_hash, "cells": 10,
            "captures": 26, "successor_database_sha256": successor_hash, "backend": "Metal", "window": false,
            "limits": ["synthetic receiver only", "no full door streaming/landing", "no sky dome pixels",
                "no native Skyrim equations, angles, fade or visual parity", "no point lights, weather, fog or image space"],
        }),
    );
}
