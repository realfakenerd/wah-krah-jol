//! Snapshot tests for the converter's own output.
//!
//! Every value here comes from the deterministic `dummy-content` fixtures
//! through the real conversion code, so a change to what the converter produces
//! shows up as a readable diff in review instead of as a wrong render later:
//!
//! * [`converted_static_model_gltf_matches_snapshot`] - the glTF JSON chunk of
//!   a converted static model: node tree, mesh primitives and their attributes,
//!   the material with its extras, and accessor bounds. The binary chunk is
//!   deliberately not part of the snapshot.
//! * [`world_database_tables_match_snapshot`] - the exported
//!   `skyrim_world.db`'s rows (worldspaces, exterior and interior cells,
//!   references with their transforms and flags, the door references, statics,
//!   texture sets, landscape textures, terrain and the FormID map), ordered by
//!   primary key.
//! * [`conversion_manifest_matches_snapshot`] - the published
//!   `conversion-manifest.json`: schema version, completeness, input counts and
//!   one entry per converted asset.
//! * [`pipeline_report_matches_snapshot`] - the report the pipeline returns:
//!   counts, artifacts, completeness and the integration gate.
//!
//! To accept an intentional change run `cargo insta review` and approve it, or
//! read the `*.snap.new` file the failing run writes into
//! `crates/converter/tests/snapshots/` and rename it to `*.snap` once it holds
//! what the converter is meant to produce.

mod common;

use common::cpu_lod_config;
use dummy_content::{esm, layout};
use insta::assert_json_snapshot;
use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The bytes `dummy-content gen --with-interior` writes: one exterior cell,
/// its auto-load door into one interior cell, and the return door. The spec is
/// assembled from the same `layout` constants the command uses, so this file
/// cannot drift from the plugin the command publishes.
fn preset_plugin() -> Vec<u8> {
    let cells = [esm::PRESET_EXTERIOR_CELL];
    esm::plugin_with_interior(
        &esm::Plugin {
            author: layout::GENERATED_AUTHOR,
            worldspace: layout::GENERATED_WORLDSPACE,
            cells: &cells,
            model_path: layout::GENERATED_MODEL_PATH,
            diffuse: layout::GENERATED_DIFFUSE_PATH,
            normal_texture: layout::GENERATED_NORMAL_PATH,
        },
        &esm::PRESET_INTERIOR,
    )
    .unwrap()
}

/// Writes the plugin [`preset_plugin`] describes into a fresh `Skyrim.esm`.
fn write_preset_plugin(directory: &Path) -> PathBuf {
    let path = directory.join("Skyrim.esm");
    fs::write(&path, preset_plugin()).unwrap();
    path
}

/// The glTF JSON chunk of a GLB, without its binary chunk: the same document
/// the engine reads for nodes, meshes, materials and bounds.
fn glb_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"glTF", "{} is not a GLB", path.display());
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize,
        bytes.len(),
        "GLB length is inconsistent"
    );
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    assert_eq!(&bytes[16..20], b"JSON");
    let json = serde_json::from_slice(&bytes[20..20 + json_length]).unwrap();
    assert!(
        bytes.len() > 20 + json_length,
        "the snapshot is meant to cover the JSON chunk of a GLB with geometry"
    );
    json
}

/// Every row of `table` as JSON, ordered by `order_by` so the snapshot is
/// stable. Blob columns are reported as their length: the archive-encoded
/// subrecords are input to the rkyv cell cache, not something a reviewer reads.
fn table_rows(connection: &Connection, table: &str, order_by: &str) -> Vec<Value> {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY {order_by}"))
        .unwrap();
    let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
    statement
        .query_map([], |row| {
            let mut object = serde_json::Map::new();
            for (index, column) in columns.iter().enumerate() {
                object.insert(column.clone(), column_value(row.get::<_, SqlValue>(index)?));
            }
            Ok(Value::Object(object))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn column_value(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(number) => Value::from(number),
        SqlValue::Real(number) => serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or_else(|| Value::from(format!("{number}"))),
        SqlValue::Text(text) => Value::from(text),
        SqlValue::Blob(bytes) => json!({ "encoded_bytes": bytes.len() }),
    }
}

/// Fails any snapshot that carries a machine-specific path, so none can be
/// accepted with one in it. `root` is the directory the fixture ran in.
fn assert_no_machine_paths(value: &Value, root: &Path) {
    let root = root.to_string_lossy().replace('\\', "/");
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => {
                let normalized = text.replace('\\', "/");
                assert!(
                    !normalized.contains(&root),
                    "snapshot text {text:?} carries the fixture directory"
                );
                assert!(
                    !normalized.contains(":/"),
                    "snapshot text {text:?} looks like an absolute path"
                );
            }
            Value::Array(items) => stack.extend(items),
            Value::Object(entries) => stack.extend(entries.values()),
            _ => {}
        }
    }
}

/// Runs the generated `Data/` tree through the real pipeline and hands back the
/// directory that owns it, the output directory and the report.
async fn convert_generated_data() -> (tempfile::TempDir, PathBuf, converter::PipelineReport) {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();

    let output = directory.path().join("modern");
    let config = cpu_lod_config(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    (directory, output, report)
}

#[tokio::test]
async fn converted_static_model_gltf_matches_snapshot() {
    let (directory, output, _) = convert_generated_data().await;
    let document = glb_json(&output.join("meshes/generated.glb"));

    assert_no_machine_paths(&document, directory.path());
    // The model the pipeline publishes, built from the fixture alone: no field
    // here differs between two runs or two machines, so nothing is redacted.
    // `OPEN_SKYRIM_material` is absent because the material stage writes it only
    // for a material with an extra texture slot or an alpha flag, and the
    // fixture's quad has neither (`crates/converter/src/material.rs`).
    assert_json_snapshot!(document);
}

#[test]
fn world_database_tables_match_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let plugin_path = write_preset_plugin(directory.path());
    let database = directory.path().join("skyrim_world.db");
    converter::EsmParser::convert_plugins(std::slice::from_ref(&plugin_path), &database).unwrap();

    let connection = Connection::open(&database).unwrap();
    // The tables a wrong render comes from: the cell grid the streamer loads,
    // the reference transforms it places, the door references it must not place
    // as statics, and the base records their models come from. `plugins`,
    // `conversion_cache` and `records` are left out: they carry the plugin's
    // absolute path, a conversion timestamp and raw subrecord blobs.
    let snapshot = json!({
        "worldspaces": table_rows(&connection, "worldspaces", "id"),
        "cells": table_rows(&connection, "cells", "id"),
        "references": table_rows(&connection, "\"references\"", "id"),
        "statics": table_rows(&connection, "statics", "id"),
        "npcs": table_rows(&connection, "npcs", "id"),
        "actor_models": table_rows(&connection, "actor_models", "id"),
        "actor_model_parts": table_rows(&connection, "actor_model_parts", "npc_id, slot"),
        "texture_sets": table_rows(&connection, "texture_sets", "id"),
        "landscape_textures": table_rows(&connection, "landscape_textures", "id"),
        "land": table_rows(&connection, "land", "cell_id"),
        "formid_map": table_rows(&connection, "formid_map", "form_id"),
    });

    assert_no_machine_paths(&snapshot, directory.path());
    assert_json_snapshot!(snapshot);
}

#[tokio::test]
async fn conversion_manifest_matches_snapshot() {
    let (directory, output, _) = convert_generated_data().await;
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();

    assert_no_machine_paths(&manifest, directory.path());
    assert_json_snapshot!(manifest, {
        // Content digests carry nothing a reviewer can read, and a texture's
        // digest depends on the basis-universal encoder rather than on the
        // converter; entry keys, output paths and sizes are the reviewable
        // part. The converted meshes and the database have their own snapshots
        // above.
        ".**.source_hash" => "[source_hash]",
        ".**.output_hash" => "[output_hash]",
        ".**.hash" => "[hash]",
    });
}

#[tokio::test]
async fn pipeline_report_matches_snapshot() {
    let (directory, _, report) = convert_generated_data().await;
    let database_cache_key = report
        .database_cache_key
        .as_deref()
        .expect("a fresh database bundle has a cache identity");
    assert_eq!(database_cache_key.len(), 64);
    assert!(
        database_cache_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    let mut report = serde_json::to_value(&report).unwrap();
    // Normalize duration values explicitly: an insta map wildcard also
    // redacts keys, which would hide changes to the measured phase names.
    for field in ["phase_elapsed_ms", "publication_phase_elapsed_ms"] {
        for elapsed in report[field].as_object_mut().unwrap().values_mut() {
            assert!(elapsed.is_u64());
            *elapsed = Value::from("[elapsed_ms]");
        }
    }

    assert_no_machine_paths(&report, directory.path());
    assert_json_snapshot!(report, {
        // Wall-clock time.
        ".elapsed_ms" => "[elapsed_ms]",
        ".lod_elapsed_ms" => "[lod_elapsed_ms]",
        ".publication_elapsed_ms" => "[publication_elapsed_ms]",
        ".archive_timings.*.source_hash_seconds" => "[seconds]",
        ".archive_timings.*.extraction_seconds" => "[seconds]",
        ".archive_timings.*.cache_restore_seconds" => "[seconds]",
        ".archive_timings.*.cache_link_seconds" => "[seconds]",
        ".archive_timings.*.sync_seconds" => "[seconds]",
        ".archive_timings.*.sync_worker_seconds" => "[seconds]",
        ".archive_timings.*.checkpoint_sync_worker_seconds" => "[seconds]",
        // The identity includes the executable and platform. Keep its shape
        // checked above while retaining the cache outcome and work counters.
        ".database_cache_key" => "[database_cache_key]",
        // Artifacts are appended in the order the parallel conversions finish,
        // which is not stable between runs; sorting keeps the same set of
        // published files readable.
        ".artifacts" => insta::sorted_redaction(),
    });
}
