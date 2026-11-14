//! Interactive rendering preferences. Automated runs keep their explicit configuration.

use crate::{config::EngineConfig, world::components::StreamingCamera};
use bevy::{
    camera::{Exposure, visibility::NoCpuCulling},
    core_pipeline::{prepass::DepthPrepass, tonemapping::Tonemapping},
    light::{
        CascadeShadowConfig, CascadeShadowConfigBuilder, DirectionalLightShadowMap,
        PointLightShadowMap,
    },
    prelude::*,
    render::{
        occlusion_culling::OcclusionCulling, render_asset::RenderAssetBytesPerFrame,
        renderer::RenderAdapter, view::NoIndirectDrawing,
    },
    window::{MonitorSelection, PresentMode, PrimaryWindow, WindowMode},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const TONE_MAPS: [Tonemapping; 9] = [
    Tonemapping::None,
    Tonemapping::Reinhard,
    Tonemapping::ReinhardLuminance,
    Tonemapping::AcesFitted,
    Tonemapping::AgX,
    Tonemapping::SomewhatBoringDisplayTransform,
    Tonemapping::TonyMcMapface,
    Tonemapping::BlenderFilmic,
    Tonemapping::KhronosPbrNeutral,
];
pub const MSAA_MODES: [Msaa; 4] = [Msaa::Off, Msaa::Sample2, Msaa::Sample4, Msaa::Sample8];
pub const SHADOW_SIZES: [usize; 4] = [512, 1024, 2048, 4096];
pub const WINDOW_SIZES: [(u32, u32); 4] = [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440)];

#[derive(Resource, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingSettings {
    pub version: u32,
    pub point_lights: bool,
    pub point_light_budget: usize,
    pub point_light_shadows: bool,
    pub point_shadow_size: usize,
    pub sun: bool,
    pub sun_illuminance: f32,
    pub sun_color: [f32; 3],
    pub ambient_brightness: f32,
    pub ambient_color: [f32; 3],
    pub shadows: bool,
    pub shadow_size: usize,
    pub shadow_cascades: usize,
    pub shadow_distance_scale: f32,
    pub sky: bool,
    pub sky_brightness_scale: f32,
    pub fog: bool,
    pub fog_density_scale: f32,
    pub water_reflections: bool,
    pub water_reflection_strength: f32,
    pub water_waves: bool,
    pub water_flow_normals: bool,
    pub wave_scale: f32,
    pub wave_speed: f32,
    pub wave_strength: f32,
    pub cpu_frustum_culling: bool,
    pub indirect_drawing: bool,
    pub occlusion_culling: bool,
    pub depth_prepass: bool,
    pub msaa: usize,
    pub exposure_ev100: f32,
    pub tone_map: usize,
    /// Persisted grade mode; existing indices 0 through 4 remain stable.
    pub color_grade: usize,
    pub visual_preset: usize,
    pub grade_saturation: f32,
    pub grade_brightness: f32,
    pub grade_contrast: f32,
    pub grade_tint_amount: f32,
    pub grade_tint_red: f32,
    pub grade_tint_green: f32,
    pub grade_tint_blue: f32,
    pub grade_pivot: f32,
    /// Bit mask of explicitly edited grade fields; zero leaves source values intact.
    pub grade_override_mask: u16,
    pub fov_degrees: f32,
    pub vsync: bool,
    pub window_mode: usize,
    pub window_size: usize,
    pub hud: bool,
    pub stream_radius: i32,
    pub unload_radius: i32,
    pub lod_level0_distance: f32,
    pub lod_level1_distance: f32,
    pub lod_maximum_distance: f32,
    pub lod_multiplier: f32,
    pub cell_commits: usize,
    pub commit_ms: f64,
    pub cell_unloads: usize,
    pub model_spawns: usize,
    pub upload_mib: usize,
    pub io_threads: usize,
    pub diagnostic_asset_fallbacks: bool,
    /// [`crate::visual_profile::VisualProfile`] index; read only at startup.
    pub visual_profile: usize,
}

impl Default for RenderingSettings {
    fn default() -> Self {
        let config = EngineConfig::default();
        let weather = crate::sky::WeatherLighting::default();
        let sunlight = weather.sunlight.to_srgba();
        let ambient = weather.mean_ambient().to_srgba();
        Self {
            version: 1,
            point_lights: false,
            point_light_budget: crate::lights::ENABLED_LIGHT_BUDGET,
            point_light_shadows: false,
            point_shadow_size: 1,
            sun: true,
            sun_illuminance: crate::lights::SUN_ILLUMINANCE,
            sun_color: [sunlight.red, sunlight.green, sunlight.blue],
            ambient_brightness: crate::lights::AMBIENT_ILLUMINANCE,
            ambient_color: [ambient.red, ambient.green, ambient.blue],
            shadows: true,
            shadow_size: 2,
            shadow_cascades: 4,
            shadow_distance_scale: 1.0,
            sky: true,
            sky_brightness_scale: 1.0,
            fog: true,
            fog_density_scale: 1.0,
            water_reflections: true,
            water_reflection_strength: 1.0,
            water_waves: true,
            water_flow_normals: true,
            wave_scale: 0.006,
            wave_speed: 0.15,
            wave_strength: crate::render::WAVE_STRENGTH,
            cpu_frustum_culling: true,
            indirect_drawing: true,
            occlusion_culling: true,
            depth_prepass: true,
            msaa: 0,
            exposure_ev100: crate::color_pipeline::DEFAULT_SCENE_EV100,
            tone_map: 6,
            color_grade: 3,
            visual_preset: 1,
            grade_saturation: 1.66,
            grade_brightness: 1.38,
            grade_contrast: 1.50,
            grade_tint_amount: 0.56,
            grade_tint_red: 0.88,
            grade_tint_green: 0.81,
            grade_tint_blue: 0.72,
            grade_pivot: 0.18,
            grade_override_mask: 0,
            fov_degrees: PerspectiveProjection::default().fov.to_degrees(),
            vsync: true,
            window_mode: 0,
            window_size: 1,
            hud: true,
            stream_radius: config.stream_radius,
            unload_radius: config.unload_radius,
            lod_level0_distance: config.terrain_lod.block_level0_distance,
            lod_level1_distance: config.terrain_lod.block_level1_distance,
            lod_maximum_distance: config.terrain_lod.block_maximum_distance,
            lod_multiplier: config.terrain_lod.split_distance_mult,
            cell_commits: config.max_cell_commits_per_frame,
            commit_ms: config.max_commit_micros_per_frame as f64 / 1000.0,
            cell_unloads: config.max_cell_unloads_per_frame,
            model_spawns: config.max_model_spawns_per_frame,
            upload_mib: config.max_upload_mib_per_frame,
            io_threads: config.io_threads,
            diagnostic_asset_fallbacks: config.diagnostic_asset_fallbacks,
            visual_profile: config.visual_profile.index(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Graphics,
    Streaming,
    Controls,
}

#[derive(Clone, Copy)]
pub enum SettingKind {
    Toggle,
    Number { min: f64, max: f64, step: f64 },
    Choice(&'static [&'static str]),
}

pub struct SettingSpec {
    pub label: &'static str,
    pub page: SettingsPage,
    /// Heading the menu lists this setting under; entries of a group stay contiguous.
    pub group: &'static str,
    pub kind: SettingKind,
    /// When present and false, the setting has no visible effect and the menu dims it.
    pub requires: Option<fn(&RenderingSettings) -> bool>,
    pub read: fn(&RenderingSettings) -> f64,
    pub write: fn(&mut RenderingSettings, f64),
}

macro_rules! toggle {
    ($group:literal, $label:literal, $field:ident) => {
        SettingSpec {
            label: $label,
            page: SettingsPage::Graphics,
            group: $group,
            kind: SettingKind::Toggle,
            requires: None,
            read: |s| u8::from(s.$field) as f64,
            write: |s, value| s.$field = value != 0.0,
        }
    };
}
macro_rules! number {
    ($page:ident, $group:literal, $label:literal, $field:ident $( [$index:literal] )?; $min:expr, $max:expr, $step:expr) => { SettingSpec {
        label: $label, page: SettingsPage::$page, group: $group,
        kind: SettingKind::Number { min: $min, max: $max, step: $step },
        requires: None,
        read: |s| s.$field $( [$index] )? as f64, write: |s, value| s.$field $( [$index] )? = value as _,
    } };
}
macro_rules! grade_number {
    ($label:literal, $field:ident, $bit:literal; $min:expr, $max:expr, $step:expr) => {
        SettingSpec {
            label: $label,
            page: SettingsPage::Graphics,
            group: "Color grading",
            kind: SettingKind::Number {
                min: $min,
                max: $max,
                step: $step,
            },
            requires: None,
            read: |s| s.$field as f64,
            write: |s, value| {
                s.$field = value as f32;
                s.grade_override_mask |= 1 << $bit;
            },
        }
    };
}
macro_rules! choice {
    ($group:literal, $label:literal, $field:ident, $choices:expr) => {
        SettingSpec {
            label: $label,
            page: SettingsPage::Graphics,
            group: $group,
            kind: SettingKind::Choice($choices),
            requires: None,
            read: |s| s.$field as f64,
            write: |s, value| s.$field = value as usize,
        }
    };
}

/// One inventory supplies UI controls and saved-file validation.
pub static SETTING_SPECS: &[SettingSpec] = &[
    toggle!("Point lights", "Point lights", point_lights),
    number!(Graphics, "Point lights", "Enabled point-light budget", point_light_budget; 0.0, 256.0, 8.0)
        .when(|s| s.point_lights),
    toggle!("Point lights", "Point-light shadows", point_light_shadows).when(|s| s.point_lights),
    choice!(
        "Point lights",
        "Point-light shadow-map resolution",
        point_shadow_size,
        &["512", "1024", "2048", "4096"]
    )
    .when(|s| s.point_lights && s.point_light_shadows),
    toggle!("Sun and ambient light", "Sun lighting", sun),
    number!(Graphics, "Sun and ambient light", "Sun illuminance (lux)", sun_illuminance; 0.0, 100_000.0, 1000.0)
        .when(|s| s.sun),
    number!(Graphics, "Sun and ambient light", "Sun red", sun_color[0]; 0.0, 1.0, 0.05).when(|s| s.sun),
    number!(Graphics, "Sun and ambient light", "Sun green", sun_color[1]; 0.0, 1.0, 0.05).when(|s| s.sun),
    number!(Graphics, "Sun and ambient light", "Sun blue", sun_color[2]; 0.0, 1.0, 0.05).when(|s| s.sun),
    number!(Graphics, "Sun and ambient light", "Ambient brightness", ambient_brightness; 0.0, 2000.0, 20.0),
    number!(Graphics, "Sun and ambient light", "Ambient red", ambient_color[0]; 0.0, 1.0, 0.05),
    number!(Graphics, "Sun and ambient light", "Ambient green", ambient_color[1]; 0.0, 1.0, 0.05),
    number!(Graphics, "Sun and ambient light", "Ambient blue", ambient_color[2]; 0.0, 1.0, 0.05),
    toggle!("Sun shadows", "Sun shadows", shadows).when(|s| s.sun),
    choice!(
        "Sun shadows",
        "Sun shadow-map resolution",
        shadow_size,
        &["512", "1024", "2048", "4096"]
    )
    .when(|s| s.sun && s.shadows),
    number!(Graphics, "Sun shadows", "Sun shadow cascades", shadow_cascades; 1.0, 4.0, 1.0)
        .when(|s| s.sun && s.shadows),
    number!(Graphics, "Sun shadows", "Sun shadow-distance multiplier", shadow_distance_scale; 0.25, 4.0, 0.25)
        .when(|s| s.sun && s.shadows),
    toggle!("Sky and fog", "Sky dome", sky),
    number!(Graphics, "Sky and fog", "Sky-brightness multiplier", sky_brightness_scale; 0.0, 4.0, 0.1)
        .when(|s| s.sky),
    toggle!("Sky and fog", "Exterior distance fog", fog),
    number!(Graphics, "Sky and fog", "Fog-density multiplier", fog_density_scale; 0.0, 4.0, 0.1)
        .when(|s| s.fog),
    toggle!("Water", "Water reflections", water_reflections),
    number!(Graphics, "Water", "Water reflection multiplier", water_reflection_strength; 0.0, 1.0, 0.1)
        .when(|s| s.water_reflections),
    toggle!("Water", "Water waves", water_waves),
    toggle!("Water", "Water flow normals", water_flow_normals),
    number!(Graphics, "Water", "Water wave scale", wave_scale; 0.001, 0.05, 0.001).when(|s| s.water_waves),
    number!(Graphics, "Water", "Water wave speed", wave_speed; 0.0, 2.0, 0.05).when(|s| s.water_waves),
    number!(Graphics, "Water", "Water wave strength", wave_strength; 0.0, 2.0, 0.05).when(|s| s.water_waves),
    toggle!("Culling and drawing", "CPU frustum culling", cpu_frustum_culling),
    toggle!("Culling and drawing", "GPU indirect drawing", indirect_drawing),
    toggle!("Culling and drawing", "GPU occlusion culling", occlusion_culling),
    toggle!("Culling and drawing", "Depth prepass", depth_prepass),
    choice!("Image", "MSAA", msaa, &["Off", "2x", "4x", "8x"]),
    number!(Graphics, "Image", "Exposure (EV100)", exposure_ev100; -5.0, 20.0, 0.25),
    choice!(
        "Image",
        "Tone mapping",
        tone_map,
        &[
            "None",
            "Reinhard",
            "Reinhard luminance",
            "ACES fitted",
            "AgX",
            "Somewhat boring",
            "Tony McMapface",
            "Blender filmic",
            "Khronos PBR neutral",
        ]
    ),
    number!(Graphics, "Image", "Vertical field of view (degrees)", fov_degrees; 30.0, 110.0, 5.0),
    choice!(
        "Image",
        "Visual profile (restart)",
        visual_profile,
        &["Mudcrab", "Skyrim"]
    ),
    choice!("Color grading", "Grade mode", color_grade, &["Off", "Saturation", "Full", "Approved sRGB", "Snapshot linear", "Record sRGB"]),
    choice!("Color grading", "Sky and fog preset", visual_preset, &["Vivid", "Cool Gray"]),
    grade_number!("Grade saturation", grade_saturation, 0; 0.0, 3.0, 0.05).when(|s| s.color_grade != 0),
    grade_number!("Grade brightness", grade_brightness, 1; 0.0, 3.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade contrast", grade_contrast, 2; 0.0, 3.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade tint amount", grade_tint_amount, 3; 0.0, 1.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade tint red", grade_tint_red, 4; 0.0, 1.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade tint green", grade_tint_green, 5; 0.0, 1.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade tint blue", grade_tint_blue, 6; 0.0, 1.0, 0.025).when(|s| s.color_grade >= 2),
    grade_number!("Grade pivot", grade_pivot, 7; 0.01, 0.99, 0.01).when(|s| s.color_grade >= 2),
    toggle!("Window", "VSync", vsync),
    choice!(
        "Window",
        "Window mode",
        window_mode,
        &["Windowed", "Borderless fullscreen"]
    ),
    choice!(
        "Window",
        "Windowed resolution",
        window_size,
        &["1280 x 720", "1600 x 900", "1920 x 1080", "2560 x 1440"]
    ),
    toggle!("Interface and diagnostics", "Status HUD", hud),
    toggle!(
        "Interface and diagnostics",
        "Diagnostic fallbacks (new loads)",
        diagnostic_asset_fallbacks
    ),
    number!(Streaming, "Range", "Full-detail radius (cells)", stream_radius; 0.0, 16.0, 1.0),
    number!(Streaming, "Range", "Unload radius (cells)", unload_radius; 0.0, 32.0, 1.0),
    number!(Streaming, "Terrain level of detail", "Level-4 terrain LOD distance (units)", lod_level0_distance; 0.0, 1_000_000.0, 5000.0),
    number!(Streaming, "Terrain level of detail", "Level-8 terrain LOD distance (units)", lod_level1_distance; 0.0, 1_000_000.0, 5000.0),
    number!(Streaming, "Terrain level of detail", "Level-16 terrain LOD distance (units)", lod_maximum_distance; 0.0, 1_000_000.0, 10_000.0),
    number!(Streaming, "Terrain level of detail", "Terrain LOD distance multiplier", lod_multiplier; 0.0, 4.0, 0.25),
    number!(Streaming, "Per-frame budgets", "Cell commits per frame", cell_commits; 1.0, 64.0, 1.0),
    number!(Streaming, "Per-frame budgets", "Cell commit budget (ms)", commit_ms; 0.5, 100.0, 0.5),
    number!(Streaming, "Per-frame budgets", "Cell unloads per frame (0 = unlimited)", cell_unloads; 0.0, 128.0, 1.0),
    number!(Streaming, "Per-frame budgets", "Model spawns per frame (0 = unlimited)", model_spawns; 0.0, 512.0, 8.0),
    number!(Streaming, "Per-frame budgets", "Asset upload MiB per frame (0 = unlimited)", upload_mib; 0.0, 256.0, 4.0),
    number!(Streaming, "Threads", "IO threads (restart; 0 = automatic)", io_threads; 0.0, 64.0, 1.0),
];

impl SettingSpec {
    const fn when(self, requires: fn(&RenderingSettings) -> bool) -> Self {
        Self {
            requires: Some(requires),
            ..self
        }
    }

    /// False when a controlling setting makes this one have no visible effect.
    pub fn relevant(&self, settings: &RenderingSettings) -> bool {
        self.requires.is_none_or(|requires| requires(settings))
    }

    /// True when the session value differs from what Reset defaults restores.
    pub fn is_modified(&self, settings: &RenderingSettings) -> bool {
        if let Some(bit) = grade_override_bit(self.label)
            && settings.grade_override_mask & (1 << bit) == 0
        {
            return false;
        }
        (self.read)(settings) != (self.read)(&RenderingSettings::default())
    }

    pub fn value_label(&self, settings: &RenderingSettings) -> String {
        let value = (self.read)(settings);
        match self.kind {
            SettingKind::Toggle => if value == 0.0 { "Off" } else { "On" }.to_owned(),
            SettingKind::Choice(labels) => {
                labels.get(value as usize).unwrap_or(&"Unknown").to_string()
            }
            SettingKind::Number { step, .. } => {
                let precision = if step < 0.01 {
                    3
                } else if step < 1.0 {
                    2
                } else {
                    0
                };
                format!("{value:.precision$}")
            }
        }
    }

    pub fn adjust(&self, settings: &mut RenderingSettings, direction: i32) {
        let old_occlusion = settings.occlusion_culling;
        let old_radius = settings.stream_radius;
        let old = (self.read)(settings);
        let value = match self.kind {
            SettingKind::Toggle => u8::from(old == 0.0) as f64,
            SettingKind::Choice(labels) => {
                (old as i32 + direction).rem_euclid(labels.len() as i32) as f64
            }
            SettingKind::Number { min, max, step } => {
                (old + f64::from(direction) * step).clamp(min, max)
            }
        };
        (self.write)(settings, value);
        if !old_occlusion && settings.occlusion_culling {
            settings.indirect_drawing = true;
            settings.depth_prepass = true;
        }
        if !settings.indirect_drawing || !settings.depth_prepass {
            settings.occlusion_culling = false;
        }
        if old_radius != settings.stream_radius {
            settings.unload_radius = settings.unload_radius.max(settings.stream_radius + 1);
        } else {
            settings.unload_radius = settings.unload_radius.max(settings.stream_radius);
        }
    }
}

fn grade_override_bit(label: &str) -> Option<u8> {
    Some(match label {
        "Grade saturation" => 0,
        "Grade brightness" => 1,
        "Grade contrast" => 2,
        "Grade tint amount" => 3,
        "Grade tint red" => 4,
        "Grade tint green" => 5,
        "Grade tint blue" => 6,
        "Grade pivot" => 7,
        _ => return None,
    })
}

impl RenderingSettings {
    pub(crate) fn grade_overrides(&self) -> crate::color_pipeline::SceneGradeOverrides {
        let value = |bit: u8, value| (self.grade_override_mask & (1 << bit) != 0).then_some(value);
        crate::color_pipeline::SceneGradeOverrides {
            saturation: value(0, self.grade_saturation),
            brightness: value(1, self.grade_brightness),
            contrast: value(2, self.grade_contrast),
            tint_amount: value(3, self.grade_tint_amount),
            tint_red: value(4, self.grade_tint_red),
            tint_green: value(5, self.grade_tint_green),
            tint_blue: value(6, self.grade_tint_blue),
            pivot: value(7, self.grade_pivot),
        }
    }

    /// Returns whether any shown value changed.
    pub(crate) fn show_resolved_grade(
        &mut self,
        grade: crate::color_pipeline::SkyrimCinematicGrade,
        pivot: f32,
    ) -> bool {
        let mut changed = false;
        for (bit, slot, value) in [
            (0, &mut self.grade_saturation, grade.saturation),
            (1, &mut self.grade_brightness, grade.brightness),
            (2, &mut self.grade_contrast, grade.contrast),
            (3, &mut self.grade_tint_amount, grade.tint_amount),
            (4, &mut self.grade_tint_red, grade.tint_rgb[0]),
            (5, &mut self.grade_tint_green, grade.tint_rgb[1]),
            (6, &mut self.grade_tint_blue, grade.tint_rgb[2]),
            (7, &mut self.grade_pivot, pivot),
        ] {
            // Assign only on a real difference: this runs inside `apply_render_settings`,
            // whose own `ResMut` change tick is what wakes the menu, the weather and the
            // scene grade pipeline. Writing an equal value would wake them for nothing.
            if self.grade_override_mask & (1 << bit) == 0 && *slot != value {
                *slot = value;
                changed = true;
            }
        }
        changed
    }

    fn restore_grade_controls(&mut self, saved: &Self) {
        self.color_grade = saved.color_grade;
        self.visual_preset = saved.visual_preset;
        self.grade_saturation = saved.grade_saturation;
        self.grade_brightness = saved.grade_brightness;
        self.grade_contrast = saved.grade_contrast;
        self.grade_tint_amount = saved.grade_tint_amount;
        self.grade_tint_red = saved.grade_tint_red;
        self.grade_tint_green = saved.grade_tint_green;
        self.grade_tint_blue = saved.grade_tint_blue;
        self.grade_pivot = saved.grade_pivot;
        self.grade_override_mask = saved.grade_override_mask;
    }

    fn restore_grade_values(&mut self, saved: &Self) {
        self.grade_saturation = saved.grade_saturation;
        self.grade_brightness = saved.grade_brightness;
        self.grade_contrast = saved.grade_contrast;
        self.grade_tint_amount = saved.grade_tint_amount;
        self.grade_tint_red = saved.grade_tint_red;
        self.grade_tint_green = saved.grade_tint_green;
        self.grade_tint_blue = saved.grade_tint_blue;
        self.grade_pivot = saved.grade_pivot;
        self.grade_override_mask = saved.grade_override_mask;
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported settings version".into());
        }
        if self.grade_override_mask & !0xff != 0 {
            return Err("grade override mask contains unknown fields".into());
        }
        for spec in SETTING_SPECS {
            let grade_bit = grade_override_bit(spec.label);
            if grade_bit.is_some_and(|bit| self.grade_override_mask & (1 << bit) == 0) {
                continue;
            }
            let value = (spec.read)(self);
            let (min, max) = match spec.kind {
                SettingKind::Toggle => (0.0, 1.0),
                SettingKind::Choice(labels) => (0.0, (labels.len() - 1) as f64),
                SettingKind::Number { min, max, .. } => (min, max),
            };
            if !value.is_finite() || !(min..=max).contains(&value) {
                return Err(format!("{} must be between {min} and {max}", spec.label));
            }
        }
        if self.unload_radius < self.stream_radius {
            return Err("unload radius must cover the full-detail radius".into());
        }
        if self.occlusion_culling && (!self.depth_prepass || !self.indirect_drawing) {
            return Err("occlusion culling requires depth prepass and indirect drawing".into());
        }
        Ok(())
    }

    fn take_launch_values(&mut self, config: &EngineConfig) {
        self.color_grade = config.color_grade.index();
        self.visual_preset =
            usize::from(config.visual_preset == crate::color_pipeline::SceneVisualPreset::CoolGray);
        if grade_source_is_launch_owned(&config.settings_overrides) {
            self.grade_override_mask = 0;
        }
        self.point_lights = config.lights;
        self.stream_radius = config.stream_radius;
        self.unload_radius = config.unload_radius;
        self.lod_level0_distance = config.terrain_lod.block_level0_distance;
        self.lod_level1_distance = config.terrain_lod.block_level1_distance;
        self.lod_maximum_distance = config.terrain_lod.block_maximum_distance;
        self.lod_multiplier = config.terrain_lod.split_distance_mult;
        self.cell_commits = config.max_cell_commits_per_frame;
        self.commit_ms = config.max_commit_micros_per_frame as f64 / 1000.0;
        self.cell_unloads = config.max_cell_unloads_per_frame;
        self.model_spawns = config.max_model_spawns_per_frame;
        self.upload_mib = config.max_upload_mib_per_frame;
        self.io_threads = config.io_threads;
        self.diagnostic_asset_fallbacks = config.diagnostic_asset_fallbacks;
        self.visual_profile = config.visual_profile.index();
    }

    fn retain_explicit_launch_values(&mut self, config: &EngineConfig) {
        let mut launch = RenderingSettings::default();
        launch.take_launch_values(config);
        self.take_launch_owned_values(&launch, &config.settings_overrides);
    }

    /// Copies from `source` the fields that the explicit launch `overrides` own.
    fn take_launch_owned_values(&mut self, source: &RenderingSettings, overrides: &[String]) {
        if overrides.iter().any(|flag| flag == "--visual-preset") {
            self.visual_preset = source.visual_preset;
        }
        if grade_source_is_launch_owned(overrides) {
            self.color_grade = source.color_grade;
            self.restore_grade_values(source);
        }
        for flag in overrides {
            match flag.as_str() {
                "--lights" => self.point_lights = source.point_lights,
                "--stream-radius" => {
                    self.stream_radius = source.stream_radius;
                    self.unload_radius = source.unload_radius;
                }
                "ini:stream-radius" => {
                    if !overrides.iter().any(|s| s == "--stream-radius") {
                        self.stream_radius = source.stream_radius;
                        self.unload_radius = source.unload_radius;
                    }
                }
                "ini:fBlockLevel0Distance" => self.lod_level0_distance = source.lod_level0_distance,
                "ini:fBlockLevel1Distance" => self.lod_level1_distance = source.lod_level1_distance,
                "ini:fBlockMaximumDistance" => {
                    self.lod_maximum_distance = source.lod_maximum_distance
                }
                "ini:fSplitDistanceMult" => self.lod_multiplier = source.lod_multiplier,
                "--max-commit-ms" => self.commit_ms = source.commit_ms,
                "--max-unloads-per-frame" => self.cell_unloads = source.cell_unloads,
                "--max-model-spawns-per-frame" => self.model_spawns = source.model_spawns,
                "--max-upload-mib-per-frame" => self.upload_mib = source.upload_mib,
                "--io-threads" => self.io_threads = source.io_threads,
                "--visual-profile" => self.visual_profile = source.visual_profile,
                "--diagnostic-asset-fallbacks" => {
                    self.diagnostic_asset_fallbacks = source.diagnostic_asset_fallbacks
                }
                _ => {}
            }
        }
    }

    pub(crate) fn apply_config(&self, config: &mut EngineConfig) {
        if let Some(mode) = crate::color_pipeline::SceneGradeMode::from_index(self.color_grade) {
            config.color_grade = mode;
        }
        config.visual_preset = if self.visual_preset == 0 {
            crate::color_pipeline::SceneVisualPreset::Vivid
        } else {
            crate::color_pipeline::SceneVisualPreset::CoolGray
        };
        config.lights = self.point_lights;
        config.stream_radius = self.stream_radius;
        config.unload_radius = self.unload_radius;
        config.terrain_lod.block_level0_distance = self.lod_level0_distance;
        config.terrain_lod.block_level1_distance = self.lod_level1_distance;
        config.terrain_lod.block_maximum_distance = self.lod_maximum_distance;
        config.terrain_lod.split_distance_mult = self.lod_multiplier;
        config.max_cell_commits_per_frame = self.cell_commits;
        config.max_commit_micros_per_frame = (self.commit_ms * 1000.0) as u64;
        config.max_cell_unloads_per_frame = self.cell_unloads;
        config.max_model_spawns_per_frame = self.model_spawns;
        config.max_upload_mib_per_frame = self.upload_mib;
        config.diagnostic_asset_fallbacks = self.diagnostic_asset_fallbacks;
    }

    fn config_differs(&self, config: &EngineConfig) -> bool {
        self.color_grade != config.color_grade.index()
            || self.visual_preset
                != usize::from(
                    config.visual_preset == crate::color_pipeline::SceneVisualPreset::CoolGray,
                )
            || self.point_lights != config.lights
            || self.stream_radius != config.stream_radius
            || self.unload_radius != config.unload_radius
            || self.lod_level0_distance != config.terrain_lod.block_level0_distance
            || self.lod_level1_distance != config.terrain_lod.block_level1_distance
            || self.lod_maximum_distance != config.terrain_lod.block_maximum_distance
            || self.lod_multiplier != config.terrain_lod.split_distance_mult
            || self.cell_commits != config.max_cell_commits_per_frame
            || (self.commit_ms * 1000.0) as u64 != config.max_commit_micros_per_frame
            || self.cell_unloads != config.max_cell_unloads_per_frame
            || self.model_spawns != config.max_model_spawns_per_frame
            || self.upload_mib != config.max_upload_mib_per_frame
            || self.diagnostic_asset_fallbacks != config.diagnostic_asset_fallbacks
    }
}

fn grade_source_is_launch_owned(overrides: &[String]) -> bool {
    [
        "--color-grade",
        "--grade-preset",
        "--tone-map",
        "--tone-map-params",
    ]
    .iter()
    .any(|flag| overrides.iter().any(|value| value == flag))
}

#[derive(Resource)]
pub struct SettingsFile {
    pub path: Option<PathBuf>,
    pub status: String,
    /// The preferences the file held before explicit launch options replaced them in the running
    /// settings. A save writes these values back for the fields those options own.
    stored: RenderingSettings,
    /// The explicit launch options from the run that loaded `stored`.
    launch_overrides: Vec<String>,
    preserve_unavailable_record_mode: bool,
    unavailable_record_mode_replaced: bool,
}

impl SettingsFile {
    pub fn session_only() -> Self {
        Self {
            path: None,
            status: "Changes apply now. This preview does not save preferences.".into(),
            stored: RenderingSettings::default(),
            launch_overrides: Vec::new(),
            preserve_unavailable_record_mode: false,
            unavailable_record_mode_replaced: false,
        }
    }

    pub(crate) fn note_grade_mode_selection(&mut self) {
        if self.preserve_unavailable_record_mode {
            self.unavailable_record_mode_replaced = true;
        }
    }

    pub(crate) fn restore_defaults(&mut self, settings: &mut RenderingSettings) {
        self.note_grade_mode_selection();
        *settings = RenderingSettings::default();
        self.status = "Defaults restored for this session. Choose Save to keep them.".into();
    }

    pub fn save(&mut self, settings: &RenderingSettings) {
        self.status = match self.path.as_ref() {
            Some(path) => match save_settings(path, &self.persisted(settings)) {
                Ok(()) => "Settings saved. IO-thread changes apply after restart.".into(),
                Err(error) => format!("Could not save settings: {error}"),
            },
            None => "Saving is unavailable for this session.".into(),
        };
    }

    /// What the file should hold for these settings. A field owned by an explicit launch option
    /// keeps the value the file held before the run, or the menu default when there was no valid
    /// file, so a launch value is never written. The running settings are not changed, and the
    /// launch value stays in effect until the next run.
    fn persisted(&self, settings: &RenderingSettings) -> RenderingSettings {
        let mut persisted = settings.clone();
        persisted.take_launch_owned_values(&self.stored, &self.launch_overrides);
        if self.preserve_unavailable_record_mode && !self.unavailable_record_mode_replaced {
            persisted.color_grade = self.stored.color_grade;
        }
        persisted
    }
}

fn preferences_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(|root| PathBuf::from(root).join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|root| Path::new(root).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|root| PathBuf::from(root).join(".config")));
    root.map(|root| root.join("Mudcrab/settings.json"))
}

pub(crate) fn prepare_settings(config: &mut EngineConfig) -> (RenderingSettings, SettingsFile) {
    prepare_settings_at(config, preferences_path())
}

fn prepare_settings_at(
    config: &mut EngineConfig,
    path: Option<PathBuf>,
) -> (RenderingSettings, SettingsFile) {
    let mut settings = RenderingSettings::default();
    settings.take_launch_values(config);
    let mut file = SettingsFile {
        path,
        status: "Changes apply now. Choose Save to keep them across runs.".into(),
        stored: RenderingSettings::default(),
        launch_overrides: config.settings_overrides.clone(),
        preserve_unavailable_record_mode: false,
        unavailable_record_mode_replaced: false,
    };
    if let Some(path) = &file.path {
        match std::fs::read(path) {
            Ok(bytes) => {
                let saved = serde_json::from_slice::<RenderingSettings>(&bytes)
                    .map_err(|e| e.to_string())
                    .and_then(|s| s.validate().map(|()| s));
                match saved {
                    Ok(saved) => {
                        file.stored = saved.clone();
                        settings = saved;
                        settings.retain_explicit_launch_values(config);
                        if settings.color_grade
                            == crate::color_pipeline::SceneGradeMode::RecordSrgb.index()
                            && config.image_space_selection().ok().flatten().is_none()
                            && !config
                                .settings_overrides
                                .iter()
                                .any(|flag| flag == "--color-grade")
                        {
                            settings.color_grade = config.color_grade.index();
                            file.preserve_unavailable_record_mode = true;
                            file.status = "Saved record-srgb grade is unavailable without an explicit image-space selection; using the approved default.".into();
                        }
                    }
                    Err(error) => {
                        file.status = format!("Saved settings ignored: {error}");
                        warn!(%error, "saved rendering settings ignored");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => file.status = format!("Could not read settings: {error}"),
        }
    }
    settings.apply_config(config);
    config.io_threads = settings.io_threads;
    if let Some(profile) = crate::visual_profile::VisualProfile::from_index(settings.visual_profile)
    {
        config.visual_profile = profile;
    }
    (settings, file)
}

fn save_settings(path: &Path, settings: &RenderingSettings) -> Result<(), String> {
    settings.validate()?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    let parent = path.parent().ok_or("settings path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let temporary = parent.join(format!("settings-{}-{stamp}.tmp", std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e: std::io::Error| e.to_string())
}

/// Targets affected by interactive renderer preferences, grouped under Bevy's system parameter limit.
#[derive(bevy::ecs::system::SystemParam)]
#[allow(clippy::type_complexity)]
pub(crate) struct RenderSettingsTargets<'w, 's> {
    cameras: Query<
        'w,
        's,
        (
            Entity,
            &'static mut Exposure,
            &'static mut Tonemapping,
            &'static mut Msaa,
            &'static mut Projection,
        ),
        With<StreamingCamera>,
    >,
    reflections: Query<'w, 's, Entity, With<crate::render::WaterReflectionCamera>>,
    /// The sun and the "a sun light appeared since the last apply" check need conflicting
    /// access (`&mut DirectionalLight` against that component's change tick), so they share a
    /// `ParamSet`. Every other target's marker is not mutated by this system, so those live in
    /// `new_targets` as one query.
    sun: ParamSet<
        'w,
        's,
        (
            Query<
                'w,
                's,
                (
                    &'static mut DirectionalLight,
                    &'static mut CascadeShadowConfig,
                    Option<&'static crate::world::environment::interior::WorldEnvironmentLight>,
                ),
            >,
            Query<'w, 's, (), Added<DirectionalLight>>,
        ),
    >,
    ambient: Option<ResMut<'w, GlobalAmbientLight>>,
    scene_environment: Option<Res<'w, crate::world::environment::interior::SceneEnvironment>>,
    shadow_map: Option<ResMut<'w, DirectionalLightShadowMap>>,
    point_shadow_map: Option<ResMut<'w, PointLightShadowMap>>,
    upload: Option<ResMut<'w, RenderAssetBytesPerFrame>>,
    windows: Query<'w, 's, (&'static mut Window, Ref<'static, PrimaryWindow>)>,
    hud: Query<'w, 's, &'static mut Node, With<crate::physics::NoclipOverlay>>,
    /// Targets that appeared since the last apply. A camera, light, window or overlay
    /// spawned after the last preference change still needs the current settings, so
    /// `apply_render_settings` runs for it even though nothing else changed.
    new_targets: Query<
        'w,
        's,
        (),
        Or<(
            Added<StreamingCamera>,
            Added<crate::render::WaterReflectionCamera>,
            Added<PrimaryWindow>,
            Added<crate::physics::NoclipOverlay>,
        )>,
    >,
}

/// Apply only on preference changes. Weather, light budgets and water read the same resource.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_render_settings(
    mut settings: ResMut<RenderingSettings>,
    mut config: ResMut<EngineConfig>,
    mut commands: Commands,
    grade_source: Option<Res<crate::color_pipeline::SceneGradeSource>>,
    mut tone_map_source: Option<ResMut<crate::tone_mapping::SceneToneMapSource>>,
    current_mode: Option<Res<crate::color_pipeline::SceneGradeMode>>,
    current_preset: Option<Res<crate::color_pipeline::SceneVisualPreset>>,
    current_overrides: Option<Res<crate::color_pipeline::SceneGradeOverrides>>,
    mut sky_palette: Option<ResMut<crate::sky::SkyPalette>>,
    mut targets: RenderSettingsTargets,
    mut previous: Local<Option<RenderingSettings>>,
    adapter: Option<Res<RenderAdapter>>,
    mut file: Option<ResMut<SettingsFile>>,
) {
    // This system writes some of its own inputs (the shown grade values, the MSAA fallback and
    // the engine config), so the resource tick alone cannot mean "a preference changed": compare
    // against the snapshot left by the previous run, and also run when the engine config or the
    // renderer changed underneath it, or when a render target appeared since the last run.
    // A settled frame returns here and does none of the work below.
    let preferences_changed = settings.is_changed()
        || previous.as_ref() != Some(&*settings)
        || config.is_changed()
        || adapter.as_ref().is_some_and(|adapter| adapter.is_changed())
        || !targets.new_targets.is_empty()
        || !targets.sun.p1().is_empty();
    if !preferences_changed {
        return;
    }
    let old_mode = current_mode
        .as_deref()
        .copied()
        .unwrap_or(crate::color_pipeline::SceneGradeMode::SnapshotSrgb);
    let old_preset = current_preset
        .as_deref()
        .copied()
        .unwrap_or(crate::color_pipeline::SceneVisualPreset::CoolGray);
    let mode = crate::color_pipeline::SceneGradeMode::from_index(settings.color_grade);
    let preset = if settings.visual_preset == 0 {
        crate::color_pipeline::SceneVisualPreset::Vivid
    } else {
        crate::color_pipeline::SceneVisualPreset::CoolGray
    };
    let source = grade_source.as_deref().cloned().unwrap_or_default();
    let mut tone = tone_map_source.as_deref().cloned().unwrap_or_default();
    let selected_tonemapping = TONE_MAPS[settings.tone_map];
    tone.bevy_tonemapping = selected_tonemapping;
    let tone_map_changed = tone_map_source
        .as_deref()
        .is_some_and(|source| source.bevy_tonemapping != selected_tonemapping);
    if tone_map_changed && let Some(source) = tone_map_source.as_deref_mut() {
        source.bevy_tonemapping = selected_tonemapping;
    }
    let overrides = settings.grade_overrides();
    let resolved = mode
        .ok_or_else(|| "unsupported persisted grade mode".to_owned())
        .and_then(|mode| tone.resolve_with_overrides(&source, mode, preset, overrides));
    match resolved {
        Ok(resolved) => {
            let mode = mode.expect("resolved mode is present");
            if previous
                .as_ref()
                .is_some_and(|previous| previous.color_grade != settings.color_grade)
                && let Some(file) = file.as_deref_mut()
            {
                file.note_grade_mode_selection();
            }
            // Through `ResMut` the call itself would mark the resource changed even when every
            // value already matches, so the tick is set only for a real difference.
            if settings
                .bypass_change_detection()
                .show_resolved_grade(resolved.grade, resolved.post.pivot)
            {
                settings.set_changed();
            }
            let next_palette = source.sky_palette(preset);
            let palette_changed = sky_palette.as_deref() != Some(&next_palette);
            if palette_changed && let Some(palette) = sky_palette.as_deref_mut() {
                *palette = next_palette;
            }
            // Re-inserting an equal value still bumps the change tick, and every reader of
            // these resources (the scene grade pipeline, the menu) keys its work on that
            // tick. Insert only what actually differs.
            if current_mode.as_deref() != Some(&mode) {
                commands.insert_resource(mode);
            }
            if current_preset.as_deref() != Some(&preset) {
                commands.insert_resource(preset);
            }
            if current_overrides.as_deref() != Some(&overrides) {
                commands.insert_resource(overrides);
            }
        }
        Err(error) => {
            if let Some(previous) = previous.as_ref() {
                settings.restore_grade_controls(previous);
            } else {
                settings.color_grade = old_mode.index();
                settings.visual_preset =
                    usize::from(old_preset == crate::color_pipeline::SceneVisualPreset::CoolGray);
            }
            if let Some(file) = file.as_deref_mut() {
                file.status = format!("Grade change rejected: {error}");
            }
        }
    }
    let samples = [1, 2, 4, 8][settings.msaa];
    if settings.msaa != 0
        && let Some(adapter) = adapter
        && [
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureFormat::Depth32Float,
        ]
        .iter()
        .any(|format| {
            !adapter
                .get_texture_format_features(*format)
                .flags
                .sample_count_supported(samples)
        })
    {
        settings.msaa = 0;
        if let Some(file) = file.as_deref_mut() {
            file.status = format!("{samples}x MSAA is unavailable on this renderer; using Off.");
        }
    }
    if settings.config_differs(&config) {
        settings.apply_config(&mut config);
    }
    for (entity, mut exposure, mut tone_map, mut msaa, mut projection) in &mut targets.cameras {
        exposure.ev100 = settings.exposure_ev100;
        if tone.mode == crate::tone_mapping::SceneToneMapMode::Tony {
            *tone_map = TONE_MAPS[settings.tone_map];
        }
        *msaa = MSAA_MODES[settings.msaa];
        if let Projection::Perspective(p) = &mut *projection {
            p.fov = settings.fov_degrees.to_radians();
        }
        apply_camera_features(&mut commands, entity, &settings);
    }
    for entity in &targets.reflections {
        commands.entity(entity).insert(MSAA_MODES[settings.msaa]);
        apply_camera_features(&mut commands, entity, &settings);
    }
    for (mut light, mut cascades, environment_light) in &mut targets.sun.p0() {
        // The scene consumer applies environment lighting after Update. Preferences never
        // overwrite authored interior RGB; fixture lights keep the existing direct controls.
        if environment_light.is_none() || targets.scene_environment.is_none() {
            light.illuminance = if settings.sun {
                settings.sun_illuminance
            } else {
                0.0
            };
            light.color = Color::srgb(
                settings.sun_color[0],
                settings.sun_color[1],
                settings.sun_color[2],
            );
            light.shadow_maps_enabled = settings.sun && settings.shadows;
        }
        let base = crate::app::sun_shadow_cascades(&config);
        let maximum =
            base.bounds.last().copied().unwrap_or(1000.0) * settings.shadow_distance_scale;
        *cascades = CascadeShadowConfigBuilder {
            minimum_distance: 7.0,
            maximum_distance: maximum,
            first_cascade_far_bound: 700.0_f32.min(maximum * 0.5),
            num_cascades: settings.shadow_cascades,
            ..default()
        }
        .into();
    }
    if targets.scene_environment.is_none()
        && let Some(ambient) = targets.ambient.as_deref_mut()
    {
        ambient.brightness = settings.ambient_brightness;
        ambient.color = Color::srgb(
            settings.ambient_color[0],
            settings.ambient_color[1],
            settings.ambient_color[2],
        );
    }
    if let Some(shadow_map) = targets.shadow_map.as_deref_mut() {
        shadow_map.size = SHADOW_SIZES[settings.shadow_size];
    }
    if let Some(shadow_map) = targets.point_shadow_map.as_deref_mut() {
        shadow_map.size = SHADOW_SIZES[settings.point_shadow_size];
    }
    if let Some(upload) = targets.upload.as_deref_mut() {
        *upload = RenderAssetBytesPerFrame {
            max_bytes: config.max_upload_bytes_per_frame(),
        };
    }
    for (mut window, primary) in &mut targets.windows {
        // A primary window that replaced the first one starts at its own defaults, so it
        // takes the configured mode and size whether or not the preference changed.
        let added = primary.is_added();
        window.present_mode = if settings.vsync {
            PresentMode::AutoVsync
        } else {
            PresentMode::AutoNoVsync
        };
        if added
            || previous
                .as_ref()
                .is_none_or(|previous| previous.window_mode != settings.window_mode)
        {
            window.mode = if settings.window_mode == 0 {
                WindowMode::Windowed
            } else {
                WindowMode::BorderlessFullscreen(MonitorSelection::Current)
            };
        }
        if added
            || previous
                .as_ref()
                .is_none_or(|previous| previous.window_size != settings.window_size)
        {
            let (width, height) = WINDOW_SIZES[settings.window_size];
            window.resolution.set(width as f32, height as f32);
        }
    }
    for mut node in &mut targets.hud {
        node.display = if settings.hud {
            Display::Flex
        } else {
            Display::None
        };
    }
    *previous = Some(settings.clone());
}

fn apply_camera_features(commands: &mut Commands, camera: Entity, settings: &RenderingSettings) {
    let mut camera = commands.entity(camera);
    if settings.cpu_frustum_culling {
        camera.remove::<NoCpuCulling>();
    } else {
        camera.insert(NoCpuCulling);
    }
    if settings.indirect_drawing {
        camera.remove::<NoIndirectDrawing>();
    } else {
        camera.insert(NoIndirectDrawing);
    }
    if settings.depth_prepass {
        camera.insert(DepthPrepass);
    } else {
        camera.remove::<DepthPrepass>();
    }
    if settings.occlusion_culling {
        camera.insert(OcclusionCulling);
    } else {
        camera.remove::<OcclusionCulling>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConfigAction, console::AppConsoleExt};

    fn launch(args: &[String]) -> EngineConfig {
        match EngineConfig::from_args(args.iter().cloned()).unwrap() {
            ConfigAction::Run(config) => *config,
            ConfigAction::Help => panic!("expected a run"),
        }
    }

    fn unavailable_record_fixture(path: &Path) -> (EngineConfig, RenderingSettings, SettingsFile) {
        save_settings(
            path,
            &RenderingSettings {
                color_grade: crate::color_pipeline::SceneGradeMode::RecordSrgb.index(),
                ..default()
            },
        )
        .unwrap();
        let mut config = EngineConfig::default();
        let (settings, file) = prepare_settings_at(&mut config, Some(path.to_owned()));
        assert!(file.preserve_unavailable_record_mode);
        assert_eq!(
            settings.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        (config, settings, file)
    }

    fn unavailable_record_app(path: &Path) -> App {
        let (config, settings, file) = unavailable_record_fixture(path);
        let mode = config.color_grade;
        let preset = config.visual_preset;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(config)
            .insert_resource(settings)
            .insert_resource(file)
            .insert_resource(mode)
            .insert_resource(preset)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .insert_resource(crate::sky::SkyPalette::default())
            .add_systems(Update, apply_render_settings);
        app.update();
        app
    }

    fn adjust_grade_mode(app: &mut App, direction: i32) {
        let spec = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Grade mode")
            .unwrap();
        spec.adjust(
            app.world_mut()
                .resource_mut::<RenderingSettings>()
                .into_inner(),
            direction,
        );
        app.update();
    }

    fn save_app_settings(app: &mut App) {
        let settings = app.world().resource::<RenderingSettings>().clone();
        app.world_mut()
            .resource_mut::<SettingsFile>()
            .save(&settings);
    }

    #[test]
    fn defaults_preserve_world_lighting_on_first_launch_and_reset() {
        for minimal_file in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("settings.json");
            if minimal_file {
                std::fs::write(&path, r#"{"version":1}"#).unwrap();
            }
            let mut config = EngineConfig::default();
            let (settings, file) = prepare_settings_at(&mut config, Some(path.clone()));
            let weather = crate::sky::WeatherLighting::default();
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .insert_resource(settings)
                .insert_resource(file)
                .insert_resource(GlobalAmbientLight {
                    color: Color::srgb(0.2, 0.3, 0.4),
                    brightness: 321.0,
                    ..default()
                })
                .add_systems(Update, apply_render_settings);
            let sun = app
                .world_mut()
                .spawn((
                    DirectionalLight {
                        color: Color::srgb(0.4, 0.3, 0.2),
                        illuminance: 123.0,
                        ..default()
                    },
                    crate::app::sun_shadow_cascades(&config),
                ))
                .id();
            app.insert_resource(config);
            let assert_lighting = |app: &App,
                                   sun_color: Color,
                                   sun_illuminance,
                                   ambient_color: Color,
                                   ambient_brightness| {
                let light = app.world().get::<DirectionalLight>(sun).unwrap();
                let ambient = app.world().resource::<GlobalAmbientLight>();
                for (actual, expected) in [(light.color, sun_color), (ambient.color, ambient_color)]
                {
                    for (actual, expected) in actual
                        .to_linear()
                        .to_f32_array()
                        .into_iter()
                        .zip(expected.to_linear().to_f32_array())
                    {
                        assert!((actual - expected).abs() < 1e-6);
                    }
                }
                assert_eq!(light.illuminance, sun_illuminance);
                assert_eq!(ambient.brightness, ambient_brightness);
            };
            let assert_defaults = |app: &App| {
                assert_lighting(
                    app,
                    weather.sunlight,
                    crate::lights::SUN_ILLUMINANCE,
                    weather.mean_ambient(),
                    crate::lights::AMBIENT_ILLUMINANCE,
                )
            };
            app.update();
            assert_defaults(&app);

            {
                let mut settings = app.world_mut().resource_mut::<RenderingSettings>();
                settings.sun_color = [1.0; 3];
                settings.ambient_color = [0.1; 3];
                settings.sun_illuminance = 4_000.0;
                settings.ambient_brightness = 100.0;
            }
            app.update();
            assert_lighting(
                &app,
                Color::srgb(1.0, 1.0, 1.0),
                4_000.0,
                Color::srgb(0.1, 0.1, 0.1),
                100.0,
            );
            app.world_mut()
                .resource_scope(|world, mut file: Mut<SettingsFile>| {
                    file.restore_defaults(&mut world.resource_mut::<RenderingSettings>());
                });
            app.update();
            assert_defaults(&app);
            save_app_settings(&mut app);
            let mut config = EngineConfig::default();
            let (reloaded, _) = prepare_settings_at(&mut config, Some(path));
            *app.world_mut().resource_mut::<RenderingSettings>() = reloaded;
            app.update();
            assert_defaults(&app);
        }
    }

    #[test]
    fn menu_numeric_adjustment_reaches_scene_camera_as_explicit_override() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings::default())
            .insert_resource(SettingsFile::session_only())
            .insert_resource(crate::color_pipeline::SceneGradeMode::SnapshotSrgb)
            .insert_resource(crate::color_pipeline::SceneVisualPreset::CoolGray)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .insert_resource(crate::sky::SkyPalette::default())
            .add_systems(Update, apply_render_settings)
            .add_systems(PostUpdate, crate::color_pipeline::apply_scene_grade_mode);
        let camera = app
            .world_mut()
            .spawn(crate::color_pipeline::SceneColorPipeline::default())
            .id();
        app.update();
        let before = app
            .world()
            .get::<crate::color_pipeline::SkyrimCinematicPost>(camera)
            .unwrap()
            .brightness;
        let spec = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Grade brightness")
            .unwrap();
        spec.adjust(
            app.world_mut()
                .resource_mut::<RenderingSettings>()
                .into_inner(),
            1,
        );
        app.update();
        let after = app
            .world()
            .get::<crate::color_pipeline::SkyrimCinematicPost>(camera)
            .unwrap()
            .brightness;
        assert!(after > before);
        assert_eq!(
            app.world()
                .resource::<crate::color_pipeline::SceneGradeOverrides>()
                .brightness,
            Some(after)
        );
        assert!(spec.is_modified(app.world().resource::<RenderingSettings>()));
        app.world_mut()
            .resource_mut::<RenderingSettings>()
            .visual_preset = 0;
        app.update();
        assert_eq!(
            *app.world()
                .resource::<crate::color_pipeline::SceneVisualPreset>(),
            crate::color_pipeline::SceneVisualPreset::Vivid
        );
        assert_eq!(
            *app.world().resource::<crate::sky::SkyPalette>(),
            crate::sky::SkyPalette::SKYRIM_CLEAR_DAY
        );
        assert_eq!(
            app.world()
                .get::<crate::color_pipeline::SkyrimCinematicPost>(camera)
                .unwrap()
                .brightness,
            after
        );
    }

    /// What `apply_render_settings` did on the frame that just ran, seen by a system that
    /// runs after it on the same frame.
    #[derive(Resource, Default)]
    struct SettingsApplyProbe {
        settings_changed: bool,
        grade_mode_changed: bool,
    }

    /// Records, after the apply, whether the settings and the scene grade mode changed.
    fn probe_settings_apply(
        settings: Res<RenderingSettings>,
        mode: Option<Res<crate::color_pipeline::SceneGradeMode>>,
        mut probe: ResMut<SettingsApplyProbe>,
    ) {
        probe.settings_changed = settings.is_changed();
        probe.grade_mode_changed = mode.is_some_and(|mode| mode.is_changed());
    }

    /// With nothing changed, the apply leaves the settings and the scene grade untouched.
    #[test]
    fn settled_render_settings_are_not_reapplied_every_frame() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings::default())
            .insert_resource(SettingsFile::session_only())
            .insert_resource(crate::color_pipeline::SceneGradeMode::SnapshotSrgb)
            .insert_resource(crate::color_pipeline::SceneVisualPreset::CoolGray)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .insert_resource(crate::sky::SkyPalette::default())
            .init_resource::<SettingsApplyProbe>()
            .add_systems(Update, apply_render_settings)
            .add_systems(Last, probe_settings_apply);
        app.update();
        // The launch frame legitimately applies the settings the run started with. Every
        // later frame with no menu action, no config change and no new target is settled:
        // it must not dirty `RenderingSettings` nor replace an identical scene grade.
        for frame in 1..=3 {
            app.update();
            let probe = app.world().resource::<SettingsApplyProbe>();
            assert!(
                !probe.settings_changed,
                "frame {frame}: apply_render_settings dirtied RenderingSettings with nothing changed"
            );
            assert!(
                !probe.grade_mode_changed,
                "frame {frame}: apply_render_settings replaced an identical SceneGradeMode"
            );
        }
        // A preference change that does not touch the grade (here the HUD toggle) still runs
        // the apply, but must not replace the identical scene grade resources: every reader
        // of `SceneGradeMode` keys its work on that change tick.
        app.world_mut().resource_mut::<RenderingSettings>().hud = !RenderingSettings::default().hud;
        app.update();
        assert!(
            !app.world()
                .resource::<SettingsApplyProbe>()
                .grade_mode_changed,
            "an unchanged scene grade was replaced after an unrelated preference change"
        );
    }

    /// A render target that appears later runs the apply again. That run must not dirty
    /// `RenderingSettings` when the shown grade is already current, and a replacement
    /// primary window must take the configured mode and size, which its defaults differ from.
    #[test]
    fn a_new_target_reapplies_without_dirtying_settings_and_sizes_a_new_window() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings {
                window_mode: 1,
                window_size: 3,
                ..RenderingSettings::default()
            })
            .insert_resource(SettingsFile::session_only())
            .insert_resource(crate::color_pipeline::SceneGradeMode::SnapshotSrgb)
            .insert_resource(crate::color_pipeline::SceneVisualPreset::CoolGray)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .insert_resource(crate::sky::SkyPalette::default())
            .init_resource::<SettingsApplyProbe>()
            .add_systems(Update, apply_render_settings)
            .add_systems(Last, probe_settings_apply);
        app.update();
        app.update();
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.update();
        assert!(
            !app.world()
                .resource::<SettingsApplyProbe>()
                .settings_changed,
            "a new target dirtied RenderingSettings although no shown value changed"
        );
        let window = app.world().get::<Window>(window).unwrap();
        assert_eq!(
            window.mode,
            WindowMode::BorderlessFullscreen(MonitorSelection::Current)
        );
        let (width, height) = WINDOW_SIZES[3];
        assert_eq!(
            (window.resolution.width(), window.resolution.height()),
            (width as f32, height as f32)
        );
    }

    #[test]
    fn unedited_grade_rows_show_source_values_without_claiming_user_override() {
        let mut settings = RenderingSettings::default();
        let grade = crate::color_pipeline::SkyrimCinematicGrade {
            saturation: 1.91,
            brightness: 1.42,
            contrast: 1.37,
            tint_amount: 0.44,
            tint_rgb: [0.73, 0.82, 0.91],
        };
        settings.show_resolved_grade(grade, 0.23);
        let spec = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Grade brightness")
            .unwrap();
        assert_eq!((spec.read)(&settings), f64::from(grade.brightness));
        assert_eq!(spec.value_label(&settings), "1.42");
        assert!(!spec.is_modified(&settings));
        assert_eq!(settings.grade_override_mask, 0);
    }

    #[test]
    fn rejected_record_mode_keeps_last_valid_menu_and_runtime_mode() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings::default())
            .insert_resource(SettingsFile::session_only())
            .insert_resource(crate::color_pipeline::SceneGradeMode::SnapshotSrgb)
            .insert_resource(crate::color_pipeline::SceneVisualPreset::CoolGray)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .add_systems(Update, apply_render_settings);
        app.update();
        app.world_mut()
            .resource_mut::<RenderingSettings>()
            .color_grade = crate::color_pipeline::SceneGradeMode::RecordSrgb.index();
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::color_pipeline::SceneGradeMode>(),
            &crate::color_pipeline::SceneGradeMode::SnapshotSrgb
        );
        assert_eq!(
            app.world().resource::<RenderingSettings>().color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        assert!(
            app.world()
                .resource::<SettingsFile>()
                .status
                .contains("record-srgb")
        );
        assert_eq!(
            *app.world()
                .resource::<crate::color_pipeline::SceneGradeOverrides>(),
            crate::color_pipeline::SceneGradeOverrides::default()
        );
        assert_eq!(
            app.world().resource::<EngineConfig>().color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb
        );
    }

    #[test]
    fn invalid_numeric_override_rolls_back_menu_and_effective_source_state() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings::default())
            .insert_resource(SettingsFile::session_only())
            .insert_resource(crate::color_pipeline::SceneGradeMode::SnapshotSrgb)
            .insert_resource(crate::color_pipeline::SceneVisualPreset::CoolGray)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .add_systems(Update, apply_render_settings);
        app.update();
        let baseline = app.world().resource::<RenderingSettings>().clone();
        app.world_mut()
            .resource_mut::<RenderingSettings>()
            .grade_brightness = f32::NAN;
        app.world_mut()
            .resource_mut::<RenderingSettings>()
            .grade_override_mask |= 1 << 1;
        app.update();
        let settings = app.world().resource::<RenderingSettings>();
        assert_eq!(settings.grade_brightness, baseline.grade_brightness);
        assert_eq!(settings.grade_override_mask, baseline.grade_override_mask);
        assert_eq!(settings.color_grade, baseline.color_grade);
        assert_eq!(
            *app.world()
                .resource::<crate::color_pipeline::SceneGradeOverrides>(),
            crate::color_pipeline::SceneGradeOverrides::default()
        );
        assert!(
            app.world()
                .resource::<SettingsFile>()
                .status
                .contains("finite")
        );
    }

    #[test]
    fn setting_groups_are_contiguous_per_page_so_the_menu_headings_do_not_repeat() {
        for page in [SettingsPage::Graphics, SettingsPage::Streaming] {
            let mut seen: Vec<&str> = Vec::new();
            for spec in SETTING_SPECS.iter().filter(|spec| spec.page == page) {
                if seen.last() != Some(&spec.group) {
                    assert!(!seen.contains(&spec.group), "{} is split", spec.group);
                    seen.push(spec.group);
                }
            }
        }
    }

    #[test]
    fn dependent_settings_report_relevance_and_modification() {
        let budget = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Enabled point-light budget")
            .unwrap();
        let lights = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Point lights")
            .unwrap();
        let mut settings = RenderingSettings::default();
        assert!(!lights.is_modified(&settings));
        assert!(!budget.relevant(&settings));
        lights.adjust(&mut settings, 1);
        assert!(lights.is_modified(&settings));
        assert!(budget.relevant(&settings));
        assert!(!budget.is_modified(&settings));
    }

    #[test]
    fn saved_visual_profile_selects_the_run_and_still_refuses_foreign_flags() {
        use crate::visual_profile::VisualProfile;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            visual_profile: VisualProfile::Skyrim.index(),
            ..default()
        };
        save_settings(&path, &saved).unwrap();

        let mut config = EngineConfig::default();
        prepare_settings_at(&mut config, Some(path.clone()));
        assert_eq!(config.visual_profile, VisualProfile::Skyrim);
        config.check_visual_profile().unwrap();

        // The parser accepted --tone-map under the default profile; the saved one refuses it.
        let mut config = launch(&["--tone-map".into(), "tony".into()]);
        prepare_settings_at(&mut config, Some(path.clone()));
        assert!(matches!(
            config.check_visual_profile(),
            Err(crate::config::ConfigError::ForeignToProfile {
                option: "--tone-map",
                ..
            })
        ));

        let mut config = launch(&["--visual-profile".into(), "mudcrab".into()]);
        let (loaded, _) = prepare_settings_at(&mut config, Some(path));
        assert_eq!(config.visual_profile, VisualProfile::Mudcrab);
        assert_eq!(loaded.visual_profile, VisualProfile::Mudcrab.index());

        let unknown = RenderingSettings {
            visual_profile: 2,
            ..default()
        };
        assert!(unknown.validate().is_err());
    }

    #[test]
    fn saved_preferences_round_trip_and_explicit_flags_win() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            point_lights: true,
            stream_radius: 4,
            unload_radius: 5,
            upload_mib: 8,
            io_threads: 2,
            exposure_ev100: 8.0,
            ..default()
        };
        save_settings(&path, &saved).unwrap();
        let (loaded, _) = prepare_settings_at(&mut EngineConfig::default(), Some(path.clone()));
        assert_eq!(loaded, saved);
        let mut config = launch(&[
            "--stream-radius".into(),
            "1".into(),
            "--max-upload-mib-per-frame".into(),
            "0".into(),
            "--io-threads".into(),
            "0".into(),
        ]);
        let (loaded, _) = prepare_settings_at(&mut config, Some(path));
        assert_eq!((loaded.stream_radius, loaded.unload_radius), (1, 2));
        assert_eq!((loaded.upload_mib, loaded.io_threads), (0, 0));
        assert!(config.lights);
        assert_eq!(loaded.exposure_ev100, 8.0);
        assert_eq!(config.max_upload_bytes_per_frame(), None);
    }

    #[test]
    fn saved_grade_selection_is_applied_before_scene_sources_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            color_grade: crate::color_pipeline::SceneGradeMode::SnapshotLinear.index(),
            visual_preset: 0,
            grade_brightness: 1.7,
            grade_override_mask: 1 << 1,
            ..default()
        };
        save_settings(&path, &saved).unwrap();

        let mut config = EngineConfig::default();
        let (active, _) = prepare_settings_at(&mut config, Some(path));
        assert_eq!(
            config.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotLinear
        );
        assert_eq!(
            config.visual_preset,
            crate::color_pipeline::SceneVisualPreset::Vivid
        );
        assert_eq!(active.color_grade, config.color_grade.index());
        assert_eq!(active.grade_overrides().brightness, Some(1.7));
        let source = crate::color_pipeline::SceneGradeSource::load(&config, false).unwrap();
        let tone = crate::tone_mapping::SceneToneMapSource::load(&config, &source).unwrap();
        let resolved = tone
            .resolve_with_overrides(
                &source,
                config.color_grade,
                config.visual_preset,
                active.grade_overrides(),
            )
            .unwrap();
        assert_eq!(resolved.grade.brightness, 1.7);
        assert_eq!(resolved.post.linear_domain, 1);
    }

    #[test]
    fn explicit_grade_launch_ownership_preserves_only_its_owned_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            color_grade: crate::color_pipeline::SceneGradeMode::Full.index(),
            visual_preset: 1,
            tone_map: 3,
            grade_brightness: 1.8,
            grade_override_mask: 1 << 1,
            ..default()
        };
        save_settings(&path, &saved).unwrap();

        let mut custom_grade = launch(&[
            "--color-grade".into(),
            "snapshot-linear".into(),
            "--visual-preset".into(),
            "vivid".into(),
        ]);
        let (mut active, mut file) = prepare_settings_at(&mut custom_grade, Some(path.clone()));
        assert_eq!(
            active.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotLinear.index()
        );
        assert_eq!(active.visual_preset, 0);
        assert_eq!(active.grade_override_mask, 0);
        active.hud = !active.hud;
        file.save(&active);
        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.color_grade, saved.color_grade);
        assert_eq!(persisted.visual_preset, saved.visual_preset);
        assert_eq!(persisted.grade_override_mask, saved.grade_override_mask);
        assert_eq!(persisted.grade_brightness, saved.grade_brightness);
        assert_eq!(persisted.hud, active.hud);

        let mut visual_only = launch(&["--visual-preset".into(), "vivid".into()]);
        let (active, _) = prepare_settings_at(&mut visual_only, Some(path));
        assert_eq!(active.color_grade, saved.color_grade);
        assert_eq!(active.visual_preset, 0);
        assert_eq!(active.grade_override_mask, saved.grade_override_mask);
    }

    #[test]
    fn source_launch_options_do_not_infer_record_mode_or_apply_stale_tuning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            color_grade: crate::color_pipeline::SceneGradeMode::RecordSrgb.index(),
            grade_brightness: 2.2,
            grade_override_mask: 1 << 1,
            ..default()
        };
        save_settings(&path, &saved).unwrap();

        for args in [
            vec!["--grade-preset".into(), "custom.json".into()],
            vec!["--tone-map".into(), "retail-curve".into()],
            vec!["--tone-map-params".into(), "curve.json".into()],
        ] {
            let mut config = launch(&args);
            let (mut active, mut file) = prepare_settings_at(&mut config, Some(path.clone()));
            assert_eq!(active.color_grade, config.color_grade.index());
            assert_ne!(
                config.color_grade,
                crate::color_pipeline::SceneGradeMode::RecordSrgb
            );
            assert_eq!(active.grade_override_mask, 0);
            active.hud = !active.hud;
            file.save(&active);
            let persisted =
                serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap())
                    .unwrap();
            assert_eq!(persisted.color_grade, saved.color_grade);
            assert_eq!(persisted.grade_override_mask, saved.grade_override_mask);
            assert_eq!(persisted.grade_brightness, saved.grade_brightness);
            assert_eq!(persisted.hud, active.hud);
            save_settings(&path, &saved).unwrap();
        }

        let mut selected_record = launch(&["--image-space".into(), "0x12F88".into()]);
        let (active, _) = prepare_settings_at(&mut selected_record, Some(path));
        assert_eq!(
            selected_record.color_grade,
            crate::color_pipeline::SceneGradeMode::RecordSrgb,
            "an explicit record source enables an already-persisted record mode"
        );
        assert_eq!(active.color_grade, saved.color_grade);
        assert_eq!(active.grade_overrides().brightness, Some(2.2));

        let mut selection_only = launch(&["--image-space".into(), "0x12F88".into()]);
        let (active, _) = prepare_settings_at(&mut selection_only, None);
        assert_eq!(
            selection_only.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb
        );
        assert_eq!(active.color_grade, selection_only.color_grade.index());
    }

    #[test]
    fn saved_record_mode_without_selection_falls_back_and_preserves_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            color_grade: crate::color_pipeline::SceneGradeMode::RecordSrgb.index(),
            point_lights: true,
            hud: false,
            ..default()
        };
        save_settings(&path, &saved).unwrap();
        let mut config = EngineConfig::default();
        let (mut active, mut file) = prepare_settings_at(&mut config, Some(path.clone()));
        assert_eq!(
            active.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        assert_eq!(
            config.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb
        );
        assert!(active.point_lights);
        assert!(!active.hud);
        assert!(file.status.contains("record-srgb"));
        active.hud = true;
        file.save(&active);
        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.color_grade, saved.color_grade);
        assert!(persisted.point_lights);
        assert!(persisted.hud);
        let (reloaded, file) =
            prepare_settings_at(&mut EngineConfig::default(), Some(path.clone()));
        assert_eq!(
            reloaded.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        assert!(file.preserve_unavailable_record_mode);
    }

    #[test]
    fn choosing_a_valid_grade_replaces_unavailable_saved_record_mode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut app = unavailable_record_app(&path);
        adjust_grade_mode(&mut app, -1);
        assert_eq!(
            app.world().resource::<RenderingSettings>().color_grade,
            crate::color_pipeline::SceneGradeMode::Full.index()
        );
        save_app_settings(&mut app);

        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.color_grade,
            crate::color_pipeline::SceneGradeMode::Full.index()
        );
        let (reloaded, file) = prepare_settings_at(&mut EngineConfig::default(), Some(path));
        assert_eq!(reloaded.color_grade, persisted.color_grade);
        assert!(!file.preserve_unavailable_record_mode);
    }

    #[test]
    fn returning_to_fallback_grade_before_save_repairs_unavailable_record_mode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut app = unavailable_record_app(&path);
        adjust_grade_mode(&mut app, -1);
        adjust_grade_mode(&mut app, 1);
        assert_eq!(
            app.world().resource::<RenderingSettings>().color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        save_app_settings(&mut app);

        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        let (reloaded, file) = prepare_settings_at(&mut EngineConfig::default(), Some(path));
        assert_eq!(reloaded.color_grade, persisted.color_grade);
        assert!(!file.preserve_unavailable_record_mode);
    }

    #[test]
    fn reset_defaults_repairs_unavailable_record_mode_on_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let (mut config, mut settings, mut file) = unavailable_record_fixture(&path);
        file.restore_defaults(&mut settings);
        file.save(&settings);

        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        let (reloaded, file) = prepare_settings_at(&mut config, Some(path));
        assert_eq!(reloaded.color_grade, persisted.color_grade);
        assert!(!file.preserve_unavailable_record_mode);
    }

    #[test]
    fn rejected_record_grade_keeps_unavailable_preference_and_valid_runtime_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut app = unavailable_record_app(&path);
        adjust_grade_mode(&mut app, 2);
        assert_eq!(
            app.world().resource::<RenderingSettings>().color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        assert_eq!(
            *app.world()
                .resource::<crate::color_pipeline::SceneGradeMode>(),
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb
        );
        save_app_settings(&mut app);
        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.color_grade,
            crate::color_pipeline::SceneGradeMode::RecordSrgb.index()
        );
    }

    #[test]
    fn explicit_console_selection_of_fallback_repairs_unavailable_record_mode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let (config, settings, file) = unavailable_record_fixture(&path);
        let mut app = App::new();
        app.insert_resource(config.color_grade)
            .insert_resource(config.visual_preset)
            .insert_resource(settings)
            .insert_resource(file)
            .insert_resource(crate::color_pipeline::SceneGradeSource::default())
            .insert_resource(crate::tone_mapping::SceneToneMapSource::default())
            .insert_resource(crate::color_pipeline::SceneGradeOverrides::default())
            .init_resource::<crate::console::ConsoleState>()
            .add_console_command(crate::color_pipeline::scene_grade_command());

        crate::console::execute_line(app.world_mut(), "SceneGrade snapshot-srgb");
        save_app_settings(&mut app);

        let persisted =
            serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted.color_grade,
            crate::color_pipeline::SceneGradeMode::SnapshotSrgb.index()
        );
        let (reloaded, _) = prepare_settings_at(&mut EngineConfig::default(), Some(path));
        assert_eq!(reloaded.color_grade, persisted.color_grade);
    }

    #[test]
    fn old_settings_schema_defaults_new_grade_fields_without_overriding_custom_values() {
        let old = r#"{"version":1,"color_grade":2,"visual_preset":0,"grade_brightness":2.75}"#;
        let settings = serde_json::from_str::<RenderingSettings>(old).unwrap();
        settings.validate().unwrap();
        assert_eq!(settings.grade_override_mask, 0);
        assert_eq!(settings.grade_brightness, 2.75);
        assert_eq!(settings.grade_overrides().brightness, None);
    }

    #[test]
    fn invalid_grade_mask_and_explicit_value_leave_saved_file_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        save_settings(&path, &RenderingSettings::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        for invalid in [
            RenderingSettings {
                grade_override_mask: 1 << 12,
                ..default()
            },
            RenderingSettings {
                grade_brightness: f32::INFINITY,
                grade_override_mask: 1 << 1,
                ..default()
            },
        ] {
            let mut file = SettingsFile {
                path: Some(path.clone()),
                status: String::new(),
                stored: RenderingSettings::default(),
                launch_overrides: Vec::new(),
                preserve_unavailable_record_mode: false,
                unavailable_record_mode_replaced: false,
            };
            file.save(&invalid);
            assert!(file.status.contains("Could not save"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn preference_loading_excludes_automated_runs() {
        for args in [
            vec!["--headless".into()],
            vec!["--hidden-window".into()],
            vec!["--benchmark-only".into()],
            vec!["--benchmark-frames".into(), "2".into()],
            vec!["--acceptance-screenshot".into(), "capture.png".into()],
            vec!["--shots".into(), "shots.json".into()],
            vec!["--matched-route".into(), "route.json".into()],
            vec!["--streaming-fixture".into()],
            vec!["--auto-fly-speed".into(), "1".into()],
        ] {
            assert!(!launch(&args).interactive_world_physics(), "{args:?}");
        }
    }

    #[test]
    fn only_valid_explicit_ini_keys_override_saved_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let ini = directory.path().join("SkyrimPrefs.ini");
        let saved = RenderingSettings {
            stream_radius: 4,
            unload_radius: 5,
            lod_multiplier: 2.0,
            lod_level0_distance: 45_000.0,
            ..default()
        };
        save_settings(&path, &saved).unwrap();
        std::fs::write(
            &ini,
            "[TerrainManager]\nfSplitDistanceMult=3\nfBlockLevel0Distance=bad\n",
        )
        .unwrap();
        let mut config = launch(&["--ini".into(), ini.display().to_string()]);
        let (loaded, _) = prepare_settings_at(&mut config, Some(path));
        assert_eq!(loaded.lod_multiplier, 3.0);
        assert_eq!(loaded.lod_level0_distance, 45_000.0);
        assert_eq!((loaded.stream_radius, loaded.unload_radius), (4, 5));
    }

    #[test]
    fn invalid_saved_choices_fall_back_without_losing_launch_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let invalid = RenderingSettings {
            tone_map: 999,
            ..default()
        };
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let mut config = EngineConfig {
            lights: true,
            stream_radius: 1,
            unload_radius: 2,
            ..default()
        };
        let (loaded, file) = prepare_settings_at(&mut config, Some(path));
        assert!(loaded.point_lights);
        assert_eq!(loaded.stream_radius, 1);
        assert_eq!(loaded.tone_map, 6);
        assert!(file.status.contains("ignored"));
    }

    /// Explicit launch values outside the menu's ranges stay in the running settings, while the file
    /// keeps the values saved before the run. The persisted bytes validate, and an unrelated change
    /// still saves.
    #[test]
    fn save_keeps_launch_owned_fields_out_of_the_file_and_the_running_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let saved = RenderingSettings {
            stream_radius: 3,
            unload_radius: 4,
            commit_ms: 2.5,
            ..RenderingSettings::default()
        };
        save_settings(&path, &saved).unwrap();
        let launch_args = ["--stream-radius", "20", "--max-commit-ms", "250"].map(String::from);

        let (mut active, mut file) =
            prepare_settings_at(&mut launch(&launch_args), Some(path.clone()));
        assert_eq!(
            active.stream_radius, 20,
            "the launch value is the running value"
        );
        assert_eq!(
            active.commit_ms, 250.0,
            "the launch budget is the running value"
        );
        active.hud = !active.hud;
        let running = active.clone();

        file.save(&active);
        assert!(file.status.starts_with("Settings saved"), "{}", file.status);
        assert_eq!(
            active, running,
            "saving leaves the running settings unchanged"
        );
        let written = serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap())
            .expect("the persisted bytes parse");
        written.validate().expect("the persisted bytes validate");
        assert_eq!(
            (
                written.stream_radius,
                written.unload_radius,
                written.commit_ms
            ),
            (3, 4, 2.5),
            "the file keeps the values saved before the run"
        );
        assert_eq!(written.hud, running.hud, "the unrelated change is saved");

        let (reloaded, _) = prepare_settings_at(&mut EngineConfig::default(), Some(path.clone()));
        assert_eq!(reloaded.hud, running.hud, "the unrelated change reloads");
        assert_eq!((reloaded.stream_radius, reloaded.commit_ms), (3, 2.5));

        let (overridden, _) = prepare_settings_at(&mut launch(&launch_args), Some(path));
        assert_eq!(
            (overridden.stream_radius, overridden.commit_ms),
            (20, 250.0),
            "the explicit launch values still win on reload"
        );
    }

    /// Without a valid file, the saved baseline for launch-owned fields is the menu default, and the
    /// bytes written still validate.
    #[test]
    fn save_without_a_file_writes_valid_defaults_for_launch_owned_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let launch_args = ["--stream-radius", "20", "--max-commit-ms", "250"].map(String::from);
        let (mut active, mut file) =
            prepare_settings_at(&mut launch(&launch_args), Some(path.clone()));
        active.hud = !active.hud;

        file.save(&active);
        assert!(file.status.starts_with("Settings saved"), "{}", file.status);
        let written = serde_json::from_slice::<RenderingSettings>(&std::fs::read(&path).unwrap())
            .expect("the persisted bytes parse");
        written.validate().expect("the persisted bytes validate");
        let defaults = RenderingSettings::default();
        assert_eq!(
            (
                written.stream_radius,
                written.unload_radius,
                written.commit_ms
            ),
            (
                defaults.stream_radius,
                defaults.unload_radius,
                defaults.commit_ms
            ),
            "a launch value is never written when there was no saved baseline"
        );
        assert_eq!(written.hud, active.hud);
    }

    #[test]
    fn rejected_save_keeps_previous_file_and_reports_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        save_settings(&path, &RenderingSettings::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let invalid = RenderingSettings {
            exposure_ev100: f32::NAN,
            ..default()
        };
        let mut file = SettingsFile {
            path: Some(path.clone()),
            status: String::new(),
            stored: RenderingSettings::default(),
            launch_overrides: Vec::new(),
            preserve_unavailable_record_mode: false,
            unavailable_record_mode_replaced: false,
        };
        file.save(&invalid);
        assert!(file.status.contains("Could not save"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let wrong_version = RenderingSettings {
            version: 2,
            ..RenderingSettings::default()
        };
        file.save(&wrong_version);
        assert!(
            file.status.contains("unsupported settings version"),
            "{}",
            file.status
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn renderer_changes_reach_camera_and_upload_budget_but_io_waits_for_restart() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings::default())
            .insert_resource(RenderAssetBytesPerFrame::default())
            .add_systems(Update, apply_render_settings);
        let camera = app
            .world_mut()
            .spawn((
                Camera3d::default(),
                crate::color_pipeline::SceneColorPipeline::default(),
                StreamingCamera,
                Msaa::Off,
                DepthPrepass,
                OcclusionCulling,
            ))
            .id();
        app.update();
        {
            let mut settings = app.world_mut().resource_mut::<RenderingSettings>();
            settings.depth_prepass = false;
            settings.occlusion_culling = false;
            settings.indirect_drawing = false;
            settings.cpu_frustum_culling = false;
            settings.exposure_ev100 = 8.5;
            settings.fov_degrees = 70.0;
            settings.upload_mib = 0;
            settings.io_threads = 4;
        }
        app.update();
        assert!(app.world().get::<DepthPrepass>(camera).is_none());
        assert!(app.world().get::<OcclusionCulling>(camera).is_none());
        assert!(app.world().get::<NoCpuCulling>(camera).is_some());
        assert!(app.world().get::<NoIndirectDrawing>(camera).is_some());
        assert_eq!(app.world().get::<Exposure>(camera).unwrap().ev100, 8.5);
        let Projection::Perspective(projection) = app.world().get::<Projection>(camera).unwrap()
        else {
            panic!("expected perspective");
        };
        assert!((projection.fov.to_degrees() - 70.0).abs() < 0.001);
        assert_eq!(
            app.world().resource::<RenderAssetBytesPerFrame>().max_bytes,
            None
        );
        assert_eq!(app.world().resource::<EngineConfig>().io_threads, 0);
    }

    #[test]
    fn menu_changes_enforce_culling_dependencies_and_unload_coverage() {
        let mut settings = RenderingSettings::default();
        let depth = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Depth prepass")
            .unwrap();
        let occlusion = SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "GPU occlusion culling")
            .unwrap();
        depth.adjust(&mut settings, 1);
        assert!(!settings.depth_prepass && !settings.occlusion_culling);
        occlusion.adjust(&mut settings, 1);
        assert!(settings.depth_prepass && settings.occlusion_culling && settings.indirect_drawing);
        settings.unload_radius = settings.stream_radius;
        SETTING_SPECS
            .iter()
            .find(|spec| spec.label == "Full-detail radius (cells)")
            .unwrap()
            .adjust(&mut settings, 1);
        assert!(settings.unload_radius > settings.stream_radius);
        settings.validate().unwrap();
    }
}
