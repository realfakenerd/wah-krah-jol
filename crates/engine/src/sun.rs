//! Clock/climate driven directional light geometry; shading and the sky palette stay separate.
//!
//! Sun path follows spud (MIT, chasmlol/spud), commit e5f2de4,
//! src/engine/weather.cpp::sun_parameter / Sky::evaluate. Written independently in Rust.

use crate::{config::EngineConfig, streaming::ActiveSpace, world::environment::EnvironmentCatalog};
use bevy::prelude::*;

/// Settings in GMST order; the same names may be explicitly overridden in `[Sky]` INI files.
pub const SUN_KEYS: [&str; 5] = [
    "fSunXExtreme",
    "fSunYExtreme",
    "fSunZExtreme",
    "fSunDirXExtreme",
    "fSunAlphaTransTime",
];

/// X position extreme, Y position extreme, Z lighting extreme, X lighting extreme, fade hours.
/// These are spud's fallback values; winning GMST records replace them before INI overrides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunSettings(pub [f32; 5]);

impl Default for SunSettings {
    /// Fallback for packs without the optional game settings.
    fn default() -> Self {
        Self([400.0, 25.0, -100.0, 400.0, 2.0])
    }
}

impl SunSettings {
    /// Validate each setting before accepting it from optional records or INI input.
    /// Bounds avoid zero division/degenerate vectors and leave ample room for modded paths.
    pub fn valid(index: usize, value: f32) -> bool {
        value.is_finite()
            && match index {
                0 | 2 => (0.001..=1_000_000.0).contains(&value.abs()),
                1 | 3 => value.abs() <= 1_000_000.0,
                4 => (0.0..24.0).contains(&value),
                _ => false,
            }
    }

    /// Apply validated optional overrides without replacing other authored values.
    pub fn with_overrides(mut self, values: [Option<f32>; 5]) -> Self {
        for (index, value) in values.into_iter().enumerate() {
            if let Some(value) = value.filter(|v| Self::valid(index, *v)) {
                self.0[index] = value;
            }
        }
        self
    }
}

/// Climate TNAM boundaries in ten-minute units (not degrees or hours).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunTiming(pub [u8; 4]);

impl Default for SunTiming {
    /// A symmetric 06:00/18:00 day for worlds without usable TNAM.
    fn default() -> Self {
        Self([36, 36, 108, 108])
    }
}

impl SunTiming {
    /// Accept ordered same-day transitions; malformed climates use the fallback and warn at load.
    pub fn valid(self) -> bool {
        let [a, b, c, d] = self.0;
        a <= b
            && b < c
            && c <= d
            && d <= 144
            && u16::from(c) + u16::from(d) - u16::from(a) - u16::from(b) < 288
    }
}

/// Unit vector toward the sun in engine +Y-up space; light rays travel in the opposite direction.
/// The visible disc follows a different path. At night this repeats the return sweep; the existing
/// constant light intensity/palette is retained rather than adding moon lighting or a night grade.
pub fn sun_direction(hour: f64, timing: SunTiming, settings: SunSettings) -> Vec3 {
    let fallback = SunSettings::default();
    let values = std::array::from_fn(|i| {
        Some(if SunSettings::valid(i, settings.0[i]) {
            settings.0[i]
        } else {
            fallback.0[i]
        })
    });
    let [x, y, z, dir_x, transition] = fallback.with_overrides(values).0.map(f64::from);
    let timing = if timing.valid() {
        timing
    } else {
        SunTiming::default()
    };
    let [rise_begin, rise_end, set_begin, set_end] = timing.0.map(|v| f64::from(v) / 6.0);
    let mut start = (rise_begin + rise_end - transition) * 0.5;
    let mut length = (set_begin + set_end + transition) * 0.5 - start;
    // An extreme transition can consume the night. Retain authored midpoints, omit that fade.
    if !(0.0..24.0).contains(&length) || length == 0.0 {
        start = (rise_begin + rise_end) * 0.5;
        length = (set_begin + set_end) * 0.5 - start;
    }
    let hour = if hour.is_finite() {
        hour.rem_euclid(24.0)
    } else {
        12.0
    };
    let elapsed = (hour - start).rem_euclid(24.0);
    let parameter = if elapsed < length {
        1.0 - 2.0 * elapsed / length
    } else {
        2.0 * (elapsed - length) / (24.0 - length) - 1.0
    };
    let direction = bevy::math::DVec3::new(parameter * dir_x, y * dir_x / x, -z).normalize();
    Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        direction.as_vec3().to_array(),
    ))
}

/// Game hour and game seconds per real second. Capture/benchmark runs freeze this at launch.
#[derive(Resource, Debug, Clone, Copy)]
pub struct GameClock {
    pub hour: f64,
    pub timescale: f64,
}

impl GameClock {
    /// Start at the requested hour; automated captures must not drift while streaming settles.
    pub fn from_config(config: &EngineConfig) -> Self {
        let frozen = config.shots.is_some()
            || config.acceptance_screenshot.is_some()
            || config.is_benchmark_run()
            || config.matched_route.is_some();
        Self {
            hour: config.game_hour,
            timescale: if frozen { 0.0 } else { config.timescale },
        }
    }

    /// Advance smoothly using double precision, including a clean midnight wrap.
    pub fn advance(&mut self, seconds: f64) {
        self.hour = (self.hour + seconds * self.timescale / 3600.0).rem_euclid(24.0);
    }
}

/// Marks the world's sun so fixture and other directional lights are never rotated accidentally.
#[derive(Component)]
pub struct SunLight;

/// Resolve the active world's optional timing and winning settings, then apply explicit INI keys.
pub fn world_sun_direction(
    config: &EngineConfig,
    hour: f64,
    catalog: Option<&EnvironmentCatalog>,
    worldspace: u32,
) -> Vec3 {
    let timing = catalog
        .and_then(|c| {
            c.worldspace_climate(worldspace)
                .and_then(|id| c.sun_timing(id))
        })
        .unwrap_or_default();
    let settings = catalog
        .map_or_else(SunSettings::default, |c| c.sun_settings())
        .with_overrides(config.sun_settings_overrides);
    sun_direction(hour, timing, settings)
}

/// Advance before rendering; no allocations or database access occur per frame.
pub(crate) fn update_sun(
    time: Res<Time>,
    mut clock: ResMut<GameClock>,
    config: Res<EngineConfig>,
    catalog: Option<Res<EnvironmentCatalog>>,
    space: Option<Res<ActiveSpace>>,
    mut sun: Query<&mut Transform, With<SunLight>>,
) {
    clock.advance(time.delta_secs_f64());
    if config.sun_position.is_some() {
        return;
    }
    let worldspace = space.as_deref().map_or(config.worldspace_id, |s| {
        s.exterior_worldspace(config.worldspace_id)
    });
    let direction = world_sun_direction(&config, clock.hour, catalog.as_deref(), worldspace);
    let rotation = Quat::from_rotation_arc(Vec3::Z, direction);
    for mut transform in &mut sun {
        if transform.rotation != rotation {
            transform.rotation = rotation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the scheduled clock advance rather than only the standalone arithmetic helper.
    #[test]
    fn runtime_driver_advances_hour_across_midnight() {
        let mut app = App::new();
        let mut time = Time::<()>::default();
        time.advance_by(std::time::Duration::from_secs_f64(3.6));
        app.insert_resource(time)
            .insert_resource(EngineConfig::default())
            .insert_resource(GameClock {
                hour: 23.99,
                timescale: 20.0,
            })
            .add_systems(Update, update_sun);
        let light = app.world_mut().spawn((SunLight, Transform::default())).id();
        app.update();
        assert!((app.world().resource::<GameClock>().hour - 0.01).abs() < 1e-10);
        let direction = app.world().get::<Transform>(light).unwrap().back();
        assert!(direction.is_finite());
    }

    /// The actual scheduled system rotates only the marked sun and respects explicit overrides.
    #[test]
    fn runtime_driver_tracks_clock_and_preserves_override() {
        let mut app = App::new();
        app.insert_resource(Time::<()>::default())
            .insert_resource(EngineConfig::default())
            .insert_resource(GameClock {
                hour: 9.0,
                timescale: 0.0,
            })
            .add_systems(Update, update_sun);
        let light = app.world_mut().spawn((SunLight, Transform::default())).id();
        let other = app.world_mut().spawn(Transform::default()).id();
        app.update();
        let at_nine = app.world().get::<Transform>(light).unwrap().rotation;
        app.world_mut().resource_mut::<GameClock>().hour = 17.0;
        app.update();
        let at_seventeen = app.world().get::<Transform>(light).unwrap().rotation;
        assert!((at_nine * Vec3::Z).distance(at_seventeen * Vec3::Z) > 1.0);
        assert_eq!(
            app.world().get::<Transform>(other).unwrap().rotation,
            Quat::IDENTITY
        );
        app.world_mut().resource_mut::<EngineConfig>().sun_position = Some((61.0, 74.0));
        app.world_mut().resource_mut::<GameClock>().hour = 12.0;
        app.update();
        assert_eq!(
            app.world().get::<Transform>(light).unwrap().rotation,
            at_seventeen
        );
    }

    /// Independent captured direction also reproduced from spud's actual imported settings.
    #[test]
    fn noon_matches_independent_creation_vector_and_axis_mapping() {
        let direction = sun_direction(
            12.0,
            SunTiming([33, 60, 96, 123]),
            SunSettings([400.0, 40.0, -150.0, 200.0, 0.5]),
        );
        assert!(direction.distance(Vec3::new(0.233_646_62, 0.963_792_3, -0.128_505_65)) < 1e-6);
    }

    /// An asymmetric synthetic climate/settings pair detects sign and midpoint mistakes.
    #[test]
    fn synthetic_day_sweep_has_independent_endpoints_and_midpoint() {
        let timing = SunTiming([18, 42, 108, 132]); // midpoints 5 and 20; expanded to 4 and 21
        let settings = SunSettings([200.0, -60.0, -120.0, 100.0, 2.0]);
        for (hour, expected) in [
            (4.0, Vec3::new(100.0, 120.0, 30.0)),
            (12.5, Vec3::new(0.0, 120.0, 30.0)),
            (21.0, Vec3::new(-100.0, 120.0, 30.0)),
            (0.5, Vec3::new(0.0, 120.0, 30.0)),
        ] {
            assert!(sun_direction(hour, timing, settings).distance(expected.normalize()) < 1e-6);
        }
    }

    /// Midnight and both day-window boundaries must be continuous; arbitrary hours wrap.
    #[test]
    fn path_is_continuous_and_periodic() {
        for timing in [
            SunTiming::default(),
            SunTiming([0, 6, 132, 143]),
            SunTiming([6, 12, 138, 144]),
        ] {
            for hour in [0.0, 5.0, 6.0, 12.0, 19.0, 23.999] {
                let a = sun_direction(hour, timing, SunSettings::default());
                assert!(
                    a.distance(sun_direction(hour + 24.0, timing, SunSettings::default())) < 1e-6
                );
                assert!(
                    a.distance(sun_direction(
                        hour + 0.00001,
                        timing,
                        SunSettings::default()
                    )) < 1e-4
                );
                assert!((a.length() - 1.0).abs() < 1e-6);
            }
        }
    }

    /// Malformed optional input never produces NaN or a degenerate rotation.
    #[test]
    fn invalid_inputs_use_finite_fallbacks() {
        for hour in [f64::NAN, f64::INFINITY, -23.0] {
            for timing in [SunTiming([255, 0, 0, 255]), SunTiming([0, 1, 140, 143])] {
                assert!(
                    sun_direction(
                        hour,
                        timing,
                        SunSettings([0.0, f32::NAN, 0.0, f32::MAX, 23.9])
                    )
                    .is_normalized()
                );
            }
        }
    }

    /// A fractional tick survives midnight and captures cannot advance during settling.
    #[test]
    fn clock_advances_and_capture_runs_freeze() {
        let mut clock = GameClock {
            hour: 23.99,
            timescale: 20.0,
        };
        clock.advance(3.6);
        assert!((clock.hour - 0.01).abs() < 1e-10);
        let config = EngineConfig {
            shots: Some("synthetic.json".into()),
            game_hour: 9.25,
            ..Default::default()
        };
        let mut clock = GameClock::from_config(&config);
        clock.advance(1234.0);
        assert_eq!(clock.hour, 9.25);
    }
}
