//! Opt-in tests against the registered pack's real authored container data.
use super::*;
use bevy::ecs::system::RunSystemOnce;
use bevy_rapier3d::prelude::{KinematicCharacterControllerOutput, QueryFilter, ReadRapierContext};
use std::path::Path;

fn mesh_data(pack: &Path, name: &str) -> CollisionAsset {
    let bytes = std::fs::read(pack.join(name)).unwrap();
    assert_eq!(&bytes[0..4], b"glTF");
    let len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let json: serde_json::Value = serde_json::from_slice(&bytes[20..20 + len]).unwrap();
    serde_json::from_value(json["scenes"][0]["extras"]["mudcrabCollision"].clone()).unwrap()
}

fn ray_triangle(origin: Vec3, direction: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let edge1 = b - a;
    let edge2 = c - a;
    let cross = direction.cross(edge2);
    let det = edge1.dot(cross);
    if det.abs() < 1.0e-6 {
        return None;
    }
    let inv = det.recip();
    let t = origin - a;
    let u = t.dot(cross) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = t.cross(edge1);
    let v = direction.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let distance = edge2.dot(q) * inv;
    (distance >= 0.0).then_some(distance)
}

#[test]
#[ignore = "requires MUDCRAB_COLLISION_PACK and its registered game data"]
fn real_container_placement_geometry_and_player() {
    let pack = std::path::PathBuf::from(std::env::var("MUDCRAB_COLLISION_PACK").unwrap());
    let db = rusqlite::Connection::open_with_flags(
        pack.join("skyrim_world.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    for (id, path) in [
        (0x12df8u32, "meshes/clutter/barrel01.glb"),
        (0x86176u32, "meshes/clutter/sburialurn01.glb"),
    ] {
        let (pos, rot, scale): ([f32; 3], [f32; 3], f32) = db
            .query_row(
                "SELECT pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,scale FROM \"references\" WHERE id=?1 AND header_flags=0",
                [id],
                |r| Ok(([r.get(0)?, r.get(1)?, r.get(2)?], [r.get(3)?, r.get(4)?, r.get(5)?], r.get(6)?)),
            )
            .unwrap();
        let asset = mesh_data(&pack, path);
        let CollisionShape::Mesh {
            vertices,
            triangles,
        } = &asset.shapes[0]
        else {
            panic!("expected authored mesh")
        };
        let decision =
            static_decision_from_asset(fixed_collision_tier(Some("CONT"), path).unwrap(), &asset);
        let StaticCollisionDecision::Authored(parts, _) = decision else {
            panic!("asset rejected: {path}")
        };
        let mut app = crate::physics::headless::fixture_app();
        // Use the recorded placement in world coordinates. The fixture has a simple ground
        // at this position; it does not load the surrounding game cell.
        let origin = creation_to_bevy(Vec3::from_array(pos));
        app.world_mut().spawn((
            RigidBody::Fixed,
            Collider::cuboid(1000.0, 20.0, 1000.0),
            Transform::from_translation(origin + Vec3::new(0.0, -20.0, 0.0)),
            crate::physics::world_collision_groups(),
        ));
        let transform = Transform::from_translation(origin)
            .with_rotation(creation_rotation_to_bevy(rot))
            .with_scale(Vec3::splat(scale));
        let root = app.world_mut().spawn(transform).id();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                attach_authored_collision(&mut commands, root, &parts)
            })
            .unwrap();
        for _ in 0..3 {
            app.update();
        }
        let body_translation = app
            .world_mut()
            .run_system_once(move |ctx: ReadRapierContext| {
                let ctx = ctx.single().unwrap();
                ctx.rigidbody_set.bodies[ctx.entity2body()[&root]].translation()
            })
            .unwrap();
        assert!(
            (body_translation - origin).length() < 0.01,
            "wrong Rapier placement for {path}"
        );
        let world_vertices: Vec<Vec3> = vertices
            .iter()
            .map(|v| transform.to_matrix().transform_point3(Vec3::from_array(*v)))
            .collect();
        let min = world_vertices.iter().copied().reduce(Vec3::min).unwrap();
        let max = world_vertices.iter().copied().reduce(Vec3::max).unwrap();
        let mut hits = 0;
        for ix in 1..4 {
            for iz in 1..4 {
                let origin = Vec3::new(
                    min.x + (max.x - min.x) * ix as f32 / 4.0,
                    max.y + 10.0,
                    min.z + (max.z - min.z) * iz as f32 / 4.0,
                );
                let expected = triangles
                    .iter()
                    .filter_map(|t| {
                        ray_triangle(
                            origin,
                            Vec3::NEG_Y,
                            world_vertices[t[0] as usize],
                            world_vertices[t[1] as usize],
                            world_vertices[t[2] as usize],
                        )
                    })
                    .min_by(f32::total_cmp);
                if let Some(expected) = expected {
                    let (entity, distance) = app
                        .world_mut()
                        .run_system_once(move |ctx: ReadRapierContext| {
                            ctx.single().unwrap().cast_ray(
                                origin,
                                Vec3::NEG_Y,
                                expected + 1.0,
                                true,
                                QueryFilter::default()
                                    .groups(crate::physics::player_collision_groups()),
                            )
                        })
                        .unwrap()
                        .expect("authored mesh must be hit");
                    assert_eq!(entity, root, "unexpected collider before {path}");
                    assert!(
                        (distance - expected).abs() < 0.05,
                        "{path} hit {distance}, expected {expected}"
                    );
                    hits += 1;
                }
            }
        }
        assert!(hits >= 4, "insufficient rays for {path}: {hits}");
        let outside = Vec3::new(max.x + 50.0, max.y + 10.0, max.z + 50.0);
        let blocker = app
            .world_mut()
            .run_system_once(move |ctx: ReadRapierContext| {
                ctx.single().unwrap().cast_ray(
                    outside,
                    Vec3::NEG_Y,
                    10.0,
                    true,
                    QueryFilter::default().groups(crate::physics::player_collision_groups()),
                )
            })
            .unwrap();
        assert!(
            blocker.is_none(),
            "unexpected elevated blocker outside {path}"
        );
        crate::physics::headless::place_player(&mut app, origin + Vec3::new(-250.0, 200.0, 0.0));
        app.insert_resource(crate::physics::WalkIntent {
            wish_dir: Vec3::X,
            target_speed: crate::physics::MovementTuning::default().run_speed,
            jump_pressed: false,
        });
        let mut contacted = false;
        for _ in 0..180 {
            app.update();
            let mut query = app.world_mut().query_filtered::<&KinematicCharacterControllerOutput, With<crate::physics::PlayerBody>>();
            contacted |= query.iter(app.world()).any(|output| {
                output
                    .collisions
                    .iter()
                    .any(|collision| collision.entity == root)
            });
        }
        let (position, _) = crate::physics::headless::player_pose(&mut app);
        assert!(contacted, "player did not contact {path}: {position:?}");
        crate::physics::headless::place_player(&mut app, origin + Vec3::new(-250.0, 200.0, 150.0));
        app.insert_resource(crate::physics::WalkIntent {
            wish_dir: Vec3::X,
            target_speed: crate::physics::MovementTuning::default().run_speed,
            jump_pressed: false,
        });
        for _ in 0..180 {
            app.update();
        }
        let (clear_position, _) = crate::physics::headless::player_pose(&mut app);
        assert!(
            clear_position.x > origin.x + 50.0,
            "unexpected blocker beside {path}: {clear_position:?}"
        );
        println!(
            "reference {id:08x} {path}: {hits} matching rays, player x {}, clear-path x {}",
            position.x, clear_position.x
        );
    }
}
