//! The interior preset end to end: the `dummy-content gen --with-interior`
//! `Data/` tree through the real pipeline, and the world database it exports.

use dummy_content::{esm, layout};
use std::path::Path;

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

/// Writes the tree `dummy-content gen --with-interior` writes: the default
/// data tree without its plugin, then the preset plugin through the writer the
/// command publishes it with.
fn generate_interior_data(root: &Path) {
    layout::prepare_directory(root, false).unwrap();
    let mut formats = layout::Formats::all();
    // The preset replaces `Skyrim.esm`, as `run_gen` arranges.
    formats.esm = false;
    layout::generate(root, layout::DEFAULT_SEED, formats).unwrap();
    layout::write_plugin(root, &preset_plugin()).unwrap();
}

#[tokio::test]
async fn generated_interior_plugin_converts_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    generate_interior_data(&data);

    let output = directory.path().join("modern");
    let config = converter::PipelineConfig::new(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();

    assert!(report.complete);
    assert_eq!(report.skipped, 0);
    for relative in ["meshes/generated.glb", "skyrim_world.db", "cell_cache.rkyv"] {
        assert!(output.join(relative).is_file(), "missing {relative}");
    }

    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let interior_cells: i64 = connection
        .query_row(
            "SELECT count(*) FROM cells WHERE worldspace_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        interior_cells, 1,
        "the interior cell is not in a worldspace in the exported world"
    );

    let (references, statics): (i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM \"references\"), (SELECT count(*) FROM statics)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (references, statics),
        (3, 3),
        "the static placement and both door bases are exported"
    );

    let references_without_model: i64 = connection
        .query_row(
            "SELECT count(*) FROM \"references\" r LEFT JOIN statics s ON s.id = r.base_form_id
             WHERE s.model_path IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        references_without_model, 0,
        "the static and door references all resolve to exported models"
    );
}
