//! The WALK view moves smoothly between 60 Hz physics ticks at any render rate.
//!
//! Before view interpolation the camera followed the body only on frames where a tick had run.
//! At 120, 144 and 240 Hz that repeated the camera position on 50%, 59% and 75% of frames (see the
//! controller-feel measurements, `feel_measure.rs`). These tests pin the replacement behaviour.

use super::{headless, *};

const SPEED: f32 = 370.0;
const START: Vec3 = Vec3::new(-100.0, 200.0, 900.0);

#[derive(Clone, Copy, Debug)]
struct Sample {
    eye: Vec3,
    body: Vec3,
}

fn eye_position(app: &mut App) -> Vec3 {
    app.world_mut()
        .query_filtered::<&Transform, With<StreamingCamera>>()
        .single(app.world())
        .expect("fixture camera")
        .translation
}

fn set_render_hz(app: &mut App, hz: f64) {
    app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / hz),
    ));
}

/// A settled body at running speed along -Z, ready to record frames at `hz`.
fn running_app(hz: f64) -> App {
    let mut app = headless::fixture_app();
    headless::place_player(&mut app, START);
    for _ in 0..150 {
        app.update();
    }
    app.insert_resource(WalkIntent {
        wish_dir: Vec3::NEG_Z,
        target_speed: SPEED,
        jump_pressed: false,
    });
    set_render_hz(&mut app, hz);
    // One second of warm-up: acceleration to run speed finishes in 13 ticks.
    for _ in 0..hz as usize {
        app.update();
    }
    app
}

fn record(app: &mut App, frames: usize) -> Vec<Sample> {
    (0..frames)
        .map(|_| {
            app.update();
            Sample {
                eye: eye_position(app),
                body: headless::player_pose(app).0,
            }
        })
        .collect()
}

fn steps(samples: &[Sample]) -> Vec<f32> {
    samples
        .windows(2)
        .map(|pair| (pair[1].eye - pair[0].eye).length())
        .collect()
}

#[test]
fn view_moves_smoothly_at_high_render_rates() {
    for hz in [120.0_f64, 144.0, 240.0] {
        let mut app = running_app(hz);
        let samples = record(&mut app, hz as usize);
        let steps = steps(&samples);
        let ideal = SPEED / hz as f32;
        let mean = steps.iter().sum::<f32>() / steps.len() as f32;
        let variance = steps.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / steps.len() as f32;
        let repeated = steps.iter().filter(|s| **s < 1.0e-4).count() as f32 / steps.len() as f32;
        let worst = steps.iter().copied().fold(0.0_f32, f32::max);
        assert!(repeated <= 0.02, "{hz} Hz: {repeated} of frames repeated");
        assert!(
            variance.sqrt() / mean <= 0.15,
            "{hz} Hz: step CV {}",
            variance.sqrt() / mean
        );
        assert!(
            worst <= ideal * 1.3,
            "{hz} Hz: worst step {worst} against ideal {ideal}"
        );
        assert!(
            (mean - ideal).abs() <= ideal * 0.05,
            "{hz} Hz: mean step {mean} against ideal {ideal}"
        );
    }
}

#[test]
fn view_at_tick_rate_and_below_keeps_even_steps() {
    // Matching and slower render rates must not regress: every frame runs one or more whole ticks.
    for hz in [30.0_f64, 60.0] {
        let mut app = running_app(hz);
        let samples = record(&mut app, hz as usize);
        let steps = steps(&samples);
        let ideal = SPEED / hz as f32;
        for step in &steps {
            assert!(
                (step - ideal).abs() <= ideal * 0.01,
                "{hz} Hz: step {step} against ideal {ideal}"
            );
        }
    }
}

#[test]
fn view_lags_the_latest_tick_by_at_most_one_tick() {
    // The authoritative body is the latest tick. The shown eye lies between the previous and the
    // latest tick, so it trails by 0 to one tick of travel and never leads.
    for hz in [90.0_f64, 144.0, 240.0] {
        let mut app = running_app(hz);
        let tick_distance = SPEED * PHYSICS_TIMESTEP;
        let eye_height = app.world().resource::<MovementTuning>().eye_height;
        for sample in record(&mut app, hz as usize) {
            let lag = sample.eye.z - sample.body.z;
            assert!(
                lag >= -1.0e-3 && lag <= tick_distance + 0.05,
                "{hz} Hz: view trails the body by {lag}, one tick is {tick_distance}"
            );
            // The body bobs by up to a fraction of a unit per tick on flat ground (issue #590).
            assert!((sample.eye.y - sample.body.y - eye_height).abs() < 0.5);
            assert!((sample.eye.x - sample.body.x).abs() < 0.05);
        }
    }
}

#[test]
fn external_body_moves_snap_the_view_without_a_sweep() {
    // Teleports, door arrivals and origin rebases move the body outside the fixed tick. The view
    // must follow at once and must not blend from the old position.
    let hz = 120.0;
    let mut app = running_app(hz);
    let ideal = SPEED / hz as f32;
    let eye_height = app.world().resource::<MovementTuning>().eye_height;
    record(&mut app, 7);
    let mut query = app
        .world_mut()
        .query_filtered::<&mut Transform, With<PlayerBody>>();
    query.single_mut(app.world_mut()).unwrap().translation += Vec3::new(-800.0, 0.0, 0.0);
    let after = record(&mut app, 10);
    for pair in after.windows(2) {
        let step = (pair[1].eye - pair[0].eye).length();
        assert!(step <= ideal * 1.3, "swept {step} after an external move");
    }
    for sample in &after {
        assert!(
            sample.eye.x < -700.0 && (sample.eye.y - sample.body.y - eye_height).abs() < 0.5,
            "view did not follow the external move: {sample:?}"
        );
    }
}

#[test]
fn view_rotation_stays_immediate() {
    // Only the position is interpolated. Look input reaches the camera on the frame it arrives.
    let mut app = running_app(144.0);
    app.world_mut().resource_mut::<LookIntent>().yaw = 0.7;
    app.world_mut().resource_mut::<LookIntent>().pitch = -0.2;
    app.update();
    let rotation = app
        .world_mut()
        .query_filtered::<&Transform, With<StreamingCamera>>()
        .single(app.world())
        .unwrap()
        .rotation;
    let (_, pitch, _) = rotation.to_euler(EulerRot::YXZ);
    // Pitch always follows the look intent; body yaw needs a captured cursor.
    assert!((pitch + 0.2).abs() < 1.0e-4, "pitch {pitch}");
}

#[test]
fn authoritative_eye_is_the_latest_tick_not_the_blend() {
    let tuning = MovementTuning::default();
    let mut pose = WalkViewPose::default();
    assert_eq!(pose.authoritative_eye(&tuning), None, "no tick yet");
    pose.snap(Vec3::new(1.0, 2.0, 3.0));
    pose.previous = Vec3::new(1.0, 2.0, 13.0);
    assert_eq!(
        pose.authoritative_eye(&tuning),
        Some(Vec3::new(1.0, 2.0 + tuning.eye_height, 3.0))
    );
    assert_eq!(pose.shown(0.0), Vec3::new(1.0, 2.0, 13.0));
    assert_eq!(pose.shown(1.0), Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(pose.shown(0.5), Vec3::new(1.0, 2.0, 8.0));
    assert!(pose.follows(Vec3::new(1.0, 2.0, 3.0)));
    assert!(!pose.follows(Vec3::new(1.0, 2.0, 3.5)));
}
