//! Runtime schema extensions and opaque preservation through public pack publication.
mod common;

use converter::records::{DecodedRecord, Value};
use converter::{AssetPipeline, PipelineConfig, config::RecordReader};
use dummy_content::layout;
use rusqlite::Connection;
use serde_json::json;
use std::{fs, io::Write, path::Path};

/// Independently frame a native record with fields kept in the supplied file order.
fn record(signature: &[u8; 4], form_id: u32, fields: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut payload = Vec::new();
    for (tag, bytes) in fields {
        payload.extend_from_slice(tag);
        payload.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(bytes);
    }
    let mut bytes = signature.to_vec();
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&form_id.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&44u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 2]);
    bytes.extend_from_slice(&payload);
    bytes
}

/// Generate ordinary synthetic assets without borrowing the schema decoder's assumptions.
fn generate(data: &Path) {
    layout::prepare_directory(data, false).unwrap();
    layout::generate(
        data,
        layout::DEFAULT_SEED,
        layout::Formats::parse("dds,nif,pex,esm").unwrap(),
    )
    .unwrap();
}

/// Append independently authored native records to the generated master.
fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// Publish through the real pipeline with the no-LOD configuration required by this track.
async fn publish(data: &Path, output: &Path) -> converter::PipelineReport {
    let mut config = PipelineConfig::new(data, output);
    config.record_reader = RecordReader::Inhouse;
    config.no_lod = true;
    config.cpu_jobs = 2;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(config, tx).await.unwrap();
    drain.await.unwrap();
    common::assert_reader_warning_publication(&report, output);
    report
}

async fn publish_with_reader(
    data: &Path,
    output: &Path,
    reader: RecordReader,
) -> converter::PipelineReport {
    let mut config = PipelineConfig::new(data, output);
    config.record_reader = reader;
    config.lod_texture_encoder = converter::TextureEncoder::Cpu;
    config.no_lod = true;
    config.cpu_jobs = 2;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(config, tx).await.unwrap();
    drain.await.unwrap();
    report
}

/// Read the published canonical record rather than accepting source-only preservation.
fn canonical_fields(output: &Path, form_id: u32) -> Vec<([u8; 4], Vec<u8>)> {
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT data FROM records WHERE form_id=?1",
            [form_id],
            |row| row.get(0),
        )
        .unwrap();
    let record =
        rkyv::from_bytes::<converter::esm::types::ArchivedRecordData, rkyv::rancor::Error>(&bytes)
            .unwrap();
    record
        .subrecords
        .into_iter()
        .map(|field| (field.tag, field.data))
        .collect()
}

/// Known records preserve repeated asymmetric opaque fields exactly once and in native order.
#[tokio::test]
async fn canonical_publication_keeps_unknown_fields_once_in_native_order() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    let output = temp.path().join("pack");
    generate(&data);
    let fields = vec![
        (*b"XRAW", vec![0xD7, 0x21, 0x00, 0x93, 0x45]),
        (*b"EDID", b"OpaqueOrderKeyword\0".to_vec()),
        (*b"XRAW", vec![0x16, 0xA2]),
        (*b"CNAM", vec![0x31, 0x62, 0x94, 0x00]),
        (*b"XEND", vec![0xA9, 0x00, 0x51, 0xCE, 0x37, 0x82, 0x14]),
    ];
    append(&data.join("Skyrim.esm"), &record(b"KYWD", 0x1800, &fields));
    publish(&data, &output).await;
    assert_eq!(canonical_fields(&output, 0x1800), fields);
}

/// Unsafe, unreferenced base models clear only their runtime projection; valid neighbors still
/// complete conversion and publish for both record readers.
#[tokio::test]
async fn unsafe_static_models_recover_through_finalization_and_publication() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate(&data);
    let signatures = [*b"TACT", *b"ADDN", *b"APPA", *b"ARTO"];
    let mut records = Vec::new();
    let mut expected = Vec::new();
    for (index, signature) in signatures.into_iter().enumerate() {
        let valid_before_id = 0x1800 + index as u32 * 3;
        let invalid_id = valid_before_id + 1;
        let valid_after_id = valid_before_id + 2;
        let valid_model = [layout::GENERATED_MODEL_PATH.as_bytes(), &[0]].concat();
        let invalid_model = b"../bad.nif\0".to_vec();
        records.extend(record(
            &signature,
            valid_before_id,
            &[
                (*b"EDID", format!("Valid{index}\0").into_bytes()),
                (*b"MODL", valid_model.clone()),
            ],
        ));
        records.extend(record(
            &signature,
            invalid_id,
            &[
                (*b"EDID", format!("Unsafe{index}\0").into_bytes()),
                (*b"MODL", invalid_model.clone()),
            ],
        ));
        records.extend(record(
            &signature,
            valid_after_id,
            &[
                (*b"EDID", format!("ValidAfter{index}\0").into_bytes()),
                (*b"MODL", valid_model),
            ],
        ));
        expected.push((valid_before_id, invalid_id, valid_after_id, invalid_model));
    }
    append(&data.join("Skyrim.esm"), &records);

    for reader in [RecordReader::Legacy, RecordReader::Inhouse] {
        let output = temp.path().join(format!("pack-{reader:?}"));
        let report = publish_with_reader(&data, &output, reader).await;
        let integration = report.integration.as_ref().unwrap();
        assert!(integration.passed, "{reader:?}: {integration:?}");
        assert!(
            report
                .warnings
                .iter()
                .all(|warning| !warning.starts_with("asset integration failed:"))
        );
        assert!(integration.bounds_updated >= 8);
        assert!(integration.statics_with_models >= 8);
        let recovered_ids = expected
            .iter()
            .map(|(_, invalid_id, _, _)| format!("{invalid_id:08X}"))
            .collect::<Vec<_>>();
        assert_eq!(
            integration
                .issues
                .iter()
                .filter(|issue| recovered_ids.iter().any(|id| issue.contains(id)))
                .count(),
            4
        );
        let database = Connection::open(output.join("skyrim_world.db")).unwrap();
        for (valid_before_id, invalid_id, valid_after_id, invalid_model) in &expected {
            for valid_id in [*valid_before_id, *valid_after_id] {
                let valid_path: Option<String> = database
                    .query_row(
                        "SELECT model_path FROM statics WHERE id=?1",
                        [valid_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(valid_path.as_deref(), Some(layout::GENERATED_MODEL_PATH));
            }
            let unsafe_path: Option<String> = database
                .query_row(
                    "SELECT model_path FROM statics WHERE id=?1",
                    [invalid_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(unsafe_path, None);
            let archived = canonical_fields(&output, *invalid_id);
            assert!(archived.contains(&(*b"MODL", invalid_model.clone())));
            assert!(integration.issues.iter().any(|issue| {
                issue.contains("plugin Skyrim.esm")
                    && issue.contains(&format!("{invalid_id:08X}"))
                    && issue.contains("../bad.nif")
            }));
        }
        let published_integration: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("integration-report.json")).unwrap())
                .unwrap();
        assert_eq!(published_integration["passed"], true);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["complete"], report.complete);
    }
}

/// Unusable texture set paths clear only their own runtime columns; the valid slot and the
/// archived record stay, and conversion still publishes for both record readers.
#[tokio::test]
async fn unsafe_texture_set_paths_clear_their_slots_and_still_publish() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate(&data);
    let form_id = 0x1800;
    let valid_normal = "textures/txst_kept_n.dds";
    let traversal = b"..\\..\\outside.dds\0".to_vec();
    append(
        &data.join("Skyrim.esm"),
        &record(
            b"TXST",
            form_id,
            &[
                (*b"EDID", b"UnsafeSlots\0".to_vec()),
                (*b"TX00", traversal.clone()),
                (*b"TX01", [valid_normal.as_bytes(), &[0]].concat()),
                (*b"TX02", b"/textures/rooted.dds\0".to_vec()),
                (*b"TX03", b"C:\\textures\\drive.dds\0".to_vec()),
                (*b"TX04", b"textures/not_a_texture.png\0".to_vec()),
            ],
        ),
    );

    for reader in [RecordReader::Legacy, RecordReader::Inhouse] {
        let output = temp.path().join(format!("pack-{reader:?}"));
        let report = publish_with_reader(&data, &output, reader).await;
        let integration = report.integration.as_ref().unwrap();
        assert!(integration.passed, "{reader:?}: {integration:?}");
        let database = Connection::open(output.join("skyrim_world.db")).unwrap();
        let slots: [Option<String>; 5] = database
            .query_row(
                "SELECT diffuse_path,normal_path,mask_path,glow_path,height_path \
                 FROM texture_sets WHERE id=?1",
                [form_id],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ])
                },
            )
            .unwrap();
        assert_eq!(
            slots,
            [None, Some(valid_normal.to_owned()), None, None, None],
            "{reader:?}"
        );
        let archived = canonical_fields(&output, form_id);
        assert!(archived.contains(&(*b"TX00", traversal.clone())));
    }
}

/// Selected union banks survive real publication, while one malformed leaf stays local.
#[tokio::test]
async fn localized_union_definitions_publish_selected_banks_and_valid_neighbors() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    let output = temp.path().join("pack");
    generate(&data);
    let addon = temp.path().join("localized-unions.json");
    fs::write(
        &addon,
        serde_json::to_vec(&json!({
            "version":1, "name":"localized-union-publication",
            "definitions":{
                "AuditLocalizedLeaf":{"kind":"lstring","size":4,"string_table":"ilstrings"},
                "AuditLocalizedAlias":{"definition":"AuditLocalizedLeaf"}
            },
            "records":[{"signature":"ZLOC","fields":[
                {"signature":"EDID","name":"editor_id","kind":"zstring"},
                {"signature":"ZTXT","name":"selected_text","kind":"union","decider":"size",
                 "string_table":"dlstrings","fields":[{"definition":"AuditLocalizedAlias"}]},
                {"signature":"ZTAL","name":"valid_tail","kind":"u16"}
            ]}]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut bytes = header(&["Skyrim.esm"]);
    bytes[8..12].copy_from_slice(&0x80u32.to_le_bytes());
    for (id, name, text, tail) in [
        (
            0x0100_1810,
            "BeforeBankFailure",
            73u32.to_le_bytes().to_vec(),
            0x5927u16,
        ),
        (
            0x0100_1811,
            "BrokenBankLeaf",
            vec![0xD7, 0x29, 0x93],
            0x173Au16,
        ),
        (
            0x0100_1812,
            "AfterBankFailure",
            73u32.to_le_bytes().to_vec(),
            0x82A6u16,
        ),
    ] {
        bytes.extend(record(
            b"ZLOC",
            id,
            &[
                (*b"EDID", format!("{name}\0").into_bytes()),
                (*b"ZTXT", text),
                (*b"ZTAL", tail.to_le_bytes().to_vec()),
            ],
        ));
    }
    fs::write(data.join("BankProbe.esp"), &bytes).unwrap();
    fs::write(data.join("plugins.txt"), "*Skyrim.esm\n*BankProbe.esp\n").unwrap();
    let directory = data.join("Strings");
    fs::create_dir_all(&directory).unwrap();
    // Both are valid banks containing the same key and distinguishable text.
    for (bank, text) in [
        ("dlstrings", "WrongParentBank"),
        ("ilstrings", "SelectedLeafBank"),
    ] {
        let payload = [
            ((text.len() + 1) as u32).to_le_bytes().to_vec(),
            format!("{text}\0").into_bytes(),
        ]
        .concat();
        let table = [
            1u32.to_le_bytes().to_vec(),
            (payload.len() as u32).to_le_bytes().to_vec(),
            73u32.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
            payload,
        ]
        .concat();
        fs::write(directory.join(format!("BankProbe_english.{bank}")), table).unwrap();
    }
    publish_config(mod_config(&data, &output, &[&addon]), true).await;
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    for (id, tail) in [(0x0100_1810, 0x5927u16), (0x0100_1812, 0x82A6u16)] {
        let fields = canonical_fields(&output, id);
        assert_eq!(fields[1], (*b"ZTXT", b"SelectedLeafBank\0".to_vec()));
        assert_eq!(fields[2], (*b"ZTAL", tail.to_le_bytes().to_vec()));
        let row: (String, u32, String, String) = db.query_row(
            "SELECT string_table,string_id,resolved_text,status FROM inhouse_localized_fields WHERE form_id=?1",
            [id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
        assert_eq!(
            row,
            (
                "ilstrings".into(),
                73,
                "SelectedLeafBank".into(),
                "resolved".into()
            )
        );
        let history = history_record(&output, id);
        assert_eq!(history.fields[1].value, Value::LocalizedString(73));
        assert_eq!(
            history.fields[1].localized_bank.as_deref(),
            Some("ilstrings")
        );
    }
    assert_eq!(
        canonical_fields(&output, 0x0100_1811),
        [
            (*b"EDID", b"BrokenBankLeaf\0".to_vec()),
            (*b"ZTAL", 0x173Au16.to_le_bytes().to_vec())
        ]
    );
    let history = converter::records::history::read_history(&output, None, 0x0100_1811).unwrap();
    assert_eq!(history.versions.len(), 1);
    // Public replay reports rejected-field records as undecodable, preserving source provenance.
    assert_eq!(history.versions[0].decode_status, "undecodable");
    assert!(history.versions[0].record.is_none());
    assert_eq!(fs::read(data.join("BankProbe.esp")).unwrap(), bytes);
}

/// Opaque preservation cannot reintroduce malformed or out-of-order recognized fields.
#[tokio::test]
async fn opaque_preservation_omits_rejected_known_fields_and_keeps_later_unknowns() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    let output = temp.path().join("pack");
    generate(&data);
    let fields = vec![
        (*b"EDID", b"ValidFirstEditor\0".to_vec()),
        (*b"CNAM", vec![0x17, 0x91, 0x63]),
        (*b"XRAW", vec![0x52, 0xA3, 0x16, 0]),
        (*b"EDID", b"RejectedLaterEditor\0".to_vec()),
        (*b"XEND", vec![0xDC, 0x27, 0x98]),
    ];
    append(&data.join("Skyrim.esm"), &record(b"KYWD", 0x1800, &fields));
    publish(&data, &output).await;
    assert_eq!(
        canonical_fields(&output, 0x1800),
        vec![fields[0].clone(), fields[2].clone(), fields[4].clone()]
    );
}

/// Frame an independent native group with its fixed header and declared parent identity.
fn group(kind: i32, label: u32, content: &[u8]) -> Vec<u8> {
    let mut bytes = b"GRUP".to_vec();
    bytes.extend_from_slice(&((content.len() + 24) as u32).to_le_bytes());
    bytes.extend_from_slice(&label.to_le_bytes());
    bytes.extend_from_slice(&kind.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(content);
    bytes
}

/// Opaque projectile-reference radii stay canonical while declared reference radii remain runtime data.
#[tokio::test]
async fn opaque_reference_fields_do_not_enter_scalar_or_spatial_projections() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    let output = temp.path().join("pack");
    generate(&data);
    let plugin = data.join("Skyrim.esm");
    let cell_id = 0x1900u32;
    let projectile_id = 0x1901u32;
    let neighbors = [
        (0x1910u32, b"BeforeOpaqueReference\0".to_vec()),
        (0x1930u32, b"AfterOpaqueReference\0".to_vec()),
    ];
    append(
        &plugin,
        &record(
            b"KYWD",
            neighbors[0].0,
            &[(*b"EDID", neighbors[0].1.clone())],
        ),
    );
    append(
        &plugin,
        &record(
            b"PROJ",
            projectile_id,
            &[(*b"EDID", b"OpaqueRadiusProjectile\0".to_vec())],
        ),
    );
    let cases = [
        (
            *b"PGRE",
            0x1920u32,
            projectile_id,
            [4213.5f32, -8311.25, 93.75, 0.125, -0.375, 0.625],
            1.5f32,
            -17.5f32,
        ),
        (
            *b"PMIS",
            0x1921u32,
            projectile_id,
            [8291.25f32, 4187.5, -41.25, -0.25, 0.75, 1.25],
            0.875f32,
            263.75f32,
        ),
        (
            *b"REFR",
            0x1922u32,
            3u32,
            [123.25f32, -47.5, 68.75, 0.5, 0.25, -0.125],
            2.25f32,
            913.25f32,
        ),
    ];
    let mut references = Vec::new();
    let mut expected = Vec::new();
    for (signature, id, base, transform, scale, radius) in cases {
        let fields = vec![
            (*b"EDID", format!("OpaqueRadius{id:08X}\0").into_bytes()),
            (*b"NAME", base.to_le_bytes().to_vec()),
            (*b"XRDS", radius.to_le_bytes().to_vec()),
            (*b"XSCL", scale.to_le_bytes().to_vec()),
            (
                *b"DATA",
                transform.into_iter().flat_map(f32::to_le_bytes).collect(),
            ),
        ];
        let native = record(&signature, id, &fields);
        expected.push((
            signature,
            id,
            base,
            transform,
            scale,
            radius,
            fields,
            native[24..].to_vec(),
        ));
        references.extend(native);
    }
    let mut world_children = record(
        b"CELL",
        cell_id,
        &[
            (*b"EDID", b"OpaqueRadiusExterior\0".to_vec()),
            (
                *b"XCLC",
                [10i32.to_le_bytes(), (-7i32).to_le_bytes()].concat(),
            ),
        ],
    );
    world_children.extend(group(9, cell_id, &references));
    append(&plugin, &group(1, 1, &world_children));
    append(
        &plugin,
        &record(
            b"KYWD",
            neighbors[1].0,
            &[(*b"EDID", neighbors[1].1.clone())],
        ),
    );
    let original_plugin = fs::read(&plugin).unwrap();
    publish(&data, &output).await;
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    for (signature, id, base, transform, scale, radius, fields, payload) in expected {
        assert_eq!(canonical_fields(&output, id), fields);
        let (source_id, source_payload): (u32, Vec<u8>) = db
            .query_row(
                "SELECT source_form_id,payload FROM inhouse_source_records WHERE form_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(source_id, id);
        assert_eq!(source_payload, payload);
        let decoded = history_record(&output, id);
        let radius_field = decoded
            .fields
            .iter()
            .find(|field| field.signature == *b"XRDS")
            .unwrap();
        let opaque = signature != *b"REFR";
        assert_eq!(radius_field.opaque, opaque);
        assert_eq!(radius_field.canonical_bytes, radius.to_le_bytes());
        let runtime_radius: Option<f64> = db
            .query_row(
                "SELECT radius_override FROM \"references\" WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            runtime_radius,
            (!opaque).then_some(f64::from(radius)),
            "opaque {signature:?} radius leaked into runtime columns"
        );
        type ReferenceProjection = (u32, u32, u32, bool, [f64; 6], f64, Vec<u8>);
        let (
            saved_cell,
            saved_world,
            saved_base,
            exterior,
            saved_transform,
            saved_scale,
            mirror,
        ): ReferenceProjection = db.query_row(
            "SELECT cell_id,worldspace_id,base_form_id,is_exterior,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,scale,data FROM \"references\" WHERE id=?1",
            [id],
            |row| Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                [
                    row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                ],
                row.get(10)?,
                row.get(11)?,
            )),
        ).unwrap();
        assert_eq!(
            (saved_cell, saved_world, saved_base, exterior),
            (cell_id, 1, base, true)
        );
        assert_eq!(saved_transform, transform.map(f64::from));
        assert_eq!(saved_scale, f64::from(scale));
        let canonical_mirror = rkyv::from_bytes::<
            converter::esm::types::ArchivedRecordData,
            rkyv::rancor::Error,
        >(&mirror)
        .unwrap();
        let mirror_fields = canonical_mirror
            .subrecords
            .into_iter()
            .map(|field| (field.tag, field.data))
            .collect::<Vec<_>>();
        assert_eq!(
            mirror_fields, fields,
            "reference data mirror lost opaque bytes"
        );
        let spatial: [f64; 6] = db
            .query_row(
                "SELECT minX,maxX,minY,maxY,minZ,maxZ FROM exterior_spatial WHERE id=?1",
                [id],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ])
                },
            )
            .unwrap();
        assert_eq!(
            spatial,
            [
                transform[0],
                transform[0],
                transform[1],
                transform[1],
                transform[2],
                transform[2],
            ]
            .map(f64::from)
        );
    }
    for (id, editor_id) in neighbors {
        assert_eq!(
            canonical_fields(&output, id),
            [(*b"EDID", editor_id.clone())]
        );
        assert_eq!(
            history_record(&output, id).fields[0].canonical_bytes,
            editor_id
        );
    }
    assert_eq!(
        fs::read(&plugin).unwrap(),
        original_plugin,
        "publication changed source input"
    );
}

/// Author the runtime extension independently of the compiled schema and its builder.
fn addon_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({
        "version": 1,
        "name": "synthetic-mod-links",
        "records": [
            {
                "signature": "ZNW1",
                "fields": [
                    {"signature": "EDID", "name": "editor_id", "kind": "zstring"},
                    {"signature": "SCAL", "name": "custom_value", "kind": "u16"},
                    {"signature": "XLNK", "name": "custom_link", "kind": "form_id", "targets": ["KYWD"]}
                ]
            },
            {
                "signature": "KYWD",
                "extends": true,
                "fields": [
                    {"signature": "XLNK", "name": "custom_link", "kind": "form_id", "targets": ["KYWD"]}
                ]
            }
        ]
    }))
    .unwrap()
}

/// Metadata rebuild sources must type every authored field to remain complete.
fn metadata_addon_bytes() -> Vec<u8> {
    let mut addon: serde_json::Value = serde_json::from_slice(&addon_bytes()).unwrap();
    addon["records"][0]["fields"]
        .as_array_mut()
        .unwrap()
        .push(json!({"signature": "XEND", "name": "extra_bytes", "kind": "bytes"}));
    serde_json::to_vec_pretty(&addon).unwrap()
}

/// Frame a TES4 master list without equating native master positions with global slots.
fn header(masters: &[&str]) -> Vec<u8> {
    let mut fields = vec![(
        *b"HEDR",
        [
            1.7f32.to_le_bytes(),
            8u32.to_le_bytes(),
            0x1800u32.to_le_bytes(),
        ]
        .concat(),
    )];
    for master in masters {
        fields.push((*b"MAST", format!("{master}\0").into_bytes()));
        fields.push((*b"DATA", vec![0xA7; 8]));
    }
    record(b"TES4", 0, &fields)
}

/// A new mod type has numeric and link bytes whose opaque and typed meanings differ.
fn custom_fields(editor_id: &str, link: Vec<u8>) -> Vec<([u8; 4], Vec<u8>)> {
    vec![
        (*b"EDID", format!("{editor_id}\0").into_bytes()),
        (*b"SCAL", 0xA17Eu16.to_le_bytes().to_vec()),
        (*b"XLNK", link),
        (*b"XEND", vec![0xD7, 0x21, 0x93, 0x00, 0x45]),
    ]
}

/// Build a sparse master list that resolves native index zero into global slot two.
fn generate_mod(data: &Path, broken: bool) {
    generate(data);
    write_mod_plugins(data, broken, true);
}

/// Exercise metadata and schema publication with plugins and no input assets to convert.
fn generate_minimal_mod(data: &Path, extend_keyword: bool) {
    fs::create_dir_all(data).unwrap();
    fs::write(data.join("Skyrim.esm"), header(&[])).unwrap();
    write_mod_plugins(data, false, extend_keyword);
}

/// Author synthetic masters, full and light plugins independently of reader preparation.
fn write_mod_plugins(data: &Path, broken: bool, extend_keyword: bool) {
    fs::write(data.join("Spacer.esm"), header(&["Skyrim.esm"])).unwrap();
    let mut owner = header(&["Skyrim.esm"]);
    owner.extend(record(
        b"KYWD",
        0x0100_1801,
        &[
            (*b"EDID", b"AddonLinkTarget\0".to_vec()),
            (*b"CNAM", vec![0x31, 0x62, 0x94, 0]),
        ],
    ));
    fs::write(data.join("Owner.esm"), owner).unwrap();
    let mut patch = header(&["Owner.esm"]);
    patch.extend(record(
        b"ZNW1",
        0x0100_1820,
        &custom_fields("CustomBefore", 0x1801u32.to_le_bytes().to_vec()),
    ));
    if extend_keyword {
        patch.extend(record(
            b"KYWD",
            0x0100_1821,
            &[
                (*b"EDID", b"ExtendedKeyword\0".to_vec()),
                (*b"CNAM", vec![0x74, 0x52, 0x19, 0]),
                (*b"XLNK", 0x1801u32.to_le_bytes().to_vec()),
            ],
        ));
    }
    if broken {
        patch.extend(record(
            b"ZNW1",
            0x0100_1822,
            &custom_fields("BadWidth", vec![0x01, 0x18, 0]),
        ));
        patch.extend(record(
            b"ZNW1",
            0x0100_1823,
            &custom_fields("InvalidMaster", 0x0200_1801u32.to_le_bytes().to_vec()),
        ));
        patch.extend(record(
            b"ZNW1",
            0x0100_1824,
            &custom_fields("DanglingTarget", 0x1817u32.to_le_bytes().to_vec()),
        ));
        patch.extend(record(
            b"ZNW1",
            0x0100_1825,
            &custom_fields("CustomAfter", 0x1801u32.to_le_bytes().to_vec()),
        ));
    }
    fs::write(data.join("Patch.esp"), patch).unwrap();
    let mut light = header(&["Owner.esm"]);
    light[8..12].copy_from_slice(&0x200u32.to_le_bytes());
    light.extend(record(
        b"ZNW1",
        0x0100_0877,
        &custom_fields("LightCustom", 0x1801u32.to_le_bytes().to_vec()),
    ));
    fs::write(data.join("AddonLight.esl"), light).unwrap();
    fs::write(
        data.join("plugins.txt"),
        "*Skyrim.esm\n*Spacer.esm\n*Owner.esm\n*Patch.esp\n*AddonLight.esl\n",
    )
    .unwrap();
}

/// Select the explicit synthetic load order and optional runtime JSON paths.
fn mod_config(data: &Path, output: &Path, addons: &[&Path]) -> PipelineConfig {
    let mut config = PipelineConfig::new(data, output);
    config.plugins_file = Some(data.join("plugins.txt"));
    config.record_reader = RecordReader::Inhouse;
    config.schema_addons = addons.iter().map(|path| path.to_path_buf()).collect();
    config.no_lod = true;
    config.cpu_jobs = 2;
    config
}

/// Drain progress even on preflight failures so assertions inspect the actual pipeline result.
async fn run_config(
    config: PipelineConfig,
) -> Result<converter::PipelineReport, converter::PipelineFailure> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = AssetPipeline::run_async(config, tx).await;
    drain.await.unwrap();
    result
}

/// Require the fixture's expected completeness before inspecting published provenance.
async fn publish_config(config: PipelineConfig, reader_warnings: bool) {
    let output = config.output_dir.clone();
    let report = run_config(config).await.unwrap();
    if reader_warnings {
        common::assert_reader_warning_publication(&report, &output);
    } else {
        assert!(report.complete, "{report:?}");
    }
}

/// History replay must reconstruct the winner using the pack's producing schema.
fn history_record(output: &Path, form_id: u32) -> DecodedRecord {
    let history = converter::records::history::read_history(output, None, form_id).unwrap();
    assert_eq!(history.versions.len(), 1);
    let version = history.versions.into_iter().next().unwrap();
    assert_eq!(
        version.decode_status, "decoded",
        "{:?}",
        version.decode_reason
    );
    assert!(version.winner);
    version.record.unwrap()
}

/// Inspect an occurrence by its native tag so schema names cannot mask a lost field.
fn field_value(record: &DecodedRecord, signature: &[u8; 4]) -> Value {
    let fields = record
        .fields
        .iter()
        .filter(|field| field.signature == *signature)
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 1, "{:?}", record.fields);
    fields[0].value.clone()
}

/// The same mod publishes opaque bytes without add-ons and typed remapped fields with them.
#[tokio::test]
async fn addons_type_new_records_and_extend_known_fields_with_asymmetric_remapping() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_mod(&data, false);
    let opaque = temp.path().join("opaque");
    publish_config(mod_config(&data, &opaque, &[]), true).await;
    let original = custom_fields("CustomBefore", 0x1801u32.to_le_bytes().to_vec());
    assert_eq!(canonical_fields(&opaque, 0x0300_1820), original);
    let unknown = history_record(&opaque, 0x0300_1820);
    assert!(!unknown.supported);
    assert_eq!(
        field_value(&unknown, b"SCAL"),
        Value::Bytes(vec![0x7E, 0xA1])
    );
    assert_eq!(
        field_value(&unknown, b"XLNK"),
        Value::Bytes(0x1801u32.to_le_bytes().to_vec())
    );
    let known = history_record(&opaque, 0x0300_1821);
    assert!(known.supported);
    assert_eq!(
        field_value(&known, b"XLNK"),
        Value::Bytes(0x1801u32.to_le_bytes().to_vec())
    );
    assert_eq!(
        canonical_fields(&opaque, 0xFE00_0877),
        custom_fields("LightCustom", 0x1801u32.to_le_bytes().to_vec())
    );

    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let typed = temp.path().join("typed");
    publish_config(mod_config(&data, &typed, &[&addon]), true).await;
    let mut remapped = original;
    remapped[2].1 = 0x0200_1801u32.to_le_bytes().to_vec();
    assert_eq!(canonical_fields(&typed, 0x0300_1820), remapped);
    for id in [0x0300_1820, 0x0300_1821] {
        let record = history_record(&typed, id);
        assert!(record.supported);
        assert_eq!(field_value(&record, b"XLNK"), Value::FormId(0x0200_1801));
        assert_eq!(
            field_value(&record, b"EDID"),
            Value::String(if id == 0x0300_1820 {
                "CustomBefore".into()
            } else {
                "ExtendedKeyword".into()
            })
        );
    }
    let custom = history_record(&typed, 0x0300_1820);
    assert_eq!(field_value(&custom, b"SCAL"), Value::Unsigned(0xA17E));
    assert_eq!(
        field_value(&custom, b"XEND"),
        Value::Bytes(vec![0xD7, 0x21, 0x93, 0, 0x45])
    );
    let light = history_record(&typed, 0xFE00_0877);
    assert!(light.supported);
    assert_eq!(light.source_form_id, 0x0100_0877);
    assert_eq!(field_value(&light, b"XLNK"), Value::FormId(0x0200_1801));
}

/// Independent extensions do not advance the base cursor or turn opaque tags into conditions.
#[tokio::test]
async fn extension_before_builtin_fields_preserves_opaque_condition_tags() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_minimal_mod(&data, true);
    let fields = vec![
        (*b"XLNK", 0x1801u32.to_le_bytes().to_vec()),
        (*b"CTDA", vec![0x31, 0x87, 0xD2]),
        (*b"CIS1", vec![0xC5, 0x19]),
        (
            *b"VMAD",
            dummy_content::inhouse_vmad::field(
                5,
                2,
                &[dummy_content::inhouse_vmad::script(
                    5,
                    "OpaqueScript",
                    2,
                    &[dummy_content::inhouse_vmad::property(
                        5,
                        "Object",
                        1,
                        1,
                        &dummy_content::inhouse_vmad::object(2, 0x1801, -1, 0xBC51),
                    )],
                )],
                &[],
            ),
        ),
        (*b"EDID", b"ExtensionBeforeBuiltin\0".to_vec()),
        (*b"CNAM", vec![0x61, 0x42, 0xD3, 0]),
    ];
    append(
        &data.join("Patch.esp"),
        &record(b"KYWD", 0x0100_1830, &fields),
    );
    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let output = temp.path().join("pack");
    publish_config(mod_config(&data, &output, &[&addon]), true).await;
    let mut expected = fields;
    expected[0].1 = 0x0200_1801u32.to_le_bytes().to_vec();
    assert_eq!(canonical_fields(&output, 0x0300_1830), expected);
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM scripts WHERE form_id=?1",
            [0x0300_1830u32],
            |row| row.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}

/// Repeated extensions retain physical order and singleton duplicates are safely omitted.
#[tokio::test]
async fn repeated_extensions_validate_each_occurrence_without_rewinding_builtin_fields() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_minimal_mod(&data, true);
    let fields = vec![
        (*b"XLNK", 0x1801u32.to_le_bytes().to_vec()),
        (*b"EDID", b"RepeatedExtension\0".to_vec()),
        (*b"XLNK", 0x1817u32.to_le_bytes().to_vec()),
        (*b"CNAM", vec![0x62, 0x19, 0xF3, 0]),
        (*b"XEND", vec![0x41, 0xB8, 0x97]),
    ];
    append(
        &data.join("Patch.esp"),
        &record(b"KYWD", 0x0100_1831, &fields),
    );
    for repeat in [false, true] {
        let mut schema: serde_json::Value = serde_json::from_slice(&addon_bytes()).unwrap();
        schema["records"][1]["fields"][0]["repeat"] = json!(repeat);
        let addon = temp.path().join(format!("repeat-{repeat}.json"));
        fs::write(&addon, serde_json::to_vec(&schema).unwrap()).unwrap();
        let output = temp.path().join(format!("pack-{repeat}"));
        publish_config(mod_config(&data, &output, &[&addon]), true).await;
        let mut expected = fields.clone();
        expected[0].1 = 0x0200_1801u32.to_le_bytes().to_vec();
        if repeat {
            expected[2].1 = vec![0; 4];
        } else {
            expected.remove(2);
        }
        assert_eq!(canonical_fields(&output, 0x0300_1831), expected);
    }
}

/// Pack and SQL provenance retain the producing bytes even after the configured JSON changes.
#[tokio::test]
async fn addon_manifest_and_history_replay_use_stored_hashed_json() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_mod(&data, false);
    let addon = temp.path().join("schema.json");
    let authored = addon_bytes();
    fs::write(&addon, &authored).unwrap();
    let output = temp.path().join("pack");
    publish_config(mod_config(&data, &output, &[&addon]), true).await;
    let sha256 = converter::cache::hash_bytes(&authored);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["schema_addons"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["schema_addons"][0]["name"], "synthetic-mod-links");
    assert_eq!(manifest["schema_addons"][0]["sha256"], sha256);
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    let stored: (u32, String, String, Vec<u8>) = db
        .query_row(
            "SELECT ordinal,name,sha256,schema_json FROM inhouse_schema_addons",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(stored, (0, "synthetic-mod-links".into(), sha256, authored));
    fs::write(&addon, b"changed addon is deliberately invalid JSON").unwrap();
    assert_eq!(
        field_value(&history_record(&output, 0x0300_1820), b"XLNK"),
        Value::FormId(0x0200_1801)
    );
}

/// A bad add-on field is omitted, bad links clear locally, and later valid records still publish.
#[tokio::test]
async fn malformed_addon_fields_preserve_valid_neighbors_through_publication() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_mod(&data, true);
    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let output = temp.path().join("pack");
    publish_config(mod_config(&data, &output, &[&addon]), true).await;
    for id in [0x0300_1820, 0x0300_1825] {
        assert_eq!(
            field_value(&history_record(&output, id), b"XLNK"),
            Value::FormId(0x0200_1801)
        );
    }
    let malformed = canonical_fields(&output, 0x0300_1822);
    assert!(!malformed.iter().any(|(tag, _)| tag == b"XLNK"));
    assert_eq!(
        malformed.last().unwrap(),
        &(*b"XEND", vec![0xD7, 0x21, 0x93, 0, 0x45])
    );
    for id in [0x0300_1823, 0x0300_1824] {
        // History decodes source versions before final winner target clearing;
        // published canonical bytes below prove the actual recovery action.
        assert_eq!(
            field_value(&history_record(&output, id), b"XLNK"),
            Value::FormId(if id == 0x0300_1823 { 0 } else { 0x0200_1817 })
        );
        assert_eq!(
            canonical_fields(&output, id)
                .into_iter()
                .find(|(tag, _)| tag == b"XLNK")
                .unwrap()
                .1,
            vec![0; 4]
        );
    }
    let diagnostic: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("inhouse-reader-diagnostics.json")).unwrap())
            .unwrap();
    assert!(
        diagnostic["decoder"]["patch.esp"]["skipped_fields"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        diagnostic["decoder"]["patch.esp"]["invalid_links"]
            .as_u64()
            .unwrap()
            >= 2
    );
    let first = diagnostic["decoder"]["patch.esp"]["first_by_category"]["field"]
        .as_str()
        .unwrap();
    assert!(first.contains("XLNK"), "{first}");
}

/// A mandatory extension failure rejects only its override and retains usable neighboring forms.
#[tokio::test]
async fn mandatory_extension_error_keeps_predecessor_and_publishes_neighbors() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_minimal_mod(&data, true);
    append(
        &data.join("Patch.esp"),
        &record(
            b"KYWD",
            0x0000_1801,
            &[
                (*b"EDID", b"BrokenOverride\0".to_vec()),
                (*b"CNAM", vec![0x91, 0x35, 0x64, 0]),
                (*b"XLNK", vec![0x01, 0x18, 0]),
            ],
        ),
    );
    append(
        &data.join("Patch.esp"),
        &record(
            b"KYWD",
            0x0100_1840,
            &[
                (*b"XLNK", 0x1801u32.to_le_bytes().to_vec()),
                (*b"EDID", b"ValidNeighbor\0".to_vec()),
                (*b"CNAM", vec![0x72, 0x49, 0xC3, 0]),
            ],
        ),
    );
    let mut schema: serde_json::Value = serde_json::from_slice(&addon_bytes()).unwrap();
    schema["records"][1]["fields"][0]["reject_record_on_error"] = json!(true);
    let addon = temp.path().join("mandatory.json");
    fs::write(&addon, serde_json::to_vec(&schema).unwrap()).unwrap();
    let output = temp.path().join("pack");
    publish_config(mod_config(&data, &output, &[&addon]), true).await;
    let chain = converter::records::history::read_history(&output, None, 0x0200_1801).unwrap();
    assert_eq!(chain.versions.len(), 2);
    let winner = chain
        .versions
        .iter()
        .find(|version| version.winner)
        .unwrap();
    assert_eq!(
        field_value(winner.record.as_ref().unwrap(), b"EDID"),
        Value::String("AddonLinkTarget".into())
    );
    assert_eq!(
        field_value(&history_record(&output, 0x0300_1840), b"XLNK"),
        Value::FormId(0x0200_1801)
    );
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    let (status, reason): (String, String) = db
        .query_row(
            "SELECT h.status,h.reason FROM record_overrides h JOIN plugins p ON p.id=h.plugin_id WHERE h.form_id=?1 AND lower(p.name)='patch.esp'",
            [0x0200_1801u32],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "skipped");
    assert!(reason.contains("mandatory custom_link"), "{reason}");
}

/// A config error cannot modify an existing publication or leave a half-applied staging pack.
#[tokio::test]
async fn invalid_addon_preflight_preserves_existing_pack_and_creates_no_staging() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_mod(&data, false);
    let output = temp.path().join("pack");
    publish_config(mod_config(&data, &output, &[]), true).await;
    let original_manifest = fs::read(output.join("conversion-manifest.json")).unwrap();
    let original_database = fs::read(output.join("skyrim_world.db")).unwrap();
    let before = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    let valid = temp.path().join("valid.json");
    fs::write(&valid, addon_bytes()).unwrap();
    let bad = temp.path().join("bad.json");
    fs::write(&bad, br#"{"version":1,"name":"bad-addon","records":[{"signature":"BAD1","fields":[{"signature":"LINK","name":"invalid","kind":"form_id"}]}]}"#).unwrap();
    let error = run_config(mod_config(&data, &output, &[&valid, &bad]))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("bad.json") || message.contains("bad-addon"),
        "{message}"
    );
    assert!(message.contains("target"), "{message}");
    assert_eq!(
        fs::read(output.join("conversion-manifest.json")).unwrap(),
        original_manifest
    );
    assert_eq!(
        fs::read(output.join("skyrim_world.db")).unwrap(),
        original_database
    );
    let after = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "valid.json" && name != "bad.json")
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(after, before);
}

/// Runtime schema add-ons cannot silently disappear under the legacy reader selection.
#[tokio::test]
async fn legacy_reader_rejects_runtime_schema_addons() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    fs::create_dir(&data).unwrap();
    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let mut config = mod_config(&data, &temp.path().join("pack"), &[&addon]);
    config.record_reader = RecordReader::Legacy;
    let error = run_config(config).await.unwrap_err().to_string();
    assert!(
        error.contains("inhouse") || error.contains("in-house"),
        "{error}"
    );
}

/// Add-on content changes metadata proofs while asset settings remain reusable.
#[test]
fn addon_content_changes_metadata_fingerprint_without_invalidating_asset_settings() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    fs::create_dir(&data).unwrap();
    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let baseline = mod_config(&data, &temp.path().join("pack"), &[]);
    let configured = mod_config(&data, &temp.path().join("pack"), &[&addon]);
    let before = converter::cache::configuration_hash(&configured).unwrap();
    assert_ne!(
        before,
        converter::cache::configuration_hash(&baseline).unwrap()
    );
    let relocated = temp.path().join("same-schema-elsewhere.json");
    fs::write(&relocated, addon_bytes()).unwrap();
    assert_eq!(
        before,
        converter::cache::configuration_hash(&mod_config(
            &data,
            &temp.path().join("pack"),
            &[&relocated],
        ))
        .unwrap()
    );
    let mut changed: serde_json::Value = serde_json::from_slice(&addon_bytes()).unwrap();
    changed["records"][0]["fields"][1]["name"] = json!("renamed_custom_value");
    fs::write(&addon, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert_ne!(
        before,
        converter::cache::configuration_hash(&configured).unwrap()
    );
    for extension in ["nif", "dds", "pex"] {
        assert_eq!(
            converter::cache::asset_configuration_hash_for_schema(
                &baseline,
                converter::cache::CONVERTER_SCHEMA_VERSION,
                extension
            )
            .unwrap(),
            converter::cache::asset_configuration_hash_for_schema(
                &configured,
                converter::cache::CONVERTER_SCHEMA_VERSION,
                extension
            )
            .unwrap()
        );
    }
}

/// Read an explicit pack artifact without consulting the converter's invocation paths.
fn json_artifact(output: &Path, name: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(output.join(name)).unwrap()).unwrap()
}

/// Rebuild from recorded plugin order and retain the original verified producer assets.
async fn rebuild_config(mut config: PipelineConfig, source: &Path, reader_warnings: bool) {
    let output = config.output_dir.clone();
    config.plugins_file = None;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::rebuild_metadata_async(config, source, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    if reader_warnings {
        common::assert_reader_warning_publication(&report, &output);
    } else {
        assert!(report.complete, "{report:?}");
    }
}

/// Repeated adapter-10 migrations retain original source documents and verified script bytes.
/// The synthetic package uses a historical metadata stamp; no older converter binary is invoked.
#[tokio::test]
async fn adapter10_metadata_rebuild_preserves_original_assets_and_saved_addons_across_generations()
{
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    // Rebuilds without output add-ons keep unknown record types opaque; a known
    // KYWD with an unrecognized extension would instead make the first rebuild incomplete.
    generate_minimal_mod(&data, false);
    let addon = temp.path().join("original.json");
    let original_bytes = metadata_addon_bytes();
    fs::write(&addon, &original_bytes).unwrap();
    let source = temp.path().join("adapter10-source");
    let original_config = mod_config(&data, &source, &[&addon]);
    publish_config(original_config.clone(), false).await;
    let mut identity = json_artifact(&source, "record-reader.json");
    identity["adapter_version"] = json!(10);
    let historical_settings = json!({
        "schema": converter::cache::CONVERTER_SCHEMA_VERSION,
        "texture_etc1s_quality": original_config.texture_fallback_quality,
        "texture_uastc_level": original_config.texture_uastc_level,
        "texture_zstd_level": original_config.texture_zstd_level,
        "texture_encoder": {"mode": "cpu"},
        "script_abi_version": original_config.script_abi_version,
        "record_reader": identity,
    });
    let historical_hash =
        converter::cache::hash_bytes(&serde_json::to_vec(&historical_settings).unwrap());
    let mut original_manifest = json_artifact(&source, "conversion-manifest.json");
    original_manifest["configuration_hash"] = json!(historical_hash);
    fs::write(
        source.join("record-reader.json"),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();
    fs::write(
        source.join("conversion-manifest.json"),
        serde_json::to_vec(&original_manifest).unwrap(),
    )
    .unwrap();
    let source_database_hash =
        converter::cache::hash_file(&source.join("skyrim_world.db")).unwrap();
    let runtime_path = "scripts/papyrus_runtime.luau";
    let original_runtime = fs::read(source.join(runtime_path)).unwrap();
    let original_runtime_hash = converter::cache::hash_bytes(&original_runtime);
    let original_documents = json!([original_bytes]);
    fs::write(
        &addon,
        b"changed invalid invocation file must not replace saved source bytes",
    )
    .unwrap();

    let first = temp.path().join("adapter12-first");
    rebuild_config(mod_config(&data, &first, &[]), &source, false).await;
    let second = temp.path().join("adapter12-second");
    rebuild_config(mod_config(&data, &second, &[]), &first, false).await;
    for output in [&first, &second] {
        let reader = json_artifact(output, "record-reader.json");
        assert_eq!(reader["adapter_version"], 12);
        let manifest = json_artifact(output, "conversion-manifest.json");
        let provenance = json_artifact(output, "metadata-rebuild.json");
        assert_ne!(
            manifest["configuration_hash"],
            original_manifest["configuration_hash"]
        );
        assert_eq!(
            manifest["schema_version"],
            original_manifest["schema_version"]
        );
        assert_eq!(
            manifest["retained_mesh_schema_version"],
            original_manifest["schema_version"]
        );
        assert_eq!(
            manifest["retained_asset_configuration_hash"],
            original_manifest["configuration_hash"]
        );
        assert_eq!(
            manifest["asset_configuration_hashes"],
            original_manifest["asset_configuration_hashes"]
        );
        assert_eq!(provenance["retained_asset_record_reader"], identity);
        assert_eq!(provenance["retained_asset_schema_json"], original_documents);
        assert_eq!(
            provenance["retained_asset_hashes"][runtime_path],
            original_runtime_hash
        );
        assert_eq!(
            fs::read(output.join(runtime_path)).unwrap(),
            original_runtime
        );
        assert_eq!(
            canonical_fields(output, 0x0300_1820),
            custom_fields("CustomBefore", 0x1801u32.to_le_bytes().to_vec())
        );
    }
    assert_eq!(
        json_artifact(&source, "conversion-manifest.json"),
        original_manifest
    );
    assert_eq!(json_artifact(&source, "record-reader.json"), identity);
    assert_eq!(
        converter::cache::hash_file(&source.join("skyrim_world.db")).unwrap(),
        source_database_hash
    );
    assert_eq!(
        fs::read(source.join(runtime_path)).unwrap(),
        original_runtime
    );
}

/// Repeated rebuilds use requested schemas while retaining exact original add-on asset provenance.
#[tokio::test]
async fn metadata_rebuilds_replay_saved_addons_and_preserve_original_asset_producer() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_minimal_mod(&data, true);
    let original_addon = temp.path().join("original.json");
    let original_bytes = metadata_addon_bytes();
    fs::write(&original_addon, &original_bytes).unwrap();
    let original = temp.path().join("original-pack");
    publish_config(mod_config(&data, &original, &[&original_addon]), false).await;
    let original_manifest = json_artifact(&original, "conversion-manifest.json");
    assert!(original_manifest["entries"].as_object().unwrap().is_empty());
    let original_reader = json_artifact(&original, "record-reader.json");
    let runtime_path = "scripts/papyrus_runtime.luau";
    let original_runtime = fs::read(original.join(runtime_path)).unwrap();
    let original_runtime_hash = converter::cache::hash_bytes(&original_runtime);
    let original_schema_json = serde_json::to_value(vec![original_bytes]).unwrap();

    // Invocation files may change after publication; replay must use saved producing bytes.
    fs::write(
        &original_addon,
        b"the original configured add-on is now invalid",
    )
    .unwrap();
    let mut replacement: serde_json::Value =
        serde_json::from_slice(&metadata_addon_bytes()).unwrap();
    replacement["name"] = json!("synthetic-mod-links-signed");
    replacement["records"][0]["fields"][1]["kind"] = json!("i16");
    let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
    let replacement_addon = temp.path().join("replacement.json");
    fs::write(&replacement_addon, &replacement_bytes).unwrap();
    let changed = temp.path().join("signed-pack");
    rebuild_config(
        mod_config(&data, &changed, &[&replacement_addon]),
        &original,
        false,
    )
    .await;
    let signed = history_record(&changed, 0x0300_1820);
    assert!(signed.supported);
    assert_eq!(field_value(&signed, b"SCAL"), Value::Signed(-24_194));
    assert_eq!(field_value(&signed, b"XLNK"), Value::FormId(0x0200_1801));
    let changed_manifest = json_artifact(&changed, "conversion-manifest.json");
    assert_eq!(
        changed_manifest["schema_addons"],
        json!([{
            "name": "synthetic-mod-links-signed",
            "sha256": converter::cache::hash_bytes(&replacement_bytes),
        }])
    );

    let opaque = temp.path().join("opaque-pack");
    rebuild_config(mod_config(&data, &opaque, &[]), &changed, true).await;
    let unknown = history_record(&opaque, 0x0300_1820);
    assert!(!unknown.supported);
    assert_eq!(
        field_value(&unknown, b"SCAL"),
        Value::Bytes(vec![0x7E, 0xA1])
    );
    assert_eq!(
        field_value(&unknown, b"XLNK"),
        Value::Bytes(0x1801u32.to_le_bytes().to_vec())
    );
    assert_eq!(
        canonical_fields(&opaque, 0x0300_1820),
        custom_fields("CustomBefore", 0x1801u32.to_le_bytes().to_vec())
    );
    for output in [&changed, &opaque] {
        let manifest = json_artifact(output, "conversion-manifest.json");
        let reader = json_artifact(output, "record-reader.json");
        let provenance = json_artifact(output, "metadata-rebuild.json");
        assert_eq!(reader["mode"], "inhouse");
        assert_eq!(
            reader["schema_addons"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            manifest["schema_addons"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        );
        assert_eq!(provenance["record_reader"], reader);
        assert_eq!(provenance["retained_asset_record_reader"], original_reader);
        assert_eq!(
            provenance["retained_asset_schema_json"],
            original_schema_json
        );
        assert_eq!(
            manifest["retained_asset_configuration_hash"],
            original_manifest["configuration_hash"]
        );
        assert_eq!(
            manifest["asset_configuration_hashes"],
            original_manifest["asset_configuration_hashes"]
        );
        assert_eq!(
            fs::read(output.join(runtime_path)).unwrap(),
            original_runtime
        );
        assert_eq!(
            provenance["retained_asset_hashes"][runtime_path],
            original_runtime_hash
        );
    }
    assert!(
        json_artifact(&opaque, "conversion-manifest.json")["schema_addons"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    assert_eq!(
        json_artifact(&original, "conversion-manifest.json"),
        original_manifest
    );
}

/// Schema bytes frozen before the first progress event remain authoritative after disk mutation.
#[tokio::test]
async fn addon_preflight_freezes_publication_bytes_before_discovery_progress() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_minimal_mod(&data, true);
    let addon = temp.path().join("schema.json");
    let original_bytes = addon_bytes();
    fs::write(&addon, &original_bytes).unwrap();
    let output = temp.path().join("pack");
    let config = mod_config(&data, &output, &[&addon]);
    let mutate = addon.clone();
    // One queued event forces progress consumers to run before later conversion stages proceed.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<converter::ProgressEvent>(1);
    let drain = tokio::spawn(async move {
        let mut changed = false;
        while let Some(event) = rx.recv().await {
            if !changed && event.stage == converter::ProgressStage::Discovering {
                fs::write(&mutate, b"invalid JSON written after preflight").unwrap();
                changed = true;
            }
        }
        changed
    });
    let report = AssetPipeline::run_async(config, tx).await.unwrap();
    assert!(
        drain.await.unwrap(),
        "the fixture never observed discovery progress"
    );
    common::assert_reader_warning_publication(&report, &output);
    assert_eq!(
        fs::read(&addon).unwrap(),
        b"invalid JSON written after preflight"
    );
    let expected_hash = converter::cache::hash_bytes(&original_bytes);
    let manifest = json_artifact(&output, "conversion-manifest.json");
    assert_eq!(manifest["schema_addons"][0]["sha256"], expected_hash);
    assert_eq!(
        field_value(&history_record(&output, 0x0300_1820), b"XLNK"),
        Value::FormId(0x0200_1801)
    );
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    let saved: (String, Vec<u8>) = db
        .query_row(
            "SELECT sha256,schema_json FROM inhouse_schema_addons",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved, (expected_hash, original_bytes));
}

/// A cached database is keyed by its schema add-ons: a base bundle never restores for an add-on
/// configuration, an unchanged add-on bundle restores with its own typed records, editing the
/// add-on rebuilds, and returning to the base reproduces the base database, never the add-on one.
#[tokio::test]
async fn database_cache_keys_and_restores_by_schema_addon_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    generate_mod(&data, false);
    let addon = temp.path().join("schema.json");
    fs::write(&addon, addon_bytes()).unwrap();
    let output = temp.path().join("pack");
    let original = custom_fields("CustomBefore", 0x1801u32.to_le_bytes().to_vec());
    let mut remapped = original.clone();
    remapped[2].1 = 0x0200_1801u32.to_le_bytes().to_vec();
    let stamp = |output: &Path| -> serde_json::Value {
        serde_json::from_slice(&fs::read(output.join("record-reader.json")).unwrap()).unwrap()
    };

    let base_cold = run_config(mod_config(&data, &output, &[])).await.unwrap();
    assert!(!base_cold.database_cache_hit);
    let base_warm = run_config(mod_config(&data, &output, &[])).await.unwrap();
    assert!(base_warm.database_cache_hit, "unchanged base restores");
    assert_eq!(
        base_warm.warnings, base_cold.warnings,
        "restore repeats base findings once"
    );
    let base_stamp = stamp(&output);
    assert!(base_stamp.get("schema_addons").is_none());
    assert!(!history_record(&output, 0x0300_1820).supported);

    let typed = run_config(mod_config(&data, &output, &[&addon]))
        .await
        .unwrap();
    assert!(
        !typed.database_cache_hit,
        "add-on configuration must not restore the base bundle"
    );
    assert_ne!(stamp(&output)["schema_sha256"], base_stamp["schema_sha256"]);
    let typed_stamp = stamp(&output);
    assert!(stamp(&output)["schema_addons"].is_array());
    assert_eq!(canonical_fields(&output, 0x0300_1820), remapped);
    assert!(history_record(&output, 0x0300_1820).supported);

    let typed_warm = run_config(mod_config(&data, &output, &[&addon]))
        .await
        .unwrap();
    assert!(
        typed_warm.database_cache_hit,
        "unchanged add-on bundle restores"
    );
    assert_eq!(canonical_fields(&output, 0x0300_1820), remapped);
    assert_eq!(stamp(&output), typed_stamp);
    assert_eq!(typed_warm.warnings, typed.warnings);

    fs::write(&addon, metadata_addon_bytes()).unwrap();
    let edited = run_config(mod_config(&data, &output, &[&addon]))
        .await
        .unwrap();
    assert!(!edited.database_cache_hit, "an edited add-on must rebuild");
    assert_ne!(
        stamp(&output),
        typed_stamp,
        "the stamp must describe the edited add-on"
    );
    fs::write(&addon, addon_bytes()).unwrap();

    let back = run_config(mod_config(&data, &output, &[])).await.unwrap();
    assert!(
        !back.database_cache_hit,
        "returning to base must not reuse the add-on bundle"
    );
    assert_eq!(stamp(&output), base_stamp);
    assert_eq!(canonical_fields(&output, 0x0300_1820), original);
    assert!(!history_record(&output, 0x0300_1820).supported);
}
