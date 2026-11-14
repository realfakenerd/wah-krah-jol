//! Procedural LIGH/REFR -> exporter -> stored SQLite -> streaming database worker ->
//! the PointLight consumer. No GPU, installed game, environment resolver or asset conversion.
use converter::esm::{
    binary::parse_plugin_file,
    exporter::{create_tables, export_to_db},
    records::RawRecord,
};
use dummy_content::{
    esm::{self, Plugin},
    layout,
};
use engine::{
    lights::{LightEvaluation, evaluate_light},
    world::database::{CellKey, DatabaseRequest, WorldDatabase},
};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    fs, thread,
    time::{Duration, Instant},
};

fn records(fade: f32, falloff: f32, offset: f32) -> HashMap<u32, RawRecord> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Generated.esm");
    let spec = Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    };
    let light = esm::Light {
        fade,
        falloff,
        radius: 512,
        radius_override: offset,
        model_path: None,
        ..esm::PRESET_LIGHT
    };
    fs::write(&path, esm::plugin_with_lights(&spec, &light).unwrap()).unwrap();
    parse_plugin_file(&path)
        .unwrap()
        .into_iter()
        .map(|record| (record.form_id, record))
        .collect()
}

fn light_record(records: &mut HashMap<u32, RawRecord>) -> &mut RawRecord {
    records
        .values_mut()
        .find(|r| r.record_type == *b"LIGH")
        .unwrap()
}

fn light_reference(records: &mut HashMap<u32, RawRecord>) -> &mut RawRecord {
    let base = light_record(records).form_id;
    records
        .values_mut()
        .find(|r| {
            r.record_type == *b"REFR"
                && r.subrecords
                    .iter()
                    .any(|(tag, data)| tag == b"NAME" && data.as_slice() == base.to_le_bytes())
        })
        .unwrap()
}

fn consume(records: &HashMap<u32, RawRecord>, legacy: bool) -> LightEvaluation {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("world.db");
    let connection = Connection::open(&path).unwrap();
    create_tables(&connection).unwrap();
    export_to_db(&connection, records).unwrap();
    if legacy {
        connection
            .execute_batch(
                "DROP TABLE reference_light_data;
            ALTER TABLE lights DROP COLUMN fade;
            ALTER TABLE lights DROP COLUMN falloff;
            ALTER TABLE lights DROP COLUMN fade_status;
            ALTER TABLE \"references\" DROP COLUMN radius_status;",
            )
            .unwrap();
    }
    let cell = records
        .values()
        .find(|r| r.record_type == *b"CELL")
        .unwrap();
    let key = CellKey::Exterior {
        worldspace_id: cell.worldspace_form_id.unwrap(),
        grid_x: esm::PRESET_EXTERIOR_CELL.grid_x,
        grid_y: esm::PRESET_EXTERIOR_CELL.grid_y,
    };
    drop(connection);
    let database = WorldDatabase::open(&path).unwrap();
    database
        .request(DatabaseRequest::Load {
            generation: 1,
            key,
            queued_at: Instant::now(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(response) = database.try_response() {
            let payload = response.result.unwrap();
            let reference = payload
                .references
                .iter()
                .find(|r| r.light.is_some())
                .unwrap();
            return evaluate_light(
                reference.light.as_ref().unwrap(),
                reference.light_radius_override,
            );
        }
        assert!(
            Instant::now() < deadline,
            "database worker did not return cell"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn authored_fade_changes_actual_light_power_without_changing_range_or_colour() {
    let unit = consume(&records(1.0, 1.0, -256.0), false)
        .point_light()
        .unwrap();
    assert_eq!(unit.range, 256.0);
    for fade in [0.0, 0.25, 2.0, 12.0] {
        let evaluation = consume(&records(fade, 1.0, -256.0), false);
        assert!(!evaluation.legacy_fade);
        let actual = evaluation.point_light().unwrap();
        assert!((actual.intensity / unit.intensity - fade).abs() < 1e-5);
        assert_eq!(actual.range, unit.range);
        assert_eq!(actual.color, unit.color);
    }
}

#[test]
fn signed_offsets_zero_and_backend_limits_survive_storage() {
    // (offset, backend range, authored radius, backend-limited). A sum above the cap clamps; the
    // base radius is 512. An offset above the cap (retail outliers) is ignored: base radius stays.
    for (offset, expected, authored, limited) in [
        (-400.0, 112.0, 112.0, false),
        (0.0, 512.0, 512.0, false),
        (100.0, 612.0, 612.0, false),
        (7_000.0, 7_512.0, 7_512.0, false),
        (8_000.0, 8192.0, 8_512.0, true),
        (20_000.0, 512.0, 512.0, true),
        (3_736_737.5, 512.0, 512.0, true),
    ] {
        let evaluated = consume(&records(1.0, 1.0, offset), false);
        assert_eq!(evaluated.authored_radius, Some(authored), "offset {offset}");
        assert_eq!(
            evaluated.point_light().unwrap().range,
            expected,
            "offset {offset}"
        );
        assert_eq!(evaluated.backend_limited, limited, "offset {offset}");
        assert_eq!(
            evaluated.radius_offset_ignored,
            offset > 8192.0,
            "offset {offset}"
        );
    }
    for offset in [-512.0, -1024.0, f32::NAN, f32::INFINITY] {
        // The procedural writer rejects non-finite radii. Inject malformed raw fields after
        // parsing, as the malformed FNAM cases do, so they reach storage and the consumer.
        let mut source = records(1.0, 1.0, if offset.is_finite() { offset } else { 0.0 });
        if !offset.is_finite() {
            let field = light_reference(&mut source)
                .subrecords
                .iter_mut()
                .find(|(tag, _)| tag == b"XRDS")
                .unwrap();
            field.1 = offset.to_le_bytes().to_vec();
        }
        let evaluated = consume(&source, false);
        if offset.is_finite() {
            assert_eq!(evaluated.point_light().unwrap().range, 0.1);
            assert!(evaluated.native_radius_fallback);
        } else {
            assert!(evaluated.point_light().is_none());
        }
        assert_eq!(evaluated.invalid_radius, !offset.is_finite());
    }
}

#[test]
fn absent_malformed_negative_and_nonfinite_fade_remain_distinct_from_valid_zero() {
    for bytes in [
        None,
        Some(vec![1, 2]),
        Some((-1.0f32).to_le_bytes().to_vec()),
        Some(f32::NAN.to_le_bytes().to_vec()),
        Some(f32::INFINITY.to_le_bytes().to_vec()),
    ] {
        let mut source = records(1.0, 1.0, 0.0);
        let light = light_record(&mut source);
        light.subrecords.retain(|(tag, _)| tag != b"FNAM");
        let malformed = bytes.is_some();
        if let Some(bytes) = bytes {
            light.subrecords.push((b"FNAM".to_vec(), bytes));
        }
        let actual = consume(&source, false);
        assert!(actual.legacy_fade);
        assert_eq!(actual.invalid_fade, malformed);
        assert_eq!(actual.effective_fade, 1.0);
    }
    let zero = consume(&records(0.0, 1.0, 0.0), false);
    assert!(!zero.legacy_fade);
    assert_eq!(zero.point_light().unwrap().intensity, 0.0);
}

#[test]
fn native_additive_xlig_fade_and_nonshadow_falloff_dispatch_survive_storage() {
    let baseline = consume(&records(1.0, 1.0, 0.0), false)
        .point_light()
        .unwrap();
    for fade_offset in [0.0f32, 0.5, 2.0, -1.0] {
        let mut source = records(1.0, 4.0, 0.0);
        let bytes = [0.0f32, fade_offset, 0.0, 0.125]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        light_reference(&mut source)
            .subrecords
            .push((b"XLIG".to_vec(), bytes));
        let actual = consume(&source, false);
        assert!(actual.unsupported_reference_data);
        assert!(!actual.malformed_reference_data);
        assert_eq!(
            actual.source.reference_data.as_ref().unwrap().fade_offset,
            Some(fade_offset)
        );
        assert_eq!(actual.source.falloff, Some(4.0));
        assert!(!actual.unsupported_falloff);
        assert_eq!(actual.native_falloff, Some(1.0));
        assert!(actual.reference_fade_applied);
        assert_eq!(actual.authored_fade, 1.0 + f64::from(fade_offset));
        let expected = (1.0 + fade_offset).max(0.0);
        assert!(
            (actual.point_light().unwrap().intensity / baseline.intensity - expected).abs() < 1e-5
        );
    }
    for bytes in [
        vec![0; 15],
        vec![0; 20],
        [f32::NAN; 4]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect(),
    ] {
        let mut source = records(1.0, 0.0, 0.0);
        let malformed = bytes.len() != 20;
        light_reference(&mut source)
            .subrecords
            .push((b"XLIG".to_vec(), bytes));
        let actual = consume(&source, false);
        assert_eq!(actual.malformed_reference_data, malformed);
        assert_eq!(actual.unsupported_reference_data, malformed);
        assert_eq!(actual.invalid_reference_fade, malformed);
        assert_eq!(actual.reference_fade_applied, !malformed);
        assert_eq!(actual.source.falloff, Some(0.0));
    }
}

#[test]
fn older_packages_use_labelled_fallbacks_and_reexport_clears_removed_xlig() {
    let mut source = records(2.0, 1.0, 0.0);
    let actual = consume(&source, true);
    assert!(actual.legacy_fade);
    assert_eq!(actual.source.fade, None);
    assert_eq!(actual.source.falloff, None);
    assert_eq!(actual.source.reference_data, None);
    let connection = Connection::open_in_memory().unwrap();
    create_tables(&connection).unwrap();
    light_reference(&mut source)
        .subrecords
        .push((b"XLIG".to_vec(), vec![0; 16]));
    export_to_db(&connection, &source).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM reference_light_data", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    light_reference(&mut source)
        .subrecords
        .retain(|(tag, _)| tag != b"XLIG");
    export_to_db(&connection, &source).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM reference_light_data", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn backend_saturates_extreme_fade_and_consumes_authored_flags() {
    // A finite fade far above any authored value saturates the backend instead of overflowing.
    let extreme = consume(&records(3.0e38, 1.0, 0.0), false);
    assert!(extreme.backend_limited && !extreme.fade_sentinel);
    assert_eq!(extreme.source.fade, Some(3.0e38));
    assert_eq!(extreme.point_light().unwrap().intensity, f32::MAX);
    // The f32::MAX sentinel is "no fade": stored raw, evaluated as unit power, never a maximum light.
    let unit = consume(&records(1.0, 1.0, 0.0), false)
        .point_light()
        .unwrap();
    let sentinel = consume(&records(f32::MAX, 1.0, 0.0), false);
    assert_eq!(sentinel.source.fade, Some(f32::MAX));
    assert!(sentinel.fade_sentinel && !sentinel.backend_limited);
    assert_eq!(sentinel.effective_fade, 1.0);
    assert_eq!(sentinel.point_light().unwrap().intensity, unit.intensity);
    let mut source = records(1.0, 1.0, 0.0);
    let light = light_record(&mut source);
    let data = &mut light
        .subrecords
        .iter_mut()
        .find(|(tag, _)| tag == b"DATA")
        .unwrap()
        .1;
    data[12..16].copy_from_slice(&0x20u32.to_le_bytes());
    let actual = consume(&source, false);
    assert!(actual.suppressed_flags);
    assert!(actual.point_light().is_none());
}

#[test]
fn native_fade_sentinel_signed_offsets_and_shadow_falloff_have_explicit_backend_bounds() {
    let baseline = consume(&records(2.0, 1.0, 0.0), false)
        .point_light()
        .unwrap();
    for (offset, expected) in [
        (0.5f32, 2.5),
        (2.0, 4.0),
        (-1.0, 1.0),
        (-2.0, 0.0),
        (-3.0, 0.0),
        // The sentinel is "no reference fade": the base fade of 2 is left as it is.
        (f32::MAX, 2.0),
    ] {
        let mut source = records(2.0, 4.0, 0.0);
        let data = [0.0f32, offset, 0.0, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        light_reference(&mut source)
            .subrecords
            .push((b"XLIG".to_vec(), data));
        let actual = consume(&source, false);
        let sentinel = offset == f32::MAX;
        assert_eq!(actual.effective_fade, expected, "offset {offset}");
        assert_eq!(
            actual.authored_fade,
            if sentinel {
                2.0
            } else {
                2.0 + f64::from(offset)
            }
        );
        assert_eq!(actual.fade_sentinel, sentinel);
        assert_eq!(actual.reference_fade_applied, !sentinel);
        assert!(!actual.invalid_reference_fade && !actual.malformed_reference_data);
        assert!(!actual.unsupported_reference_data);
        assert_eq!(actual.backend_limited, offset < -2.0, "offset {offset}");
        assert!(
            (actual.point_light().unwrap().intensity / baseline.intensity - expected / 2.0).abs()
                < 1e-5,
            "offset {offset}"
        );
    }
    for flags in [0x400u32, 0x800, 0x1000] {
        let mut source = records(1.0, 4.0, 0.0);
        let data = &mut light_record(&mut source)
            .subrecords
            .iter_mut()
            .find(|(tag, _)| tag == b"DATA")
            .unwrap()
            .1;
        data[12..16].copy_from_slice(&flags.to_le_bytes());
        let actual = consume(&source, false);
        assert_eq!(actual.native_falloff, Some(4.0));
        assert!(actual.unsupported_falloff); // Bevy retains its own attenuation; no invented exponent.
    }
}
