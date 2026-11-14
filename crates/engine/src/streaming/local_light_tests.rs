//! Stored local-light inputs through the production database worker and streaming consumer.
use super::tests::{reference_entity, spawn_reference_cell_app};
use super::*;
use crate::world::database::ReferenceRow;

/// Exercise the stored rows through the database worker and real immediate/deferred spawn
/// paths. The procedural-plugin integration tests separately check the exporter inputs.
fn stored_light_reference(update: impl FnOnce(&rusqlite::Connection)) -> ReferenceRow {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("world.db");
    let connection = rusqlite::Connection::open(&path).unwrap();
    converter::esm::exporter::create_tables(&connection).unwrap();
    connection
        .execute_batch(
            "INSERT INTO lights(id,radius,color_r,color_g,color_b,flags,falloff,fade,fade_status)
         VALUES (512,512,255,200,120,0,4,2,1);
         INSERT INTO \"references\"(id,cell_id,base_form_id,is_exterior,
             pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,scale,radius_override,radius_status)
         VALUES (256,99,512,0,100,50,-200,0,0,0,1,-256,1);",
        )
        .unwrap();
    update(&connection);
    drop(connection);
    let database = WorldDatabase::open(&path).unwrap();
    database
        .request(DatabaseRequest::Load {
            generation: 1,
            key: CellKey::Interior(99),
            queued_at: Instant::now(),
        })
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(response) = database.try_response() {
            let mut payload = response.result.unwrap();
            assert_eq!(payload.references.len(), 1);
            return payload.references.remove(0);
        }
        assert!(
            Instant::now() < deadline,
            "database worker did not return cell"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn stored_authored_light_inputs_reach_immediate_and_deferred_point_lights() {
    // Numeric expectations are independent of evaluate_light: base fade 2, signed XRDS
    // -256, and unit-fade compatibility intensity at radius 256. The f32::MAX reference fade is
    // the "no fade" sentinel: it leaves the base fade of 2 in place and is not applied.
    for initially_lit in [true, false] {
        for (offset, expected_fade) in [
            (0.5f32, 2.5f32),
            (-1.0, 1.0),
            (-2.0, 0.0),
            (-3.0, 0.0),
            (f32::MAX, 2.0),
        ] {
            let reference = stored_light_reference(|connection| {
                connection
                    .execute(
                        "INSERT INTO reference_light_data VALUES (256,16,0,?1,0,0)",
                        [offset],
                    )
                    .unwrap();
            });
            let mut app = spawn_reference_cell_app(vec![reference], initially_lit);
            app.add_plugins(crate::lights::LightsPlugin);
            app.world_mut()
                .spawn((StreamingCamera, GlobalTransform::IDENTITY));
            app.update();
            let parent = reference_entity(&app, 256);
            let evaluation = app
                .world()
                .get::<crate::lights::LightEvaluation>(parent)
                .unwrap();
            assert_eq!(evaluation.authored_radius, Some(256.0));
            assert_eq!(evaluation.effective_fade, expected_fade);
            assert_eq!(
                evaluation.authored_fade,
                if offset == f32::MAX {
                    2.0
                } else {
                    2.0 + f64::from(offset)
                }
            );
            assert_eq!(evaluation.fade_sentinel, offset == f32::MAX);
            assert_eq!(evaluation.native_falloff, Some(1.0));
            assert!(!evaluation.unsupported_falloff);
            assert_eq!(evaluation.backend_limited, offset < -2.0);
            if !initially_lit {
                assert!(app.world().get::<Children>(parent).is_none());
                assert!(
                    app.world()
                        .get::<crate::lights::DeferredPointLight>(parent)
                        .is_some()
                );
                app.world_mut().resource_mut::<EngineConfig>().lights = true;
                app.update();
            }
            let children = app.world().get::<Children>(parent).unwrap();
            assert_eq!(children.len(), 1);
            let child = children[0];
            let light = app.world().get::<PointLight>(child).unwrap();
            let expected_intensity = crate::lights::intensity_for_radius(256.0) * expected_fade;
            assert_eq!(light.range, 256.0);
            assert!(
                (f64::from(light.intensity) - f64::from(expected_intensity)).abs()
                    <= f64::from(expected_intensity).abs() * 1e-6
            );
            assert_eq!(light.color, Color::srgb_u8(255, 200, 120));
            let intensity = light.intensity;
            assert_eq!(
                app.world()
                    .get::<crate::lights::SkyrimLight>(child)
                    .unwrap()
                    .form_id,
                256
            );
            assert!(
                app.world()
                    .get::<crate::lights::DeferredPointLight>(parent)
                    .is_none()
            );
            let coverage = app.world().resource::<crate::lights::LightInputCoverage>();
            assert_eq!(coverage.evaluated, 1);
            assert_eq!(
                coverage.reference_fade_applied,
                u64::from(offset != f32::MAX)
            );
            app.world_mut().resource_mut::<EngineConfig>().lights = false;
            app.update();
            assert_eq!(
                *app.world().get::<Visibility>(child).unwrap(),
                Visibility::Hidden
            );
            app.world_mut().resource_mut::<EngineConfig>().lights = true;
            app.update();
            assert_eq!(app.world().get::<Children>(parent).unwrap().len(), 1);
            assert_eq!(
                *app.world().get::<Visibility>(child).unwrap(),
                if expected_fade > 0.0 {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                }
            );
            assert_eq!(
                app.world().get::<PointLight>(child).unwrap().intensity,
                intensity
            );
        }
    }
}

#[test]
fn stored_light_fallbacks_and_invalid_radius_reach_spawn_dispositions() {
    for (sql, range, fade, legacy_fade, invalid_fade, invalid_radius) in [
        (
            "UPDATE lights SET fade=NULL,fade_status=0",
            Some(256.0),
            1.0,
            true,
            false,
            false,
        ),
        (
            "UPDATE lights SET fade=NULL,fade_status=2",
            Some(256.0),
            1.0,
            true,
            true,
            false,
        ),
        (
            "UPDATE lights SET fade=0,fade_status=1",
            Some(256.0),
            0.0,
            false,
            false,
            false,
        ),
        (
            "UPDATE \"references\" SET radius_override=-512",
            Some(0.1),
            2.0,
            false,
            false,
            false,
        ),
        (
            "UPDATE \"references\" SET radius_override=-1024",
            Some(0.1),
            2.0,
            false,
            false,
            false,
        ),
        (
            "UPDATE \"references\" SET radius_override=NULL,radius_status=0",
            Some(512.0),
            2.0,
            false,
            false,
            false,
        ),
        (
            "UPDATE \"references\" SET radius_override=NULL,radius_status=2",
            None,
            2.0,
            false,
            false,
            true,
        ),
        (
            "DROP TABLE reference_light_data;
          ALTER TABLE lights DROP COLUMN fade;
          ALTER TABLE lights DROP COLUMN falloff;
          ALTER TABLE lights DROP COLUMN fade_status;
          ALTER TABLE \"references\" DROP COLUMN radius_status",
            Some(256.0),
            1.0,
            true,
            false,
            false,
        ),
    ] {
        for initially_lit in [true, false] {
            let reference =
                stored_light_reference(|connection| connection.execute_batch(sql).unwrap());
            let mut app = spawn_reference_cell_app(vec![reference], initially_lit);
            app.add_plugins(crate::lights::LightsPlugin);
            app.world_mut()
                .spawn((StreamingCamera, GlobalTransform::IDENTITY));
            app.world_mut().resource_mut::<EngineConfig>().lights = true;
            app.update();
            let parent = reference_entity(&app, 256);
            let evaluation = app
                .world()
                .get::<crate::lights::LightEvaluation>(parent)
                .unwrap();
            assert_eq!(evaluation.legacy_fade, legacy_fade, "{sql}");
            assert_eq!(evaluation.invalid_fade, invalid_fade, "{sql}");
            assert_eq!(evaluation.invalid_radius, invalid_radius, "{sql}");
            assert_eq!(
                evaluation.native_radius_fallback,
                range == Some(0.1),
                "{sql}"
            );
            let lights: Vec<_> = app
                .world_mut()
                .query::<&PointLight>()
                .iter(app.world())
                .copied()
                .collect();
            if let Some(range) = range {
                assert_eq!(lights.len(), 1, "{sql}");
                assert_eq!(lights[0].range, range, "{sql}");
                let expected = crate::lights::intensity_for_radius(range) * fade;
                assert!(
                    (lights[0].intensity - expected).abs() <= expected.abs() * 1e-6,
                    "{sql}"
                );
            } else {
                assert!(
                    lights.is_empty(),
                    "malformed XRDS must not use base radius: {sql}"
                );
                assert!(
                    app.world()
                        .get::<crate::lights::DeferredPointLight>(parent)
                        .is_none()
                );
            }
            let coverage = app.world().resource::<crate::lights::LightInputCoverage>();
            assert_eq!(coverage.evaluated, 1);
            assert_eq!(coverage.invalid_radius, u64::from(invalid_radius));
        }
    }
}

#[test]
fn malformed_xlig_keeps_valid_fade_and_reports_rejected_fields_at_spawn() {
    for (size, offset, expected_fade, invalid_reference_fade) in
        [(15, None, 2.0, true), (16, Some(0.5f32), 2.5, false)]
    {
        for initially_lit in [true, false] {
            let reference = stored_light_reference(|connection| {
                connection
                    .execute(
                        "INSERT INTO reference_light_data VALUES (256,?1,NULL,?2,0,0)",
                        rusqlite::params![size, offset],
                    )
                    .unwrap();
            });
            let mut app = spawn_reference_cell_app(vec![reference], initially_lit);
            app.add_plugins(crate::lights::LightsPlugin);
            app.world_mut().resource_mut::<EngineConfig>().lights = true;
            app.update();
            let parent = reference_entity(&app, 256);
            let evaluation = app
                .world()
                .get::<crate::lights::LightEvaluation>(parent)
                .unwrap();
            assert!(evaluation.malformed_reference_data);
            assert!(evaluation.unsupported_reference_data);
            assert_eq!(evaluation.invalid_reference_fade, invalid_reference_fade);
            assert_eq!(evaluation.reference_fade_applied, !invalid_reference_fade);
            let child = app.world().get::<Children>(parent).unwrap()[0];
            let light = app.world().get::<PointLight>(child).unwrap();
            assert_eq!(light.range, 256.0);
            let expected = crate::lights::intensity_for_radius(256.0) * expected_fade;
            assert!((light.intensity - expected).abs() < expected * 1e-6);
            let coverage = app.world().resource::<crate::lights::LightInputCoverage>();
            assert_eq!(coverage.malformed_reference_data, 1);
            assert_eq!(
                coverage.invalid_reference_fade,
                u64::from(invalid_reference_fade)
            );
        }
    }
}
