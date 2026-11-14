//! Headless stair traversal through the production fixed-step WALK controller.

use super::{headless, *};
use bevy::ecs::system::RunSystemOnce;

fn fixed_box(app: &mut App, center: Vec3, half_extents: Vec3) {
    app.world_mut().spawn((
        RigidBody::Fixed,
        Collider::cuboid(half_extents.x, half_extents.y, half_extents.z),
        world_collision_groups(),
        Transform::from_translation(center),
    ));
}

fn probe_step(app: &mut App, position: Vec3, horizontal: Vec3) -> Option<f32> {
    app.world_mut()
        .run_system_once(
            move |context: ReadRapierContext,
                  tuning: Res<MovementTuning>,
                  player: WalkBodyQuery| {
                let (entity, body, capsule, controller, _, _) = player.single().unwrap();
                let pose = Transform {
                    translation: position,
                    ..*body
                };
                walk_step_rise(
                    &context.single().unwrap(),
                    entity,
                    &pose,
                    capsule,
                    controller,
                    &tuning,
                    horizontal,
                )
            },
        )
        .unwrap()
}

fn settle(app: &mut App, position: Vec3) -> Vec3 {
    headless::place_player(app, position);
    for _ in 0..150 {
        app.update();
    }
    let (pose, grounded) = headless::player_pose(app);
    assert!(grounded, "player did not settle: {pose:?}");
    pose
}

fn traverse(app: &mut App, direction: Vec3, speed: f32, target: impl Fn(Vec3) -> bool) -> Vec3 {
    app.insert_resource(WalkIntent {
        wish_dir: direction.normalize(),
        target_speed: speed,
        jump_pressed: false,
    });
    for _ in 0..900 {
        app.update();
        let pose = headless::player_pose(app).0;
        assert!(pose.is_finite());
        let mut query = app.world_mut().query::<&WalkState>();
        assert!(!query.single(app.world()).unwrap().jump_active);
        if target(pose) {
            app.insert_resource(WalkIntent::default());
            // Brake before inspecting support; the controller must stand on the landing.
            for _ in 0..30 {
                app.update();
            }
            let (pose, grounded) = headless::player_pose(app);
            assert!(grounded, "lost support on landing: {pose:?}");
            return pose;
        }
    }
    panic!(
        "stair traversal stalled at {:?}, direction={direction:?}, speed={speed}",
        headless::player_pose(app)
    );
}

#[test]
fn narrow_stair_flights_walk_run_oblique_and_descend() {
    // The BleakFallsTower stair meshes have approximately 24-unit rises/treads.
    // Exercise an entire flight, separate collider seams, and oblique approaches.
    for speed in [80.1, 370.0] {
        for angle in [0.0_f32, 20.0, -20.0, 45.0] {
            for seam in [0.0, 0.25] {
                let mut app = headless::fixture_app();
                for index in 0..16 {
                    let top = (index + 1) as f32 * 24.0;
                    fixed_box(
                        &mut app,
                        Vec3::new(index as f32 * 24.0 + 12.0, top * 0.5, 850.0),
                        Vec3::new((24.0 - seam) * 0.5, top * 0.5, 700.0),
                    );
                }
                fixed_box(
                    &mut app,
                    Vec3::new(642.0, 192.0, 850.0),
                    Vec3::new(258.0, 192.0, 700.0),
                );
                let start = settle(&mut app, Vec3::new(-100.0, 200.0, 600.0));
                let direction = Vec3::new(1.0, 0.0, angle.to_radians().tan());
                let top = traverse(&mut app, direction, speed, |p| p.x >= 440.0);
                assert!(
                    (top.y - start.y - 384.0).abs() < 3.0,
                    "angle={angle}, seam={seam}, {start:?} -> {top:?}"
                );
                let bottom = traverse(&mut app, -direction, speed, |p| p.x <= -100.0);
                assert!(
                    (bottom.y - start.y).abs() < 3.0,
                    "angle={angle}, seam={seam}, {top:?} -> {bottom:?}"
                );
            }
        }
    }
}

#[test]
fn camera_presentation_tracks_production_stair_flight_without_changing_body_motion() {
    let mut app = headless::fixture_app();
    for index in 0..32 {
        let top = (index + 1) as f32 * 12.0;
        fixed_box(
            &mut app,
            Vec3::new(index as f32 * 24.0 + 12.0, top * 0.5, 850.0),
            Vec3::new(12.0, top * 0.5, 700.0),
        );
    }
    fixed_box(
        &mut app,
        Vec3::new(1026.0, 192.0, 850.0),
        Vec3::new(258.0, 192.0, 700.0),
    );
    let start = settle(&mut app, Vec3::new(-100.0, 200.0, 850.0));
    let tuning = MovementTuning::default();
    let body = {
        let mut query = app.world_mut().query_filtered::<Entity, With<PlayerBody>>();
        query.single(app.world()).unwrap()
    };
    let camera = {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<StreamingCamera>>();
        query.single(app.world()).unwrap()
    };
    app.world_mut()
        .get_mut::<Transform>(camera)
        .unwrap()
        .translation = start + Vec3::Y * tuning.eye_height;
    app.update();

    app.insert_resource(WalkIntent {
        wish_dir: Vec3::X,
        target_speed: 370.0,
        jump_pressed: false,
    });
    app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f32(1.0 / 60.0),
    ));
    let mut previous_camera_y = app.world().get::<Transform>(camera).unwrap().translation.y;
    let mut largest_rise = 0.0_f32;
    let mut moved_up = false;
    for _ in 0..900 {
        let previous_body = app.world().get::<Transform>(body).unwrap().translation;
        app.update();
        let body_pose = app.world().get::<Transform>(body).unwrap().translation;
        let camera_y = app.world().get::<Transform>(camera).unwrap().translation.y;
        let offset = app.world().resource::<WalkCameraFollowState>().offset_y;
        assert!(
            (camera_y - (body_pose.y + tuning.eye_height - offset)).abs() < 1.0e-3,
            "camera transform did not apply the accumulated presentation offset"
        );
        let rise = body_pose.y - previous_body.y;
        if rise > 10.0 {
            let tick = app.world().resource::<WalkCameraTick>();
            assert!(
                tick.last_sample_validated_rise && tick.last_sample_collected_y > 0.0,
                "physical stair rise lacked the fixed-tick validated marker: rise={rise}, marker={}, collected_y={}",
                tick.last_sample_validated_rise,
                tick.last_sample_collected_y
            );
            moved_up = true;
            largest_rise = largest_rise.max(rise);
            assert!(
                camera_y - previous_camera_y < rise,
                "camera followed a physical stair rise instantly: body rise={rise}, camera rise={}",
                camera_y - previous_camera_y
            );
        }
        previous_camera_y = camera_y;
        if body_pose.x >= 820.0 {
            break;
        }
    }
    app.insert_resource(WalkIntent::default());
    let offset_before_settle = app.world().resource::<WalkCameraFollowState>().offset_y;
    for _ in 0..30 {
        app.update();
    }
    let top = app.world().get::<Transform>(body).unwrap().translation;
    assert!(
        moved_up && largest_rise > 1.0,
        "production controller produced no stair rises"
    );
    assert!(
        (top.y - start.y - 384.0).abs() < 3.0,
        "camera work changed the physical stair endpoint: {start:?} -> {top:?}"
    );
    let settled_camera_y = app.world().get::<Transform>(camera).unwrap().translation.y;
    let expected_offset =
        offset_before_settle * (-(30.0 / 60.0) / WALK_STEP_CAMERA_RESPONSE_SECONDS).exp();
    let actual_offset = app.world().resource::<WalkCameraFollowState>().offset_y;
    assert!(
        (actual_offset - expected_offset).abs() < 1.0e-3,
        "offset recurrence mismatch: expected {expected_offset}, got {actual_offset}"
    );
    assert!((settled_camera_y - (top.y + tuning.eye_height - expected_offset)).abs() < 1.0e-3);

    // Three fixed ticks per rendered frame tests the accumulated-delta path.
    app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f32(1.0 / 20.0),
    ));
    app.insert_resource(WalkIntent {
        wish_dir: -Vec3::X,
        target_speed: 370.0,
        jump_pressed: false,
    });
    let mut previous_camera_y = settled_camera_y;
    let mut descended = false;
    let mut smoothed_descent = false;
    for _ in 0..300 {
        let previous_body_y = app.world().get::<Transform>(body).unwrap().translation.y;
        app.update();
        let body_pose = app.world().get::<Transform>(body).unwrap().translation;
        let camera_y = app.world().get::<Transform>(camera).unwrap().translation.y;
        let offset = app.world().resource::<WalkCameraFollowState>().offset_y;
        assert!(
            (camera_y - (body_pose.y + tuning.eye_height - offset)).abs() < 1.0e-3,
            "camera transform did not apply the accumulated presentation offset"
        );
        if body_pose.y < previous_body_y - 6.0 {
            descended = true;
            // Catching up from an earlier step can exceed this frame's body motion.
            // The response must stay between its previous eye and current target.
            let target_y = body_pose.y + tuning.eye_height;
            assert!(
                camera_y >= target_y.min(previous_camera_y) - 1.0e-3
                    && camera_y <= target_y.max(previous_camera_y) + 1.0e-3,
                "descent camera overshot its target: previous={previous_camera_y}, target={target_y}, actual={camera_y}"
            );
            if app
                .world()
                .resource::<WalkCameraTick>()
                .last_sample_collected_y
                < -1.0
            {
                assert!(
                    camera_y > target_y + 1.0e-3,
                    "a supported downward snap moved the view to its target instantly"
                );
                smoothed_descent = true;
            }
        }
        previous_camera_y = camera_y;
        if body_pose.x <= -100.0 {
            break;
        }
    }
    assert!(
        descended,
        "production controller produced no supported stair descent"
    );
    assert!(
        smoothed_descent,
        "the fixture never exercised a smoothed supported downward snap"
    );
    app.insert_resource(WalkIntent::default());
    for _ in 0..30 {
        app.update();
    }
    let bottom = app.world().get::<Transform>(body).unwrap().translation;
    assert!(
        (bottom.y - start.y).abs() < 3.0,
        "camera work changed the physical return endpoint: {start:?} -> {bottom:?}"
    );
}

/// A fixed slab whose top surface starts at `(0, top_y)` and descends toward +X at `degrees`.
fn fixed_descending_ramp(app: &mut App, degrees: f32, length: f32, top_y: f32) {
    let angle = degrees.to_radians();
    let half_thickness = 20.0;
    let normal = Vec3::new(angle.sin(), angle.cos(), 0.0);
    let middle =
        Vec3::new(0.0, top_y, 850.0) + Vec3::new(angle.cos(), -angle.sin(), 0.0) * (length * 0.5);
    app.world_mut().spawn((
        RigidBody::Fixed,
        Collider::cuboid(length * 0.5, half_thickness, 700.0),
        world_collision_groups(),
        Transform::from_translation(middle - normal * half_thickness)
            .with_rotation(Quat::from_rotation_z(-angle)),
    ));
}

/// Walking down a walkable ramp or hill is continuous ground contact. The eye must stay at its
/// normal height above the body on every frame, not float above it as it would for a stair drop.
#[test]
fn camera_follows_the_body_directly_down_walkable_slopes() {
    for (degrees, speed) in [
        (10.0_f32, 370.0),
        (20.0, 370.0),
        (35.0, 370.0),
        (20.0, 80.1),
    ] {
        let mut app = headless::fixture_app();
        let length = 1_400.0;
        fixed_descending_ramp(&mut app, degrees, length, 700.0);
        let surface = |x: f32| 700.0 - x * degrees.to_radians().tan();
        let start_x = 40.0;
        let start = settle(&mut app, Vec3::new(start_x, surface(start_x) + 60.0, 850.0));
        let tuning = MovementTuning::default();
        let camera = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<StreamingCamera>>();
            query.single(app.world()).unwrap()
        };
        app.world_mut()
            .get_mut::<Transform>(camera)
            .unwrap()
            .translation = start + Vec3::Y * tuning.eye_height;
        app.update();
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: speed,
            jump_pressed: false,
        });
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f32(1.0 / 60.0),
        ));
        let end_x = length * degrees.to_radians().cos() * 0.8;
        let mut frames = 0;
        let mut grounded_frames = 0;
        let mut lowest = start.y;
        for _ in 0..2_000 {
            app.update();
            frames += 1;
            let (body, grounded) = headless::player_pose(&mut app);
            grounded_frames += u32::from(grounded);
            let camera_y = app.world().get::<Transform>(camera).unwrap().translation.y;
            let follow = app.world().resource::<WalkCameraFollowState>();
            let tick = app.world().resource::<WalkCameraTick>();
            assert_eq!(
                tick.last_sample_collected_y, 0.0,
                "{degrees} deg at {speed} u/s: a ramp tick was collected as a stair drop at {body:?}"
            );
            assert!(
                follow.offset_y.abs() < 1.0e-3
                    && (camera_y - (body.y + tuning.eye_height)).abs() < 1.0e-3,
                "{degrees} deg at {speed} u/s: the view floats {:.2} u above its eye target at {body:?}",
                camera_y - (body.y + tuning.eye_height)
            );
            lowest = lowest.min(body.y);
            if body.x >= end_x {
                break;
            }
        }
        // Controls: the walk really went downhill and reached the far part of the ramp, and the
        // body spent most frames on the ground (an airborne frame resets the offset by itself).
        assert!(
            grounded_frames * 2 > frames,
            "{degrees} deg at {speed} u/s: only {grounded_frames}/{frames} grounded frames"
        );
        let end = headless::player_pose(&mut app).0;
        assert!(
            end.x >= end_x,
            "{degrees} deg at {speed} u/s stalled at {end:?} after {frames}"
        );
        assert!(
            start.y - lowest > 100.0,
            "{degrees} deg at {speed} u/s: the fixture did not descend ({start:?} -> {lowest})"
        );
    }
}

#[test]
fn stairs_keep_rise_and_overhead_clearance_limits() {
    for (rise, ceiling) in [(48.0, false), (24.0, true)] {
        let mut app = headless::fixture_app();
        fixed_box(
            &mut app,
            Vec3::new(100.0, rise * 0.5, 850.0),
            Vec3::new(100.0, rise * 0.5, 100.0),
        );
        if ceiling {
            fixed_box(
                &mut app,
                Vec3::new(0.0, 120.0, 850.0),
                Vec3::new(300.0, 10.0, 100.0),
            );
        }
        let start = settle(&mut app, Vec3::new(-100.0, 55.0, 850.0));
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: 370.0,
            jump_pressed: false,
        });
        for _ in 0..180 {
            app.update();
        }
        let end = headless::player_pose(&mut app).0;
        assert!(
            end.x < -25.0 && end.y < start.y + 3.0,
            "rise={rise}, ceiling={ceiling}, {start:?} -> {end:?}"
        );
    }
}

#[test]
fn unsupported_capsule_does_not_mantle_a_step() {
    let mut app = headless::fixture_app();
    fixed_box(
        &mut app,
        Vec3::new(100.0, 12.0, 850.0),
        Vec3::new(100.0, 12.0, 100.0),
    );
    let start = Vec3::new(-29.0, 62.4, 850.0);
    headless::place_player(&mut app, start);
    // Sync the obstacle and query pose without producing a grounded WALK tick.
    app.insert_resource(MoveMode::Noclip);
    app.update();
    let synced_start = headless::player_pose(&mut app).0;
    assert!(synced_start.distance(start) < 1.0e-4);
    // Prove the geometry offers a step before testing the caller's support gate.
    // Without that gate this pose could rise, despite having no floor support.
    assert!(probe_step(&mut app, start, Vec3::X * (370.0 * PHYSICS_TIMESTEP)).is_some());
    app.insert_resource(MoveMode::Walk);
    let mut query = app.world_mut().query::<&mut WalkState>();
    query
        .single_mut(app.world_mut())
        .unwrap()
        .horizontal_velocity = Vec3::X * 370.0;
    app.insert_resource(WalkIntent {
        wish_dir: Vec3::X,
        target_speed: 370.0,
        jump_pressed: false,
    });
    app.update();
    let end = headless::player_pose(&mut app).0;
    assert!(
        end.y <= start.y + 0.1,
        "unsupported mantle: {start:?} -> {end:?}"
    );
}

#[test]
fn walking_recovers_shallow_floor_skin_gaps() {
    // Cover lost skin, contact at the floor, float-scale overlap, and one unit
    // of solid overlap. All must recover without a multi-step-height pop.
    for start_y in [51.4, 50.4, 50.39, 49.4] {
        let mut app = headless::fixture_app();
        let rest = settle(&mut app, Vec3::new(-100.0, 200.0, 850.0));
        let start = Vec3::new(-100.0, start_y, 850.0);
        // Preserve support output while syncing the disturbed pose without WALK.
        headless::place_player(&mut app, start);
        app.insert_resource(MoveMode::Noclip);
        app.update();
        assert!(headless::player_pose(&mut app).0.distance(start) < 1.0e-4);
        app.insert_resource(MoveMode::Walk);
        app.insert_resource(WalkIntent {
            wish_dir: Vec3::X,
            target_speed: 80.1,
            jump_pressed: false,
        });
        // Exact floor contact can allow tangential motion without a blocking
        // hit. Only the lost-skin case requires immediate clearance recovery;
        // other shallow contacts must not sink further or produce a height pop.
        let contact_y = MovementTuning::default().capsule_standing_height * 0.5;
        let minimum_y = if start_y > contact_y {
            52.0
        } else {
            start_y - 0.05
        };
        for _ in 0..30 {
            app.update();
            let pose = headless::player_pose(&mut app).0;
            assert!(
                pose.y > minimum_y && pose.y < rest.y + 0.5,
                "start_y={start_y}: skin contact sank or produced a height pop: {pose:?}"
            );
        }
        let end = headless::player_pose(&mut app).0;
        assert!(
            end.x > -70.0,
            "start_y={start_y}: skin contact stalled WALK: {end:?}"
        );
    }
}

#[test]
fn skin_recovery_does_not_lift_deep_floor_penetration() {
    let mut app = headless::fixture_app();
    settle(&mut app, Vec3::new(-100.0, 200.0, 850.0));
    assert!(
        probe_step(&mut app, Vec3::new(-100.0, 51.4, 850.0), Vec3::X).is_some(),
        "control: the same floor must offer a shallow skin repair"
    );
    for y in [40.0, 20.0] {
        assert!(
            probe_step(&mut app, Vec3::new(-100.0, y, 850.0), Vec3::X).is_none(),
            "deep floor penetration at y={y} was treated as lost skin clearance"
        );
    }
}

#[test]
fn step_clearance_uses_the_controller_collision_filter() {
    let mut app = headless::fixture_app();
    fixed_box(
        &mut app,
        Vec3::new(100.0, 12.0, 850.0),
        Vec3::new(100.0, 12.0, 100.0),
    );
    // The default controller queries every group. A separate ceiling group must
    // still block the lift, even though player_collision_groups omits GROUP_5.
    app.world_mut().spawn((
        RigidBody::Fixed,
        Collider::cuboid(300.0, 10.0, 100.0),
        CollisionGroups::new(Group::GROUP_5, Group::ALL),
        Transform::from_xyz(0.0, 120.0, 850.0),
    ));
    settle(&mut app, Vec3::new(-100.0, 55.0, 850.0));
    let position = Vec3::new(-29.0, 52.45, 850.0);
    let horizontal = Vec3::X * (370.0 * PHYSICS_TIMESTEP);
    let mut query = app.world_mut().query::<&mut KinematicCharacterController>();
    query.single_mut(app.world_mut()).unwrap().filter_groups = Some(player_collision_groups());
    assert!(
        probe_step(&mut app, position, horizontal).is_some(),
        "control: excluding the separate ceiling group should expose the step"
    );
    query.single_mut(app.world_mut()).unwrap().filter_groups = None;
    assert!(
        probe_step(&mut app, position, horizontal).is_none(),
        "step clearance ignored a ceiling included by the controller filter"
    );
}

#[test]
#[ignore = "requires the immutable local Skyrim asset pack; set MUDCRAB_STAIR_ASSETS"]
fn packed_bleak_falls_tower_stair_flights() {
    let assets = std::env::var_os("MUDCRAB_STAIR_ASSETS").expect("set MUDCRAB_STAIR_ASSETS");
    for (model, x, base, start_z, end_z, top_y, sign) in [
        (
            "nortowerruinsinttopfloorstairs01",
            -272.0,
            896.14,
            208.0,
            -320.0,
            1280.16,
            -1.0,
        ),
        (
            "nortowerruinsinttopfloorstairs02",
            163.0,
            1280.16,
            -320.0,
            90.0,
            1534.68,
            1.0,
        ),
    ] {
        let path = Path::new(&assets).join(format!(
            "meshes/dungeons/nordic/exterior/towers/{model}.glb"
        ));
        let bytes = std::fs::read(&path).unwrap();
        let length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let glb: serde_json::Value = serde_json::from_slice(&bytes[20..20 + length]).unwrap();
        let collision: shared::collision::CollisionAsset =
            serde_json::from_value(glb["scenes"][0]["extras"]["mudcrabCollision"].clone()).unwrap();
        assert!(collision.authored && collision.skipped.is_empty());
        for speed in [80.1, 370.0] {
            for angle in [0.0_f32, 5.0, -5.0] {
                let mut app = headless::fixture_app();
                for shape in &collision.shapes {
                    let shared::collision::CollisionShape::Mesh {
                        vertices,
                        triangles,
                    } = shape
                    else {
                        panic!("unexpected tower shape")
                    };
                    app.world_mut().spawn((
                        RigidBody::Fixed,
                        Collider::trimesh(
                            vertices.iter().copied().map(Vec3::from_array).collect(),
                            triangles.clone(),
                        )
                        .unwrap(),
                        world_collision_groups(),
                        Transform::IDENTITY,
                    ));
                }
                fixed_box(
                    &mut app,
                    Vec3::new(x, base - 10.0, start_z),
                    Vec3::new(63.0, 10.0, 80.0),
                );
                fixed_box(
                    &mut app,
                    Vec3::new(x, top_y - 10.0, end_z + sign * 15.0),
                    Vec3::new(63.0, 10.0, 95.0),
                );
                let angle_radians = angle.to_radians();
                let start_x = x - angle_radians.tan() * (end_z - start_z).abs() * 0.5;
                let direction = Vec3::new(angle_radians.sin(), 0.0, sign * angle_radians.cos());
                let start = settle(&mut app, Vec3::new(start_x, base + 200.0, start_z));
                let top = traverse(&mut app, direction, speed, |p| sign * (p.z - end_z) >= 0.0);
                assert!(
                    (top.y - top_y - 52.4).abs() < 3.0,
                    "{model}: {start:?} -> {top:?}"
                );
                let bottom = traverse(&mut app, -direction, speed, |p| {
                    sign * (p.z - start_z) <= 0.0
                });
                assert!(
                    (bottom.y - start.y).abs() < 3.0,
                    "{model}: {top:?} -> {bottom:?}"
                );
                eprintln!(
                    "{model}, speed={speed}, angle={angle}, {start:?} -> {top:?} -> {bottom:?}"
                );
            }
        }
    }
}
