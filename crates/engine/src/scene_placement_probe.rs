//! Feature-gated, opt-in probe of two authored placements in streamed game cells.
use crate::{
    config::EngineConfig,
    physics::{MoveMode, PlayerBody},
    sky::CameraSpace,
    streaming::{
        ActiveSpace, RenderOrigin, StreamingMetrics, creation_rotation_to_bevy, creation_to_bevy,
    },
    world::components::{FormId, StreamingCamera, WorldPosition},
};
use bevy::prelude::*;
use bevy_rapier3d::prelude::{Collider, QueryFilter, ReadRapierContext, RigidBodyDisabled};
use shared::collision::{CollisionAsset, CollisionShape};

#[derive(Resource)]
struct Probe {
    id: u32,
    interior: bool,
    position: Vec3,
    rotation: [f32; 3],
    vertices: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
    initialized: bool,
    done: bool,
    started_at: Option<f64>,
    stable_since: Option<f64>,
    output: String,
}

pub struct ScenePlacementProbe;
impl Plugin for ScenePlacementProbe {
    fn build(&self, app: &mut App) {
        let kind = std::env::var("MUDCRAB_PLACEMENT_KIND").expect("placement kind");
        let (id, interior, position, rotation, path) = match kind.as_str() {
            "barrel" => (
                0x12df8,
                false,
                Vec3::new(21850.496, -45762.773, -129.13734),
                [0.0, 0.0, 4.4270086],
                "meshes/clutter/barrel01.glb",
            ),
            "urn" => (
                0x86176,
                true,
                Vec3::new(-172.0, -3284.0, -496.0),
                [0.14834438, -0.057883594, 3.1763694],
                "meshes/clutter/sburialurn01.glb",
            ),
            _ => panic!("unknown placement kind"),
        };
        let config = app.world().resource::<EngineConfig>();
        let bytes = std::fs::read(config.assets_dir.join(path)).expect("read placement GLB");
        let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let doc: serde_json::Value = serde_json::from_slice(&bytes[20..20 + size]).unwrap();
        let asset: CollisionAsset =
            serde_json::from_value(doc["scenes"][0]["extras"]["mudcrabCollision"].clone()).unwrap();
        let CollisionShape::Mesh {
            vertices,
            triangles,
        } = asset.shapes.into_iter().next().unwrap()
        else {
            panic!("expected mesh")
        };
        app.insert_resource(Probe {
            id,
            interior,
            position,
            rotation,
            vertices,
            triangles,
            initialized: false,
            done: false,
            started_at: None,
            stable_since: None,
            output: std::env::var("MUDCRAB_PLACEMENT_PROBE").unwrap(),
        })
        .add_systems(PreUpdate, initialize)
        .add_systems(Last, inspect);
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn initialize(
    mut commands: Commands,
    mut state: ResMut<Probe>,
    mut space: ResMut<ActiveSpace>,
    mut camera_space: ResMut<CameraSpace>,
    origin: Res<RenderOrigin>,
    mut mode: ResMut<MoveMode>,
    mut camera: Query<&mut Transform, (With<StreamingCamera>, Without<PlayerBody>)>,
    player: Query<Entity, With<PlayerBody>>,
) {
    if state.initialized {
        return;
    }
    let (Ok(mut camera), Ok(player)) = (camera.single_mut(), player.single()) else {
        return;
    };
    let position = if state.interior {
        *space = ActiveSpace {
            interior: Some(0xdd216),
            worldspace_id: None,
        };
        *camera_space = CameraSpace::Interior;
        creation_to_bevy(state.position)
    } else {
        creation_to_bevy(WorldPosition::from_creation_units(state.position).relative_to(origin.0))
    };
    *mode = MoveMode::Noclip;
    commands.entity(player).insert(RigidBodyDisabled);
    camera.translation = position + Vec3::new(0.0, 150.0, 200.0);
    state.initialized = true;
}

fn ray_triangle(origin: Vec3, direction: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let p = direction.cross(e2);
    let d = e1.dot(p);
    if d.abs() < 1e-6 {
        return None;
    }
    let inv = d.recip();
    let t = origin - a;
    let u = t.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = t.cross(e1);
    let v = direction.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let distance = e2.dot(q) * inv;
    (distance >= 0.0).then_some(distance)
}

fn timeout_reason(
    elapsed: f64,
    initialized: bool,
    ready: bool,
    target_found: bool,
) -> Option<&'static str> {
    if elapsed < 120.0 {
        None
    } else if !initialized {
        Some("probe initialization did not complete")
    } else if !ready {
        Some("streaming did not become ready")
    } else if !target_found {
        Some("target reference or required root collider was not found")
    } else {
        Some("probe did not stabilize")
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn inspect(
    mut state: ResMut<Probe>,
    time: Res<Time<Real>>,
    metrics: Res<StreamingMetrics>,
    origin: Res<RenderOrigin>,
    refs: Query<(Entity, &FormId, &GlobalTransform, Option<&Collider>)>,
    physics: ReadRapierContext,
    hierarchy: Query<(Option<&FormId>, Option<&Name>, Option<&ChildOf>)>,
    mut exit: MessageWriter<AppExit>,
) {
    if state.done {
        return;
    }
    let now = time.elapsed_secs_f64();
    let started_at = *state.started_at.get_or_insert(now);
    let ready = metrics.resident_cells > 0
        && metrics.loading_cells == 0
        && metrics.active_requests == 0
        && metrics.pending_asset_instances == 0
        && metrics.pending_surface_instances == 0
        && metrics.arming_queue_depth == 0;
    let policy_off = std::env::var_os("MUDCRAB_COLLISION_MEASURE_OFF").is_some();
    let target = refs
        .iter()
        .find(|(_, id, _, collider)| id.0 == state.id && (policy_off || collider.is_some()));
    if let Some(reason) =
        timeout_reason(now - started_at, state.initialized, ready, target.is_some())
    {
        let result = serde_json::json!({"form_id":format!("{:08x}",state.id),"interior":state.interior,"policy_off":policy_off,"passed":false,"reason":reason});
        std::fs::write(&state.output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        exit.write(AppExit::Success);
        state.done = true;
        return;
    }
    if !state.initialized {
        return;
    }
    let Some((entity, _, actual, _)) = target else {
        state.stable_since = None;
        return;
    };
    if !ready {
        state.stable_since = None;
        return;
    }
    let started = *state.stable_since.get_or_insert(now);
    if now - started < 5.0 {
        return;
    }
    let expected_pos = if state.interior {
        creation_to_bevy(state.position)
    } else {
        creation_to_bevy(WorldPosition::from_creation_units(state.position).relative_to(origin.0))
    };
    let expected = Transform::from_translation(expected_pos)
        .with_rotation(creation_rotation_to_bevy(state.rotation))
        .to_matrix();
    let actual = actual.to_matrix();
    let probes = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z];
    let transform_error = probes
        .into_iter()
        .map(|v| (actual.transform_point3(v) - expected.transform_point3(v)).length())
        .fold(0.0_f32, f32::max);
    let vertices: Vec<Vec3> = state
        .vertices
        .iter()
        .map(|v| expected.transform_point3(Vec3::from_array(*v)))
        .collect();
    let min = vertices.iter().copied().reduce(Vec3::min).unwrap();
    let max = vertices.iter().copied().reduce(Vec3::max).unwrap();
    let ctx = physics.single().unwrap();
    let mut matched = 0;
    let mut blocked = 0;
    let mut misses = 0;
    let mut ray_hits = Vec::new();
    for ix in 1..4 {
        for iz in 1..4 {
            let ray = Vec3::new(
                min.x + (max.x - min.x) * ix as f32 / 4.0,
                max.y + 2.0,
                min.z + (max.z - min.z) * iz as f32 / 4.0,
            );
            let expected_distance = state
                .triangles
                .iter()
                .filter_map(|t| {
                    ray_triangle(
                        ray,
                        Vec3::NEG_Y,
                        vertices[t[0] as usize],
                        vertices[t[1] as usize],
                        vertices[t[2] as usize],
                    )
                })
                .min_by(f32::total_cmp);
            if let Some(distance) = expected_distance {
                let target = |candidate| candidate == entity;
                let only = ctx.cast_ray(
                    ray,
                    Vec3::NEG_Y,
                    distance + 1.0,
                    true,
                    QueryFilter::default()
                        .groups(crate::physics::player_collision_groups())
                        .predicate(&target),
                );
                let all = ctx.cast_ray(
                    ray,
                    Vec3::NEG_Y,
                    distance + 1.0,
                    true,
                    QueryFilter::default().groups(crate::physics::player_collision_groups()),
                );
                if only.is_some_and(|(hit, d)| hit == entity && (d - distance).abs() < 0.05) {
                    matched += 1;
                } else {
                    misses += 1;
                }
                if all.is_some_and(|(hit, _)| hit != entity) {
                    blocked += 1;
                }
                let mut identity = None;
                if let Some((hit, _)) = all {
                    let mut current = hit;
                    for _ in 0..16 {
                        let Ok((form, name, parent)) = hierarchy.get(current) else {
                            break;
                        };
                        if let Some(form) = form {
                            identity = Some(format!("form:{:08x}", form.0));
                            break;
                        }
                        if identity.is_none() {
                            identity = name.map(|name| format!("name:{}", name.as_str()));
                        }
                        let Some(parent) = parent else {
                            break;
                        };
                        current = parent.parent();
                    }
                }
                ray_hits.push(serde_json::json!({"ix":ix,"iz":iz,"expected_distance":distance,"identity":identity,"distance":all.map(|(_,distance)|distance),"is_target":all.is_some_and(|(hit,_)|hit==entity)}));
            }
        }
    }
    let result = serde_json::json!({"form_id":format!("{:08x}",state.id),"interior":state.interior,"resident_cells":metrics.resident_cells,"assets_ready":metrics.assets_ready,"asset_failures":metrics.asset_load_failures,"transform_max_error":transform_error,"matched_rays":matched,"mismatched_rays":misses,"rays_blocked_by_other_scene_colliders":blocked,"policy_off":policy_off,"ray_hits":ray_hits,"passed":transform_error<0.05&&(policy_off||matched>=4&&misses==0)&&metrics.asset_load_failures==0});
    std::fs::write(&state.output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    exit.write(AppExit::Success);
    state.done = true;
}

#[cfg(test)]
mod tests {
    use super::timeout_reason;

    #[test]
    fn deadline_identifies_incomplete_probe_state() {
        assert_eq!(timeout_reason(119.9, true, true, false), None);
        assert_eq!(
            timeout_reason(120.0, false, false, false),
            Some("probe initialization did not complete")
        );
        assert_eq!(
            timeout_reason(120.0, true, false, false),
            Some("streaming did not become ready")
        );
        assert_eq!(
            timeout_reason(120.0, true, true, false),
            Some("target reference or required root collider was not found")
        );
        assert_eq!(
            timeout_reason(120.0, true, true, true),
            Some("probe did not stabilize")
        );
    }
}
