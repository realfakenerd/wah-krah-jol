//! No-window, no-light regression for Default/Own_Emit diffuse-modulated emission (#469).
//! --output <new directory> [--legacy] renders the stock additive path as a negative control.
use bevy::{
    camera::{Exposure, Hdr, RenderTarget, ScalingMode},
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured},
    window::{ExitCondition, WindowPlugin},
};
use converter::{
    material::{
        LightingShaderType, NifAlphaMode, NifMaterialDisposition, NifShaderFamily,
        NifShapeMaterial, NifTextureSemantic, NifTextureSlot, ValidatedNifMaterial,
        publish_gltf_materials,
    },
    texture::{TextureConverter, TextureEncoding},
};
use ddsfile::{AlphaMode as DdsAlphaMode, D3D10ResourceDimension, Dds, DxgiFormat, NewDxgiParams};
use engine::nif_material::NifMaterialPlugin;
use std::{
    path::Path,
    time::{Duration, Instant},
};

const TEXELS: [[u8; 4]; 2] = [[32, 16, 8, 255], [192, 128, 64, 255]];
const TINT: [f32; 3] = [127.0 / 255.0, 104.0 / 255.0, 44.0 / 255.0];

fn fixture(output: &Path) {
    std::fs::create_dir_all(output.join("textures")).unwrap();
    let mut dds = Dds::new_dxgi(NewDxgiParams {
        height: 4,
        width: 4,
        depth: None,
        format: DxgiFormat::R8G8B8A8_UNorm,
        mipmap_levels: None,
        array_layers: None,
        caps2: None,
        is_cubemap: false,
        resource_dimension: D3D10ResourceDimension::Texture2D,
        alpha_mode: DdsAlphaMode::Straight,
    })
    .unwrap();
    dds.data = (0..16).flat_map(|pixel| TEXELS[(pixel % 4) / 2]).collect();
    let mut bytes = Vec::new();
    dds.write(&mut bytes).unwrap();
    std::fs::write(
        output.join("textures/surface.opensky-srgb.opensky-wrap0.ktx2"),
        TextureConverter::convert_uncompressed(&bytes, TextureEncoding::ColorSrgb).unwrap(),
    )
    .unwrap();
    let material = ValidatedNifMaterial {
        shader_family: NifShaderFamily::Lighting,
        lighting_shader_type: Some(LightingShaderType::Default),
        shader_block: 0,
        texture_set_block: None,
        alpha_property_block: None,
        shader_flags_1: (1 << 31) | (1 << 22),
        shader_flags_2: 1,
        base_color: [1.0; 4],
        alpha: 1.0,
        alpha_mode: NifAlphaMode::Opaque,
        alpha_threshold: None,
        glossiness: 0.0,
        specular_color: [0.0; 3],
        specular_strength: 0.0,
        emissive_color: TINT,
        emissive_multiple: 1.0,
        double_sided: false,
        textures: vec![NifTextureSlot {
            slot: 0,
            semantic: NifTextureSemantic::Diffuse,
            path: "textures/surface.dds".into(),
            required: true,
        }],
        uv_offset: [0.0; 2],
        uv_scale: [1.0; 2],
        texture_clamp_mode: 0,
        tint_color: None,
    };
    let mut document =
        serde_json::json!({"asset":{"version":"2.0"},"meshes":[{"primitives":[{}]}]});
    publish_gltf_materials(
        &mut document,
        &[NifShapeMaterial {
            shape_block: 0,
            shape_name: Some("own_emit_surface".into()),
            shader_property_block: Some(0),
            alpha_property_block: None,
            disposition: NifMaterialDisposition::Validated { material },
        }],
        &[0],
        Path::new("surface.glb"),
    )
    .unwrap();
    document.as_object_mut().unwrap().remove("meshes");
    std::fs::write(
        output.join("surface.gltf"),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
}

#[derive(Resource)]
struct Probe {
    output: std::path::PathBuf,
    legacy: bool,
    material: Handle<StandardMaterial>,
    target: Handle<Image>,
    started: Instant,
    frames: u32,
    spawned: bool,
    captured: bool,
}

fn main() {
    let mut output = None;
    let mut legacy = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => {
                output = Some(std::path::PathBuf::from(
                    args.next().expect("output directory"),
                ))
            }
            "--legacy" => legacy = true,
            _ => panic!("unknown argument {arg}"),
        }
    }
    let output = output.expect("--output <new directory>");
    assert!(!output.exists(), "preserve previous probe evidence");
    fixture(&output);
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: output.to_string_lossy().into_owned(),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .disable::<bevy::winit::WinitPlugin>()
            .disable::<bevy::audio::AudioPlugin>(),
    )
    .add_plugins(NifMaterialPlugin)
    .add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
        Duration::from_millis(16),
    ))
    .insert_resource(GlobalAmbientLight::NONE);
    let material = app.world().resource::<AssetServer>().load(if legacy {
        "surface.gltf#Material0/std"
    } else {
        "surface.gltf#Material0/nif"
    });
    let target = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new_target_texture(
            800,
            200,
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            None,
        ));
    let result = app
        .insert_resource(Probe {
            output,
            legacy,
            material,
            target,
            started: Instant::now(),
            frames: 0,
            spawned: false,
            captured: false,
        })
        .add_systems(Update, render)
        .run();
    if !result.is_success() {
        std::process::exit(1);
    }
}

fn render(
    mut commands: Commands,
    mut probe: ResMut<Probe>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    images: Res<Assets<Image>>,
    mut exit: MessageWriter<AppExit>,
) {
    if probe.started.elapsed() > Duration::from_secs(90) {
        eprintln!("own-emittance probe timed out");
        exit.write(AppExit::error());
        return;
    }
    if !probe.spawned {
        let Some(material) = materials.get(&probe.material) else {
            return;
        };
        let Some(diffuse) = material.base_color_texture.as_ref() else {
            panic!("missing diffuse texture")
        };
        if images.get(diffuse).is_none() {
            return;
        }
        assert_eq!(
            material.emissive_texture.as_ref(),
            (!probe.legacy).then_some(diffuse)
        );
        let quad = meshes.add(Rectangle::new(3.0, 1.5));
        commands.spawn((
            Mesh3d(quad),
            MeshMaterial3d(probe.material.clone()),
            Transform::from_xyz(-2.0, 0.0, 0.0),
        ));
        let half = meshes.add(Rectangle::new(1.5, 1.5));
        for (index, texel) in TEXELS.iter().enumerate() {
            let linear = Color::srgb_u8(texel[0], texel[1], texel[2]).to_linear();
            let reference = materials.add(StandardMaterial {
                base_color: Color::BLACK,
                emissive: LinearRgba::rgb(
                    TINT[0] * linear.red,
                    TINT[1] * linear.green,
                    TINT[2] * linear.blue,
                ),
                ..default()
            });
            commands.spawn((
                Mesh3d(half.clone()),
                MeshMaterial3d(reference),
                Transform::from_xyz(1.25 + index as f32 * 1.5, 0.0, 0.0),
            ));
        }
        commands.spawn((
            Camera3d::default(),
            Hdr,
            Exposure { ev100: 9.7 },
            Tonemapping::TonyMcMapface,
            DebandDither::Disabled,
            Msaa::Off,
            RenderTarget::Image(probe.target.clone().into()),
            Camera {
                clear_color: ClearColorConfig::Custom(Color::BLACK),
                ..default()
            },
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::FixedVertical {
                    viewport_height: 2.0,
                },
                ..OrthographicProjection::default_3d()
            }),
            Transform::from_xyz(0.0, 0.0, 10.0),
        ));
        probe.spawned = true;
    }
    probe.frames += 1;
    if probe.frames >= 60 && !probe.captured {
        probe.captured = true;
        commands
            .spawn(Screenshot::image(probe.target.clone()))
            .observe(evaluate);
    }
}

fn evaluate(
    event: On<ScreenshotCaptured>,
    probe: Res<Probe>,
    adapter: Res<bevy::render::renderer::RenderAdapterInfo>,
    mut exit: MessageWriter<AppExit>,
) {
    let image = event.image.clone().try_into_dynamic().unwrap().to_rgb8();
    image.save(probe.output.join("probe.png")).unwrap();
    let mut passed = true;
    let mut samples = Vec::new();
    for (index, x) in [125, 275].into_iter().enumerate() {
        let actual = image.get_pixel(x, 100).0;
        let expected = image.get_pixel(x + 400, 100).0;
        let error = actual
            .into_iter()
            .zip(expected)
            .map(|(a, b)| a.abs_diff(b))
            .max()
            .unwrap();
        passed &= error <= 2;
        samples.push(serde_json::json!({"texel_srgb":TEXELS[index],"actual":actual,"reference":expected,"max_error_u8":error}));
    }
    let dark = image.get_pixel(525, 100).0;
    let light = image.get_pixel(675, 100).0;
    passed &= light[0] > dark[0] + 5;
    let report = serde_json::json!({"kind":"native-own-emit-diffuse-modulation","legacy":probe.legacy,
        "retail_parity":false,"lights":0,"ambient_brightness":0,"ev100":9.7,
        "tint":TINT,"multiple":1.0,"samples":samples,"adapter":adapter.name,"passed":passed});
    std::fs::write(
        probe.output.join("probe.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{report}");
    exit.write(if passed {
        AppExit::Success
    } else {
        AppExit::error()
    });
}
