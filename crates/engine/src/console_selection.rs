//! Console-only picking of resident mesh geometry. Identity belongs to the placed reference,
//! not to a glTF node, mesh asset, base record, or display name.

use bevy::{
    camera::{
        RenderTarget,
        primitives::Aabb,
        visibility::{RenderLayers, VisibilitySystems},
    },
    gizmos::{AppGizmoBuilder, config::GizmoConfigGroup},
    math::bounding::Aabb3d,
    picking::mesh_picking::ray_cast::{MeshRayCast, MeshRayCastSettings},
    prelude::*,
    transform::TransformSystems,
    ui::UiSystems,
    window::PrimaryWindow,
};

use crate::{
    console::{ConsoleRoot, ConsoleState},
    render::PLACED_OBJECT_RENDER_LAYERS,
    settings_ui::SettingsMenuState,
    world::components::{CellRef, FormId, InstanceBounds, StreamingCamera},
};

pub(crate) const SELECTION_HINT: &str = "Click an object above the console to select its reference";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectedReference {
    entity: Entity,
    reference_id: u32,
    cell_id: u32,
}

#[derive(Resource, Default, PartialEq)]
struct ConsoleSelection(Option<SelectedReference>);

#[derive(Component)]
pub(crate) struct ConsoleSelectionText;

#[derive(Default, Reflect, GizmoConfigGroup)]
struct SelectionGizmos;

pub(crate) struct ConsoleSelectionPlugin;

impl Plugin for ConsoleSelectionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConsoleSelection>()
            .insert_gizmo_config(
                SelectionGizmos,
                GizmoConfig {
                    render_layers: PLACED_OBJECT_RENDER_LAYERS,
                    depth_bias: -0.001,
                    ..default()
                },
            )
            .add_systems(
                PostUpdate,
                (select_reference, draw_selection, refresh_selection_text)
                    .chain()
                    .after(TransformSystems::Propagate)
                    .after(VisibilitySystems::CheckVisibility)
                    .after(VisibilitySystems::MarkNewlyHiddenEntitiesInvisible)
                    .after(UiSystems::Layout),
            );
    }
}

fn reference_for_mesh(
    mesh: Entity,
    parents: &Query<&ChildOf>,
    references: &Query<(&FormId, &CellRef)>,
) -> Option<SelectedReference> {
    std::iter::once(mesh)
        .chain(parents.iter_ancestors(mesh))
        .find_map(|entity| {
            let (id, cell) = references.get(entity).ok()?;
            Some(SelectedReference {
                entity,
                reference_id: id.0,
                cell_id: cell.0,
            })
        })
}

fn selection_is_live(selected: SelectedReference, references: &Query<(&FormId, &CellRef)>) -> bool {
    references
        .get(selected.entity)
        .is_ok_and(|(id, cell)| id.0 == selected.reference_id && cell.0 == selected.cell_id)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn select_reference(
    state: Res<ConsoleState>,
    menu: Option<Res<SettingsMenuState>>,
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<(Entity, &Window), With<PrimaryWindow>>,
    cameras: Query<
        (
            &Camera,
            &GlobalTransform,
            &RenderTarget,
            Option<&RenderLayers>,
        ),
        With<StreamingCamera>,
    >,
    overlay: Query<(&ComputedNode, &UiGlobalTransform), With<ConsoleRoot>>,
    meshes: Query<
        (
            &GlobalTransform,
            Option<&RenderLayers>,
            &InheritedVisibility,
        ),
        With<Mesh3d>,
    >,
    parents: Query<&ChildOf>,
    references: Query<(&FormId, &CellRef)>,
    mut raycast: MeshRayCast,
    mut selection: ResMut<ConsoleSelection>,
    mut was_open: Local<bool>,
) {
    let opening = state.open && !*was_open;
    *was_open = state.open;
    // Entity generations prevent a later reference from inheriting a despawned selection.
    if !state.open
        || selection
            .0
            .is_some_and(|s| !selection_is_live(s, &references))
    {
        selection.set_if_neq(ConsoleSelection(None));
    }
    if !state.open
        || opening
        || state.closing
        || state.capture_suppressed
        || menu.is_some_and(|menu| menu.blocks_gameplay())
        || !buttons.just_pressed(MouseButton::Left)
    {
        return;
    }
    let Ok((window_entity, window)) = windows.single() else {
        return;
    };
    if !window.focused {
        return;
    }
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let Ok((camera, camera_transform, target, layers)) = cameras.single() else {
        return;
    };
    let RenderTarget::Window(target_window) = target else {
        return;
    };
    if !camera.is_active
        || target_window
            .normalize(Some(window_entity))
            .is_none_or(|w| w.entity() != window_entity)
        || camera
            .logical_viewport_rect()
            .is_none_or(|rect| !rect.contains(cursor))
    {
        return;
    }
    // Match Bevy UI's physical, viewport-relative hit coordinates, including HiDPI and UiScale.
    // No computed overlay yet means no picking: the opening/layout frame must fail closed.
    let Ok((node, transform)) = overlay.single() else {
        return;
    };
    let Some(physical_cursor) = window.physical_cursor_position() else {
        return;
    };
    let viewport_origin = camera
        .physical_viewport_rect()
        .map_or(Vec2::ZERO, |rect| rect.min.as_vec2());
    if node.size().min_element() <= 0.0
        || node.contains_point(*transform, physical_cursor - viewport_origin)
    {
        return;
    }
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else {
        return;
    };
    let camera_layers = layers.cloned().unwrap_or_default();
    let filter = |entity| {
        meshes
            .get(entity)
            .is_ok_and(|(transform, layers, visibility)| {
                let affine = transform.affine();
                let determinant = affine.matrix3.determinant();
                visibility.get()
                    && affine.is_finite()
                    && determinant.is_finite()
                    && determinant != 0.0
                    && camera_layers.intersects(&layers.cloned().unwrap_or_default())
            })
    };
    // Include non-reference geometry in the nearest-hit test: terrain and other visible meshes
    // must occlude references. Picking only reference meshes would select through walls/ground.
    let picked = raycast
        .cast_ray(ray, &MeshRayCastSettings::default().with_filter(&filter))
        .first()
        .and_then(|(mesh, _)| reference_for_mesh(*mesh, &parents, &references));
    selection.set_if_neq(ConsoleSelection(picked));
}

fn selection_bounds(
    selected: SelectedReference,
    children: &Query<&Children>,
    meshes: &Query<(&Aabb, &GlobalTransform, &InheritedVisibility), With<Mesh3d>>,
) -> Option<InstanceBounds> {
    let mut combined: Option<InstanceBounds> = None;
    for entity in std::iter::once(selected.entity).chain(children.iter_descendants(selected.entity))
    {
        let Ok((bounds, transform, visibility)) = meshes.get(entity) else {
            continue;
        };
        if !visibility.get() || !transform.affine().is_finite() {
            continue;
        }
        let bounds = InstanceBounds::transformed(
            bounds.min().into(),
            bounds.max().into(),
            transform.to_matrix(),
        );
        if !bounds.min.is_finite() || !bounds.max.is_finite() {
            continue;
        }
        if let Some(combined) = &mut combined {
            combined.min = combined.min.min(bounds.min);
            combined.max = combined.max.max(bounds.max);
        } else {
            combined = Some(bounds);
        }
    }
    combined
}

fn draw_selection(
    state: Res<ConsoleState>,
    selection: Res<ConsoleSelection>,
    children: Query<&Children>,
    meshes: Query<(&Aabb, &GlobalTransform, &InheritedVisibility), With<Mesh3d>>,
    mut gizmos: Gizmos<SelectionGizmos>,
) {
    if !state.open || state.closing || state.capture_suppressed {
        return;
    }
    let Some(bounds) = selection
        .0
        .and_then(|s| selection_bounds(s, &children, &meshes))
    else {
        return;
    };
    // Recompute from current mesh transforms, so rebasing and moving clutter cannot leave a box
    // behind. Immediate gizmos need no material restoration or entity cleanup on cell unload.
    gizmos.aabb_3d(
        Aabb3d::from_min_max(bounds.min, bounds.max),
        Transform::IDENTITY,
        Color::srgb(1.0, 0.9, 0.2),
    );
}

fn refresh_selection_text(
    selection: Res<ConsoleSelection>,
    mut labels: Query<&mut Text, With<ConsoleSelectionText>>,
) {
    if !selection.is_changed() {
        return;
    }
    let label = selection.0.map_or_else(
        || SELECTION_HINT.to_owned(),
        |s| format!("Selected reference: {:08X}", s.reference_id),
    );
    for mut text in &mut labels {
        text.set_if_neq(Text::new(&label));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        console::{AppConsoleExt, ConsoleCommand, ConsolePlugin},
        physics::CursorCapture,
    };
    use bevy::{
        asset::RenderAssetUsages,
        camera::{ComputedCameraValues, RenderTargetInfo, primitives::MeshAabb},
        ecs::system::RunSystemOnce,
        gizmos::{GizmoAsset, GizmoHandles},
        input::{
            ButtonState,
            keyboard::{Key, KeyboardInput},
            mouse::MouseButtonInput,
        },
        mesh::PrimitiveTopology,
        time::TimeUpdateStrategy,
    };
    use std::{any::TypeId, time::Duration};

    struct Fixture {
        app: App,
        window: Entity,
        camera: Entity,
        reference: Entity,
        mesh: Entity,
    }

    fn fixture() -> Fixture {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, ConsolePlugin))
            .add_console_command(ConsoleCommand {
                name: "ToggleCollision",
                short: Some("tcl"),
                usage: "ToggleCollision",
                help: "test command",
                handler: Box::new(|_, _| Ok(String::new())),
            })
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<GizmoAsset>>()
            .insert_resource(CursorCapture::Captured)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                20,
            )))
            .add_plugins(ConsoleSelectionPlugin);
        let window = app
            .world_mut()
            .spawn((
                Window {
                    resolution: (800, 600).into(),
                    focused: true,
                    ..default()
                },
                PrimaryWindow,
            ))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                StreamingCamera,
                Camera {
                    computed: ComputedCameraValues {
                        clip_from_view: Mat4::perspective_infinite_reverse_rh(
                            std::f32::consts::FRAC_PI_2,
                            4.0 / 3.0,
                            0.1,
                        ),
                        target_info: Some(RenderTargetInfo {
                            physical_size: UVec2::new(800, 600),
                            scale_factor: 1.0,
                        }),
                        ..default()
                    },
                    ..default()
                },
                GlobalTransform::IDENTITY,
                RenderLayers::from_layers(crate::render::MAIN_VIEW_LAYERS),
            ))
            .id();
        let reference = app
            .world_mut()
            .spawn((
                FormId(0x000F9907),
                CellRef(12),
                Name::new("This is not a FormID"),
            ))
            .id();
        let scene = app.world_mut().spawn(ChildOf(reference)).id();
        let node = app.world_mut().spawn(ChildOf(scene)).id();
        let mesh = spawn_mesh(&mut app, Cuboid::new(4.0, 4.0, 4.0).into(), -6.0);
        app.world_mut().entity_mut(mesh).insert(ChildOf(node));
        app.update();
        // CPU fixture supplies layout/visibility/camera values; no renderer or window backend.
        let root = app
            .world_mut()
            .query_filtered::<Entity, With<ConsoleRoot>>()
            .single(app.world())
            .unwrap();
        app.world_mut().entity_mut(root).insert((
            ComputedNode {
                size: Vec2::new(800.0, 240.0),
                ..default()
            },
            UiGlobalTransform::from_xy(400.0, 480.0),
        ));
        Fixture {
            app,
            window,
            camera,
            reference,
            mesh,
        }
    }

    fn spawn_mesh(app: &mut App, mesh: Mesh, z: f32) -> Entity {
        let bounds = mesh.compute_aabb().unwrap();
        let handle = app.world_mut().resource_mut::<Assets<Mesh>>().add(mesh);
        app.world_mut()
            .spawn((
                Mesh3d(handle),
                bounds,
                GlobalTransform::from_xyz(0.0, 0.0, z),
                InheritedVisibility::VISIBLE,
                ViewVisibility::VISIBLE,
                PLACED_OBJECT_RENDER_LAYERS,
            ))
            .id()
    }

    fn key(f: &mut Fixture, code: KeyCode, state: ButtonState) {
        f.app.world_mut().write_message(KeyboardInput {
            key_code: code,
            logical_key: Key::Character("`".into()),
            state,
            text: None,
            repeat: false,
            window: f.window,
        });
    }

    fn toggle(f: &mut Fixture) {
        key(f, KeyCode::Backquote, ButtonState::Pressed);
        f.app.update();
        key(f, KeyCode::Backquote, ButtonState::Released);
        f.app.update();
    }

    fn mouse(f: &mut Fixture, point: Vec2, state: ButtonState) {
        f.app
            .world_mut()
            .get_mut::<Window>(f.window)
            .unwrap()
            .set_cursor_position(Some(point));
        f.app.world_mut().write_message(MouseButtonInput {
            button: MouseButton::Left,
            state,
            window: f.window,
        });
    }

    fn click(f: &mut Fixture, point: Vec2) {
        mouse(f, point, ButtonState::Pressed);
        f.app.update();
        mouse(f, point, ButtonState::Released);
        f.app.update();
    }

    fn selected(f: &Fixture) -> Option<SelectedReference> {
        f.app.world().resource::<ConsoleSelection>().0
    }

    fn highlight(f: &Fixture) -> Option<&GizmoAsset> {
        let handles = f.app.world().resource::<GizmoHandles>();
        let handle = handles
            .handles()
            .get(&TypeId::of::<SelectionGizmos>())?
            .as_ref()?;
        f.app.world().resource::<Assets<GizmoAsset>>().get(handle)
    }

    #[test]
    fn nested_scene_mesh_selects_reference_and_draws_current_bounds() {
        let mut f = fixture();
        toggle(&mut f);
        f.app.world_mut().resource_mut::<ConsoleState>().buffer = "tog".into();
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(
            selected(&f),
            Some(SelectedReference {
                entity: f.reference,
                reference_id: 0x000F9907,
                cell_id: 12,
            })
        );
        let text = f
            .app
            .world_mut()
            .query_filtered::<&Text, With<ConsoleSelectionText>>()
            .single(f.app.world())
            .unwrap();
        assert_eq!(text.0, "Selected reference: 000F9907");
        let box_mesh = highlight(&f).expect("selection must draw a highlight");
        assert!(!box_mesh.buffer().strip_positions.is_empty());
        assert!(
            box_mesh
                .buffer()
                .strip_positions
                .iter()
                .filter(|p| p.is_finite())
                .all(|p| p.x.abs() == 2.0 && p.y.abs() == 2.0 && [-8.0, -4.0].contains(&p.z))
        );
        assert!(f.app.world().resource::<Time<Virtual>>().is_paused());
        assert_eq!(
            *f.app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert_eq!(f.app.world().resource::<ConsoleState>().buffer, "tog");
        // Selection leaves command completion on the original keyboard path.
        key(&mut f, KeyCode::Tab, ButtonState::Pressed);
        f.app.update();
        assert_eq!(
            f.app.world().resource::<ConsoleState>().buffer,
            "ToggleCollision "
        );
        // Move the mesh as rebasing or clutter physics would; the box must follow immediately.
        f.app
            .world_mut()
            .get_mut::<GlobalTransform>(f.mesh)
            .unwrap()
            .clone_from(&GlobalTransform::from_xyz(100.0, 0.0, -6.0));
        f.app.update();
        assert!(
            highlight(&f)
                .unwrap()
                .buffer()
                .strip_positions
                .iter()
                .filter(|p| p.is_finite())
                .all(|p| [98.0, 102.0].contains(&p.x))
        );
    }

    #[test]
    fn nearest_mesh_wins_and_non_reference_geometry_occludes() {
        let mut f = fixture();
        let near = spawn_mesh(&mut f.app, Cuboid::new(2.0, 2.0, 2.0).into(), -3.0);
        // Reuse the same underlying mesh asset in both references: identity must be per instance.
        let shared_mesh = f.app.world().get::<Mesh3d>(f.mesh).unwrap().clone();
        f.app.world_mut().entity_mut(near).insert((
            shared_mesh,
            Aabb::from_min_max(Vec3::splat(-2.0), Vec3::splat(2.0)),
            GlobalTransform::from(Transform::from_xyz(0.0, 0.0, -3.0).with_scale(Vec3::splat(0.5))),
        ));
        toggle(&mut f);
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(
            selected(&f),
            None,
            "non-reference mesh blocks the farther reference"
        );
        let near_root = f.app.world_mut().spawn((FormId(0x123), CellRef(12))).id();
        f.app
            .world_mut()
            .entity_mut(near)
            .insert(ChildOf(near_root));
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f).unwrap().entity, near_root);
        f.app
            .world_mut()
            .entity_mut(near)
            .insert(InheritedVisibility::HIDDEN);
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f).unwrap().entity, f.reference);
        f.app
            .world_mut()
            .entity_mut(near)
            .insert((InheritedVisibility::VISIBLE, RenderLayers::layer(31)));
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f).unwrap().entity, f.reference);
    }

    #[test]
    fn bounding_box_overlap_without_triangle_hit_does_not_block() {
        let mut f = fixture();
        let triangle = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        )
        .with_inserted_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![[-3.0, -3.0, 0.0], [3.0, -3.0, 0.0], [-3.0, 3.0, 0.0]],
        );
        spawn_mesh(&mut f.app, triangle, -3.0);
        toggle(&mut f);
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f).unwrap().entity, f.reference);
    }

    #[test]
    fn overlay_click_preserves_selection_and_empty_click_clears_it() {
        let mut f = fixture();
        toggle(&mut f);
        click(&mut f, Vec2::new(400.0, 240.0));
        let reference = selected(&f).unwrap();
        click(&mut f, Vec2::new(400.0, 420.0));
        assert_eq!(selected(&f), Some(reference));
        click(&mut f, Vec2::new(20.0, 20.0));
        assert_eq!(selected(&f), None);
        assert!(highlight(&f).is_none());
    }

    #[test]
    fn unload_and_identity_removal_clear_selection_without_another_click() {
        for mutation in ["despawn", "remove", "reference ID", "cell ID"] {
            let mut f = fixture();
            toggle(&mut f);
            click(&mut f, Vec2::new(400.0, 240.0));
            assert!(selected(&f).is_some());
            match mutation {
                "despawn" => {
                    f.app.world_mut().despawn(f.reference);
                    f.app.world_mut().spawn((FormId(0x000F9907), CellRef(12)));
                }
                "remove" => {
                    f.app.world_mut().entity_mut(f.reference).remove::<FormId>();
                }
                "reference ID" => {
                    f.app
                        .world_mut()
                        .entity_mut(f.reference)
                        .insert(FormId(0x123));
                }
                "cell ID" => {
                    f.app
                        .world_mut()
                        .entity_mut(f.reference)
                        .insert(CellRef(13));
                }
                _ => unreachable!(),
            }
            f.app.update();
            assert_eq!(selected(&f), None);
            assert!(highlight(&f).is_none());
        }
    }

    #[test]
    fn opening_and_closing_clicks_do_not_select_or_recapture() {
        let mut f = fixture();
        key(&mut f, KeyCode::Backquote, ButtonState::Pressed);
        mouse(&mut f, Vec2::new(400.0, 240.0), ButtonState::Pressed);
        f.app.update();
        assert_eq!(selected(&f), None);
        key(&mut f, KeyCode::Backquote, ButtonState::Released);
        mouse(&mut f, Vec2::new(400.0, 240.0), ButtonState::Released);
        f.app.update();
        click(&mut f, Vec2::new(400.0, 240.0));
        assert!(selected(&f).is_some());
        key(&mut f, KeyCode::Backquote, ButtonState::Pressed);
        mouse(&mut f, Vec2::new(400.0, 240.0), ButtonState::Pressed);
        f.app.update();
        assert_eq!(selected(&f), None);
        assert!(highlight(&f).is_none());
        assert!(!f.app.world().resource::<Time<Virtual>>().is_paused());
        assert_eq!(
            *f.app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        f.app.update();
        assert_eq!(
            *f.app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
    }

    #[test]
    fn report_suppression_menu_and_focus_loss_block_picking() {
        let mut f = fixture();
        toggle(&mut f);
        f.app
            .world_mut()
            .resource_mut::<ConsoleState>()
            .capture_suppressed = true;
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f), None);
        f.app
            .world_mut()
            .resource_mut::<ConsoleState>()
            .capture_suppressed = false;
        f.app.init_resource::<SettingsMenuState>();
        f.app.world_mut().resource_mut::<SettingsMenuState>().open = true;
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f), None);
        f.app.world_mut().resource_mut::<SettingsMenuState>().open = false;
        f.app
            .world_mut()
            .get_mut::<Window>(f.window)
            .unwrap()
            .focused = false;
        click(&mut f, Vec2::new(400.0, 240.0));
        assert_eq!(selected(&f), None);
    }

    #[test]
    fn hidpi_overlay_and_inactive_or_offscreen_cameras_do_not_pick_through_ui() {
        let mut f = fixture();
        toggle(&mut f);
        f.app
            .world_mut()
            .get_mut::<Window>(f.window)
            .unwrap()
            .resolution
            .set_scale_factor_override(Some(2.0));
        f.app
            .world_mut()
            .get_mut::<Camera>(f.camera)
            .unwrap()
            .computed
            .target_info
            .as_mut()
            .unwrap()
            .scale_factor = 2.0;
        click(&mut f, Vec2::new(200.0, 210.0));
        assert_eq!(selected(&f), None, "physical cursor lies over console");
        click(&mut f, Vec2::new(200.0, 120.0));
        assert!(selected(&f).is_some());
        f.app.world_mut().resource_mut::<ConsoleSelection>().0 = None;
        f.app
            .world_mut()
            .get_mut::<Camera>(f.camera)
            .unwrap()
            .is_active = false;
        click(&mut f, Vec2::new(200.0, 120.0));
        assert_eq!(selected(&f), None);
        f.app
            .world_mut()
            .get_mut::<Camera>(f.camera)
            .unwrap()
            .is_active = true;
        f.app
            .world_mut()
            .entity_mut(f.camera)
            .insert(RenderTarget::None {
                size: UVec2::new(800, 600),
            });
        click(&mut f, Vec2::new(200.0, 120.0));
        assert_eq!(selected(&f), None);
    }

    #[test]
    fn bounds_include_all_visible_parts_and_ignore_other_references() {
        let mut f = fixture();
        let part = spawn_mesh(&mut f.app, Cuboid::new(2.0, 2.0, 2.0).into(), -6.0);
        f.app.world_mut().entity_mut(part).insert((
            ChildOf(f.reference),
            GlobalTransform::from_xyz(10.0, 0.0, -6.0),
        ));
        spawn_mesh(&mut f.app, Cuboid::new(100.0, 100.0, 100.0).into(), -200.0);
        let root = f.reference;
        let bounds = f
            .app
            .world_mut()
            .run_system_once(
                move |children: Query<&Children>,
                      meshes: Query<
                    (&Aabb, &GlobalTransform, &InheritedVisibility),
                    With<Mesh3d>,
                >| {
                    selection_bounds(
                        SelectedReference {
                            entity: root,
                            reference_id: 0x000F9907,
                            cell_id: 12,
                        },
                        &children,
                        &meshes,
                    )
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(bounds.min, Vec3::new(-2.0, -2.0, -8.0));
        assert_eq!(bounds.max, Vec3::new(11.0, 2.0, -4.0));
    }
}
