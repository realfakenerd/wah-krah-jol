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
