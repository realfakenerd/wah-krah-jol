//! Controller-feel measurements for milestone B4 (physics and movement).
//!
//! These are `measure` tests, not regression tests. Each one drives the production fixed-step WALK
//! controller headless and prints what Mudcrab does today as `FEEL | scenario | metric | value |
//! unit` rows. They never assert a feel number, because the point is to record differences against
//! the spud audit, not to pin current behaviour. They assert only that the scenario ran (the player
//! settled, the flight finished, every value is finite).
//!
//! Run (ignored by default so CI does not pay for them):
//!
//! ```text
//! python3 scripts/workspace.py cargo test --locked --profile quick -p engine --lib \
//!     feel_measure -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Set `MUDCRAB_FEEL_OUT=/path/table.txt` to append the rows to a file as well.
//!
//! Spud (chasmlol/spud, MIT) is a map only. Nothing here copies its code. The scenarios follow the
//! ten differences of the re-audit, section 3.5.

use super::{headless, *};
use std::io::Write as _;

fn row(scenario: &str, metric: &str, value: f64, unit: &str) {
    assert!(value.is_finite(), "{scenario}/{metric} is not finite");
    let line = format!("FEEL | {scenario} | {metric} | {value:.4} | {unit}");
    eprintln!("{line}");
    if let Some(path) = std::env::var_os("MUDCRAB_FEEL_OUT")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[derive(Clone, Copy, Debug)]
struct TickRow {
    body: Vec3,
    vertical_velocity: f32,
    horizontal_velocity: Vec3,
    /// Rapier's support result after this tick's move.
    supported: bool,
    jump_active: bool,
}

#[derive(Resource, Default)]
struct TickLog(Vec<TickRow>);

fn log_tick(
    mode: Res<MoveMode>,
    mut log: ResMut<TickLog>,
    player: Query<
        (
            &Transform,
            &WalkState,
            Option<&KinematicCharacterControllerOutput>,
        ),
        With<PlayerBody>,
    >,
) {
    if *mode != MoveMode::Walk {
        return;
    }
    let Ok((body, state, output)) = player.single() else {
        return;
    };
    log.0.push(TickRow {
        body: body.translation,
        vertical_velocity: state.vertical_velocity,
        horizontal_velocity: state.horizontal_velocity,
        supported: output.is_some_and(|o| o.grounded),
        jump_active: state.jump_active,
    });
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    /// Fixed ticks that had run when this render frame finished.
    ticks: usize,
    body: Vec3,
    eye: Vec3,
}

fn app() -> App {
    headless::fixture_app_with(|app| {
        app.init_resource::<TickLog>()
            .add_systems(FixedUpdate, log_tick.after(PhysicsSet::Writeback));
    })
}

fn set_render_hz(app: &mut App, hz: f64) {
    app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / hz),
    ));
}

fn fixed_box(app: &mut App, center: Vec3, half_extents: Vec3) {
    app.world_mut().spawn((
        RigidBody::Fixed,
        Collider::cuboid(half_extents.x, half_extents.y, half_extents.z),
        world_collision_groups(),
        Transform::from_translation(center),
    ));
}

fn snapshot(app: &mut App) -> Frame {
    let body = headless::player_pose(app).0;
    let eye = app
        .world_mut()
        .query_filtered::<&Transform, With<StreamingCamera>>()
        .single(app.world())
        .expect("fixture camera")
        .translation;
    let ticks = app.world().resource::<TickLog>().0.len();
    Frame { ticks, body, eye }
}

/// Place the body, let it settle on support, then clear the tick log.
fn settle(app: &mut App, position: Vec3) -> Vec3 {
    headless::place_player(app, position);
    for _ in 0..150 {
        app.update();
    }
    let (pose, grounded) = headless::player_pose(app);
    assert!(grounded, "player did not settle: {pose:?}");
    app.world_mut().resource_mut::<TickLog>().0.clear();
    pose
}

fn intent(app: &mut App, wish_dir: Vec3, target_speed: f32, jump_pressed: bool) {
    app.insert_resource(WalkIntent {
        wish_dir,
        target_speed,
        jump_pressed,
    });
}

/// Run frames until `done` is true, or fail the scenario after `limit` frames.
fn run_until(app: &mut App, limit: usize, frames: &mut Vec<Frame>, done: impl Fn(&Frame) -> bool) {
    for _ in 0..limit {
        app.update();
        let frame = snapshot(app);
        assert!(frame.body.is_finite());
        frames.push(frame);
        if done(&frame) {
            return;
        }
    }
    panic!(
        "scenario did not finish in {limit} frames; last {:?}",
        frames.last()
    );
}

fn ticks(app: &App) -> Vec<TickRow> {
    app.world().resource::<TickLog>().0.clone()
}

/// Production-like tuning: the packaged MOVT `NPC_Default_MT` speeds, not the legacy constants.
fn packaged_tuning() -> MovementTuning {
    MovementTuning {
        record_speeds: Some(DirectionalMovementSpeeds {
            directions: [
                [80.09, 370.0],
                [79.75, 370.0],
                [80.10, 370.0],
                [71.93, 205.25],
            ],
            source: "feel_measure NPC_Default_MT".into(),
        }),
        ..MovementTuning::default()
    }
}

fn mean_abs(values: &[f32]) -> f32 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().map(|v| v.abs()).sum::<f32>() / values.len() as f32
    }
}

fn max_abs(values: &[f32]) -> f32 {
    values.iter().fold(0.0, |m, v| m.max(v.abs()))
}

fn deltas(values: impl Iterator<Item = f32>) -> Vec<f32> {
    let values: Vec<f32> = values.collect();
    values.windows(2).map(|w| w[1] - w[0]).collect()
}

// ---------------------------------------------------------------------------
// Differences 1, 2, 6: a 24-unit stair flight, up and down.
// ---------------------------------------------------------------------------

fn staircase(app: &mut App) {
    // 16 risers of 24 units and 24-unit treads, then a landing at y = 384 (like the #482 tests).
    for index in 0..16 {
        let top = (index + 1) as f32 * 24.0;
        fixed_box(
            app,
            Vec3::new(index as f32 * 24.0 + 12.0, top * 0.5, 850.0),
            Vec3::new(12.0, top * 0.5, 700.0),
        );
    }
    fixed_box(
        app,
        Vec3::new(642.0, 192.0, 850.0),
        Vec3::new(258.0, 192.0, 700.0),
    );
}

fn flight(speed: f32, up: bool) {
    let name = format!("stairs24/{}/{speed}", if up { "up" } else { "down" });
    let mut app = app();
    staircase(&mut app);
    let (start, direction) = if up {
        (Vec3::new(-100.0, 200.0, 600.0), Vec3::X)
    } else {
        (Vec3::new(560.0, 450.0, 600.0), Vec3::NEG_X)
    };
    let rest = settle(&mut app, start);
    intent(&mut app, direction, speed, false);
    let mut frames = vec![snapshot(&mut app)];
    run_until(&mut app, 900, &mut frames, |f| {
        if up {
            f.body.x >= 440.0
        } else {
            f.body.x <= -60.0
        }
    });
    let log = ticks(&app);
    let tick_dy = deltas(std::iter::once(rest.y).chain(log.iter().map(|t| t.body.y)));
    let eye_dy = deltas(frames.iter().map(|f| f.eye.y));
    let body_frame_dy = deltas(frames.iter().map(|f| f.body.y));
    let eye_lag: Vec<f32> = eye_dy
        .iter()
        .zip(&body_frame_dy)
        .map(|(e, b)| e - b)
        .collect();
    let end = frames.last().unwrap();
    row(&name, "ticks", log.len() as f64, "ticks");
    row(&name, "net_body_dy", (end.body.y - rest.y) as f64, "units");
    row(
        &name,
        "body_dy_per_tick_max",
        max_abs(&tick_dy) as f64,
        "units/tick",
    );
    row(
        &name,
        "body_dy_per_tick_gt3",
        tick_dy.iter().filter(|d| d.abs() > 3.0).count() as f64,
        "ticks",
    );
    row(
        &name,
        "eye_dy_per_frame_max",
        max_abs(&eye_dy) as f64,
        "units/frame",
    );
    row(
        &name,
        "eye_dy_per_frame_mean_abs",
        mean_abs(&eye_dy) as f64,
        "units/frame",
    );
    row(
        &name,
        "eye_pop_ratio_max_over_mean",
        (max_abs(&eye_dy) / mean_abs(&eye_dy).max(1.0e-6)) as f64,
        "ratio",
    );
    row(
        &name,
        "eye_minus_body_dy_max_abs",
        max_abs(&eye_lag) as f64,
        "units/frame",
    );
    row(
        &name,
        "unsupported_ticks",
        log.iter().filter(|t| !t.supported).count() as f64,
        "ticks",
    );
    row(
        &name,
        "min_vertical_velocity",
        log.iter().map(|t| t.vertical_velocity).fold(0.0, f32::min) as f64,
        "units/s",
    );
    row(
        &name,
        "mean_horizontal_speed",
        (log.iter()
            .map(|t| t.horizontal_velocity.length())
            .sum::<f32>()
            / log.len() as f32) as f64,
        "units/s",
    );
}

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_stair_flights_up_and_down() {
    for speed in [80.1, 370.0] {
        for up in [true, false] {
            flight(speed, up);
        }
    }
}

// ---------------------------------------------------------------------------
// Difference 10: slope walking, both directions.
// ---------------------------------------------------------------------------

const RAMP_X0: f32 = -200.0;
const RAMP_Z: f32 = 900.0;

fn ramp(app: &mut App, degrees: f32) {
    let theta = degrees.to_radians();
    let run = 700.0;
    let length = run / theta.cos();
    let surface_mid =
        Vec3::new(RAMP_X0, 0.0, RAMP_Z) + Vec3::new(theta.cos(), theta.sin(), 0.0) * (length * 0.5);
    let center = surface_mid + Vec3::new(theta.sin(), -theta.cos(), 0.0) * 10.0;
    app.world_mut().spawn((
        RigidBody::Fixed,
        Collider::cuboid(length * 0.5, 10.0, 300.0),
        world_collision_groups(),
        Transform::from_translation(center).with_rotation(Quat::from_rotation_z(theta)),
    ));
}

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_slope_walk() {
    let speed = 370.0;
    for degrees in [10.0_f32, 25.0, 40.0, 48.0] {
        for up in [true, false] {
            let name = format!("slope{degrees}/{}", if up { "up" } else { "down" });
            let mut app = app();
            ramp(&mut app, degrees);
            let tan = degrees.to_radians().tan();
            let (start, direction, from, to) = if up {
                (
                    Vec3::new(RAMP_X0 - 80.0, 200.0, RAMP_Z),
                    Vec3::X,
                    RAMP_X0 + 150.0,
                    RAMP_X0 + 550.0,
                )
            } else {
                (
                    Vec3::new(RAMP_X0 + 650.0, tan * 650.0 + 200.0, RAMP_Z),
                    Vec3::NEG_X,
                    RAMP_X0 + 550.0,
                    RAMP_X0 + 150.0,
                )
            };
            let rest = settle(&mut app, start);
            intent(&mut app, direction, speed, false);
            let mut frames = vec![snapshot(&mut app)];
            run_until(&mut app, 600, &mut frames, |f| {
                if up { f.body.x >= to } else { f.body.x <= to }
            });
            let log = ticks(&app);
            let window: Vec<&TickRow> = log
                .iter()
                .filter(|t| {
                    if up {
                        (from..=to).contains(&t.body.x)
                    } else {
                        (to..=from).contains(&t.body.x)
                    }
                })
                .collect();
            assert!(
                window.len() > 10,
                "{name}: window too short ({})",
                window.len()
            );
            let first = window.first().unwrap().body;
            let last = window.last().unwrap().body;
            let seconds = (window.len() - 1) as f32 * PHYSICS_TIMESTEP;
            let horizontal = Vec2::new(last.x - first.x, last.z - first.z).length() / seconds;
            let along = (last - first).length() / seconds;
            let dy = deltas(window.iter().map(|t| t.body.y));
            let expected_dy = deltas(window.iter().map(|t| t.body.x))
                .into_iter()
                .map(|dx| dx * tan)
                .collect::<Vec<f32>>();
            let dy_error: Vec<f32> = dy.iter().zip(&expected_dy).map(|(a, b)| a - b).collect();
            row(&name, "start_rest_y", rest.y as f64, "units");
            row(&name, "commanded_speed", speed as f64, "units/s");
            row(&name, "horizontal_speed", horizontal as f64, "units/s");
            row(&name, "speed_along_surface", along as f64, "units/s");
            row(
                &name,
                "supported_fraction",
                window.iter().filter(|t| t.supported).count() as f64 / window.len() as f64,
                "fraction",
            );
            row(
                &name,
                "dy_per_tick_error_max_abs",
                max_abs(&dy_error) as f64,
                "units/tick",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Difference 9 and landing feel: jump, landing, air control.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_jump_and_landing() {
    for (label, run_speed) in [("standing", 0.0_f32), ("running", 370.0)] {
        let name = format!("jump/{label}");
        let mut app = app();
        let rest = settle(&mut app, Vec3::new(-300.0, 200.0, RAMP_Z));
        let wish = if run_speed > 0.0 { Vec3::X } else { Vec3::ZERO };
        intent(&mut app, wish, run_speed, false);
        if run_speed > 0.0 {
            for _ in 0..40 {
                app.update();
            }
        }
        app.world_mut().resource_mut::<TickLog>().0.clear();
        let mut frames = vec![snapshot(&mut app)];
        let launch_x = frames[0].body.x;
        intent(&mut app, wish, run_speed, true);
        app.update();
        frames.push(snapshot(&mut app));
        intent(&mut app, wish, run_speed, false);
        let mut landed = false;
        for _ in 0..240 {
            app.update();
            let frame = snapshot(&mut app);
            frames.push(frame);
            let log = &app.world().resource::<TickLog>().0;
            if log.len() > 12
                && log.last().is_some_and(|t| t.supported && !t.jump_active)
                && frame.body.y < rest.y + 1.0
            {
                landed = true;
                break;
            }
        }
        assert!(landed, "{name}: the player never landed");
        for _ in 0..40 {
            app.update();
            frames.push(snapshot(&mut app));
        }
        let log = ticks(&app);
        let apex = log.iter().map(|t| t.body.y - rest.y).fold(0.0, f32::max);
        let takeoff = log
            .iter()
            .position(|t| t.body.y > rest.y + 0.5)
            .unwrap_or(0);
        let touchdown = log
            .iter()
            .enumerate()
            .skip(takeoff + 3)
            .find(|(_, t)| t.supported && !t.jump_active)
            .map_or(log.len(), |(i, _)| i);
        let landing_speed = log[..touchdown.min(log.len())]
            .iter()
            .map(|t| t.vertical_velocity)
            .fold(0.0, f32::min);
        let bounce = log[touchdown.min(log.len() - 1)..]
            .iter()
            .map(|t| t.body.y - log[touchdown.min(log.len() - 1)].body.y)
            .fold(0.0, f32::max);
        let eye_dy = deltas(frames.iter().map(|f| f.eye.y));
        let eye_minus_body: Vec<f32> = frames.iter().map(|f| f.eye.y - f.body.y).collect();
        let (lo, hi) = eye_minus_body
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        row(&name, "rest_y", rest.y as f64, "units");
        row(&name, "apex_above_rest", apex as f64, "units");
        row(
            &name,
            "takeoff_delay_ticks",
            takeoff as f64,
            "ticks (press to y > rest+0.5)",
        );
        row(
            &name,
            "airborne_ticks",
            (touchdown - takeoff) as f64,
            "ticks",
        );
        row(
            &name,
            "landing_vertical_velocity",
            landing_speed as f64,
            "units/s",
        );
        row(&name, "post_landing_rise", bounce as f64, "units");
        row(
            &name,
            "eye_dy_per_frame_max",
            max_abs(&eye_dy) as f64,
            "units/frame",
        );
        row(&name, "eye_minus_body_y_min", lo as f64, "units");
        row(&name, "eye_minus_body_y_max", hi as f64, "units");
        row(
            &name,
            "air_distance_x",
            (frames.last().unwrap().body.x - launch_x) as f64,
            "units (whole scenario)",
        );
    }
}

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_air_control_response() {
    // Pure integrator. Release and reversal in the air, from run speed.
    let tuning = packaged_tuning();
    let run = tuning.target_speed(Vec2::new(0.0, -1.0), false, false);
    let air = |wish: Vec3| {
        let mut state = WalkState {
            horizontal_velocity: Vec3::X * run,
            ..default()
        };
        let mut trace = Vec::new();
        for _ in 0..60 {
            integrate_walk(
                &mut state,
                &WalkIntent {
                    wish_dir: wish,
                    target_speed: if wish == Vec3::ZERO { 0.0 } else { run },
                    jump_pressed: false,
                },
                &tuning,
                PHYSICS_TIMESTEP,
                false,
            );
            trace.push(state.horizontal_velocity.x);
        }
        trace
    };
    let release = air(Vec3::ZERO);
    let reverse = air(Vec3::NEG_X);
    let first_below = |trace: &[f32], limit: f32| {
        trace
            .iter()
            .position(|v| *v <= limit)
            .map_or(f64::NAN, |i| (i + 1) as f64)
    };
    row(
        "air/release",
        "speed_after_tick_1",
        release[0] as f64,
        "units/s",
    );
    row(
        "air/release",
        "speed_after_tick_3",
        release[2] as f64,
        "units/s",
    );
    row(
        "air/release",
        "ticks_to_5pct",
        first_below(&release, run * 0.05),
        "ticks",
    );
    row(
        "air/reverse",
        "speed_after_tick_1",
        reverse[0] as f64,
        "units/s",
    );
    row(
        "air/reverse",
        "speed_after_tick_3",
        reverse[2] as f64,
        "units/s",
    );
    row(
        "air/reverse",
        "ticks_to_cross_zero",
        first_below(&reverse, 0.0),
        "ticks",
    );
    row(
        "air/reverse",
        "ticks_to_90pct_reversed",
        first_below(&reverse, -run * 0.9),
        "ticks",
    );
    let ground_release = {
        let mut state = WalkState {
            horizontal_velocity: Vec3::X * run,
            ..default()
        };
        let mut count = 0;
        while state.horizontal_velocity.length() > run * 0.05 && count < 200 {
            integrate_walk(
                &mut state,
                &WalkIntent::default(),
                &tuning,
                PHYSICS_TIMESTEP,
                true,
            );
            count += 1;
        }
        count
    };
    row(
        "ground/release",
        "ticks_to_5pct",
        ground_release as f64,
        "ticks",
    );
}

// ---------------------------------------------------------------------------
// Difference 7: sprint, run and walk start and stop, plus the flat-floor probe.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_start_and_stop_acceleration() {
    let tuning = packaged_tuning();
    let forward = Vec2::new(0.0, -1.0);
    for (label, slow, sprint) in [
        ("walk", true, false),
        ("run", false, false),
        ("sprint", false, true),
    ] {
        let target = tuning.target_speed(forward, slow, sprint);
        let name = format!("accel/{label}");
        let mut state = WalkState::default();
        let go = WalkIntent {
            wish_dir: Vec3::NEG_Z,
            target_speed: target,
            jump_pressed: false,
        };
        let mut speeds = Vec::new();
        let mut distance = 0.0;
        let mut distance_at_1s = 0.0;
        for tick in 0..120 {
            integrate_walk(&mut state, &go, &tuning, PHYSICS_TIMESTEP, true);
            distance += state.horizontal_velocity.length() * PHYSICS_TIMESTEP;
            if tick == 59 {
                distance_at_1s = distance;
            }
            speeds.push(state.horizontal_velocity.length());
        }
        let reach = |fraction: f32| {
            speeds
                .iter()
                .position(|s| *s >= target * fraction - 1.0e-3)
                .map_or(f64::NAN, |i| (i + 1) as f64)
        };
        row(&name, "target_speed", target as f64, "units/s");
        row(&name, "ticks_to_63pct", reach(0.63), "ticks");
        row(&name, "ticks_to_90pct", reach(0.9), "ticks");
        row(&name, "ticks_to_100pct", reach(1.0), "ticks");
        row(
            &name,
            "distance_1s_from_rest",
            distance_at_1s as f64,
            "units",
        );
        row(&name, "distance_2s_from_rest", distance as f64, "units");
        row(
            &name,
            "steady_distance_2s_instant",
            (target * 2.0) as f64,
            "units (no ramp)",
        );
        // Release.
        let mut stop = WalkState {
            horizontal_velocity: Vec3::NEG_Z * target,
            ..default()
        };
        let mut stop_distance = 0.0;
        let mut stop_ticks = 0;
        while stop.horizontal_velocity.length() > 1.0e-3 && stop_ticks < 200 {
            integrate_walk(
                &mut stop,
                &WalkIntent::default(),
                &tuning,
                PHYSICS_TIMESTEP,
                true,
            );
            stop_distance += stop.horizontal_velocity.length() * PHYSICS_TIMESTEP;
            stop_ticks += 1;
        }
        row(&name, "stop_ticks", stop_ticks as f64, "ticks");
        row(&name, "stop_distance", stop_distance as f64, "units");
    }
}

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_flat_floor_position_error() {
    // Issue #590: a one-tick position correction on a flat floor. Records, per tick, the gap between
    // the body's travelled position and the sum of commanded displacements. Does not assert.
    for speed in [90.0_f32, 370.0, 555.0] {
        let name = format!("flat/{speed}");
        let mut app = app();
        let rest = settle(&mut app, Vec3::new(-100.0, 200.0, RAMP_Z));
        intent(&mut app, Vec3::NEG_Z, speed, false);
        let mut frames = vec![snapshot(&mut app)];
        run_until(&mut app, 600, &mut frames, |f| f.body.z <= RAMP_Z - 700.0);
        let log = ticks(&app);
        let mut commanded = Vec2::ZERO;
        let mut worst = (0.0_f32, 0_usize, 0.0_f32);
        let mut previous = Vec2::new(rest.x, rest.z);
        let mut worst_tick_jump = (0.0_f32, 0_usize);
        for (index, tick) in log.iter().enumerate() {
            commanded += Vec2::new(tick.horizontal_velocity.x, tick.horizontal_velocity.z)
                * PHYSICS_TIMESTEP;
            let actual = Vec2::new(tick.body.x, tick.body.z);
            let error = (actual - Vec2::new(rest.x, rest.z) - commanded).length();
            if error > worst.0 {
                worst = (error, index + 1, (actual - previous).length());
            }
            let step_error = ((actual - previous)
                - Vec2::new(tick.horizontal_velocity.x, tick.horizontal_velocity.z)
                    * PHYSICS_TIMESTEP)
                .length();
            if step_error > worst_tick_jump.0 {
                worst_tick_jump = (step_error, index + 1);
            }
            previous = actual;
        }
        row(&name, "ticks", log.len() as f64, "ticks");
        row(
            &name,
            "max_position_error_vs_commanded",
            worst.0 as f64,
            "units",
        );
        row(&name, "tick_of_max_position_error", worst.1 as f64, "tick");
        row(
            &name,
            "max_single_tick_step_error",
            worst_tick_jump.0 as f64,
            "units",
        );
        row(
            &name,
            "tick_of_max_step_error",
            worst_tick_jump.1 as f64,
            "tick",
        );
        row(
            &name,
            "max_y_drift",
            log.iter()
                .map(|t| (t.body.y - rest.y).abs())
                .fold(0.0, f32::max) as f64,
            "units",
        );
    }
}

// ---------------------------------------------------------------------------
// Difference 3: camera position between 60 Hz ticks, versus render rate.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_view_between_ticks_by_render_rate() {
    let speed = 370.0;
    for hz in [30.0_f64, 60.0, 120.0, 144.0, 240.0] {
        let name = format!("render{hz}");
        let mut app = app();
        settle(&mut app, Vec3::new(-100.0, 200.0, RAMP_Z));
        intent(&mut app, Vec3::NEG_Z, speed, false);
        set_render_hz(&mut app, hz);
        for _ in 0..hz as usize {
            app.update();
        }
        let mut frames = vec![snapshot(&mut app)];
        for _ in 0..hz as usize {
            app.update();
            frames.push(snapshot(&mut app));
        }
        let steps: Vec<f32> = frames
            .windows(2)
            .map(|w| (w[1].eye - w[0].eye).length())
            .collect();
        let mean = steps.iter().sum::<f32>() / steps.len() as f32;
        let variance = steps.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / steps.len() as f32;
        let repeated = steps.iter().filter(|s| **s < 1.0e-4).count();
        let ticks_run = frames.last().unwrap().ticks - frames.first().unwrap().ticks;
        row(&name, "frames", steps.len() as f64, "frames");
        row(&name, "ticks_run", ticks_run as f64, "ticks");
        row(&name, "ideal_step", (speed as f64) / hz, "units/frame");
        row(&name, "mean_step", mean as f64, "units/frame");
        row(
            &name,
            "step_cv",
            (variance.sqrt() / mean.max(1.0e-6)) as f64,
            "std/mean",
        );
        row(
            &name,
            "max_step_over_ideal",
            (steps.iter().fold(0.0_f32, |m, s| m.max(*s)) as f64) / (speed as f64 / hz),
            "ratio",
        );
        row(
            &name,
            "repeated_frame_fraction",
            repeated as f64 / steps.len() as f64,
            "fraction",
        );
    }
}

// ---------------------------------------------------------------------------
// Difference 4: mouse turn per count. Difference 5: body scale.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measure: prints the controller-feel table, asserts no feel value; run with --ignored --nocapture --test-threads=1"]
fn measure_mouse_turn_and_body_scale() {
    let mut look = LookIntent::default();
    sample_look_intent(&CursorCapture::Captured, Vec2::new(1.0, 0.0), &mut look);
    let yaw = look.yaw.abs();
    let mut look = LookIntent::default();
    sample_look_intent(&CursorCapture::Captured, Vec2::new(0.0, 1.0), &mut look);
    let pitch = look.pitch.abs();
    row("mouse", "yaw_per_count", yaw as f64, "rad");
    row(
        "mouse",
        "yaw_per_count_degrees",
        (yaw as f64).to_degrees(),
        "deg",
    );
    row("mouse", "pitch_per_count", pitch as f64, "rad");
    row(
        "mouse",
        "counts_per_360_degrees",
        std::f64::consts::TAU / yaw as f64,
        "counts",
    );
    // Inputs of the spud estimate, read from the local retail files on 2026-11-14:
    // SkyrimPrefs.ini fMouseHeadingSensitivity=0.0125, Skyrim.ini fMouseHeadingXScale=0.02 and
    // fMouseHeadingYScale=0.85, Skyrim.esm NPC_Default_MT SPED floats 8..10 = 3.14159 (pi).
    let derived = 0.0125_f64 * 0.02 * std::f64::consts::PI;
    row(
        "mouse",
        "derived_retail_yaw_per_count",
        derived,
        "rad (INI x INI x MOVT rotate_in_place run)",
    );
    row(
        "mouse",
        "mudcrab_over_derived",
        yaw as f64 / derived,
        "ratio",
    );

    let tuning = MovementTuning::default();
    let mut app = app();
    let rest = settle(&mut app, Vec3::new(-100.0, 200.0, RAMP_Z));
    let eye = snapshot(&mut app).eye;
    row(
        "body",
        "capsule_radius",
        tuning.capsule_radius as f64,
        "units",
    );
    row(
        "body",
        "capsule_height",
        tuning.capsule_standing_height as f64,
        "units",
    );
    row(
        "body",
        "eye_offset_above_body_center",
        tuning.eye_height as f64,
        "units",
    );
    row(
        "body",
        "resting_center_above_floor",
        rest.y as f64,
        "units (floor top = 0)",
    );
    row("body", "eye_above_floor_at_rest", eye.y as f64, "units");
}
