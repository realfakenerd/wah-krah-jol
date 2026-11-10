//! The manifest's extension hashes on real converter output: the Rust writer and the Python
//! test reference (`tests/support/extension_hash_reference.py`) must agree.

use dummy_content::layout;
use std::{fs, path::Path, process::Command};

fn generate_data(root: &Path) {
    layout::prepare_directory(root, false).unwrap();
    let formats = layout::Formats::parse("dds,pex,nif,esm").unwrap();
    layout::generate(root, layout::DEFAULT_SEED, formats).unwrap();
}

async fn convert(config: converter::PipelineConfig) -> converter::pipeline::PipelineReport {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    report
}

fn python() -> Option<&'static str> {
    let executable = ["python", "python3"].into_iter().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    if executable.is_none() {
        eprintln!("skipped the Python cross check: neither `python` nor `python3` is on PATH");
    }
    executable
}

#[tokio::test]
async fn ordinary_conversion_publishes_core_output_only() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("assets");
    generate_data(&data);
    let report = convert(converter::PipelineConfig::new(&data, &output)).await;
    assert!(report.complete, "{:?}", report.warnings);

    let manifest: converter::cache::ConversionManifest =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();
    let ids: Vec<&str> = manifest
        .extensions
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert!(
        ids.is_empty(),
        "ordinary conversion must publish core output only: {ids:?}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();
    assert!(
        json.get("extensions").is_none(),
        "empty extensions are omitted"
    );
    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    for table in [
        "actor_models",
        "actor_model_parts",
        "reference_linked_refs",
        "reference_activate_parents",
        "reference_details",
        "door_links",
        "emittance",
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "missing current core table {table}");
    }
    let references: i64 = connection
        .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
        .unwrap();
    assert!(
        references > 0,
        "real conversion must retain core references"
    );
    drop(connection);
}

/// Checks every extension a data set lists, own or foreign, under a shared asset lock.
///
/// * The converter's own extensions (`converter::extensions::specs`) are recomputed from the folder
///   and must equal the listed entries exactly: hash, identity, ownership and stats.
/// * Every other (finalized) entry goes through the composer, which requires the data set's
///   current non-empty core identity, unique ids, disjoint ownership (nested folders included) and
///   a `sha256` equal to `recompute_hash` of the content on disk. Nothing is minted or dropped.
///   The composer cannot recompute a foreign entry's `stats` or `version` meaning: those are
///   taken as listed and are not independently verified here.
/// * When Python is available, `tests/support/extension_hash_reference.py` must accept every entry.
fn verify_set(set: &Path) -> color_eyre::Result<()> {
    use color_eyre::eyre::ensure;

    let _lock = shared::asset_lock::AssetLock::acquire_shared(set)?;
    let manifest: converter::cache::ConversionManifest =
        serde_json::from_slice(&fs::read(set.join("conversion-manifest.json"))?)?;
    let own_ids: Vec<String> = converter::extensions::specs()
        .iter()
        .map(|spec| spec.id.to_owned())
        .collect();
    let (own, foreign): (Vec<_>, Vec<_>) = manifest
        .extensions
        .iter()
        .cloned()
        .partition(|entry| own_ids.contains(&entry.id));
    ensure!(
        converter::extensions::build_entries(set)? == own,
        "Rust recompute of the converter's own extensions must match the listed entries"
    );
    let mut composed = converter::cache::ConversionManifest::default();
    converter::extensions::refresh_manifest_with_finalized(&mut composed, set, &foreign)?;
    let by_id = |entries: &[shared::extensions::ExtensionEntry]| {
        let mut entries = entries.to_vec();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    };
    ensure!(
        by_id(&composed.extensions) == by_id(&manifest.extensions),
        "the composed list must equal the manifest's, with nothing minted or dropped"
    );
    let Some(python) = python() else {
        return Ok(());
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let result = Command::new(python)
        .arg("-I")
        .arg(&script)
        .arg(set)
        .output()?;
    ensure!(
        result.status.success(),
        "Python reference rejected the manifest:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

/// A finalized entry from another writer: its own table and
/// folder, built from the data set's core identity and listed next to the converter's entries.
#[tokio::test]
async fn a_finalized_foreign_entry_passes_and_a_tampered_one_fails() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("assets");
    generate_data(&data);
    convert(converter::PipelineConfig::new(&data, &output)).await;
    let database = output.join("skyrim_world.db");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let identity = shared::extensions::core_build_identity(&connection)
        .unwrap()
        .expect("the dummy conversion records a core build identity");
    connection
        .execute_batch(
            "CREATE TABLE extra_payload(a INTEGER); INSERT INTO extra_payload VALUES (7);
             CREATE INDEX idx_extra_payload ON extra_payload(a);",
        )
        .unwrap();
    drop(connection);
    fs::create_dir_all(output.join("payload")).unwrap();
    fs::write(output.join("payload/a.bin"), b"payload").unwrap();
    let mut entry = shared::extensions::ExtensionEntry {
        id: "example.payload".to_owned(),
        version: 1,
        owns: shared::extensions::ExtensionOwns {
            tables: vec!["extra_payload".to_owned()],
            indexes: vec!["idx_extra_payload".to_owned()],
            files: vec!["payload/".to_owned()],
            ..Default::default()
        },
        sha256: String::new(),
        built_from: identity,
        stats: Default::default(),
    };
    entry.sha256 = converter::extensions::recompute_hash(&output, &entry).unwrap();
    assert_eq!(
        entry.sha256, "9eaa74826afac6eab8915e2ba2e02f62a36ef70b4336bd48442bc35684fb9f21",
        "the table/index/file stream must retain its pinned encoding"
    );
    let manifest_path = output.join("conversion-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest
        .as_object_mut()
        .unwrap()
        .entry("extensions")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&entry).unwrap());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    verify_set(&output).unwrap();

    fs::write(output.join("payload/a.bin"), b"tampered").unwrap();
    let error = verify_set(&output).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!(
            "extension {} lists sha256 {}",
            entry.id, entry.sha256
        )) && message.contains("but its content hashes to"),
        "changed owned bytes must produce a hash mismatch: {message}"
    );
}

#[test]
fn python_ownership_uses_ascii_case_and_nested_file_rules() {
    let mut cases = Vec::new();
    for (kind, first, second, conflict) in [
        ("tables", "Extra", "eXTRA", true),
        ("indexes", "Idx_Extra", "idx_extra", true),
        (
            "gltf_extensions",
            "EXAMPLE_payload",
            "example_payload",
            false,
        ),
        (
            "gltf_extensions",
            "EXAMPLE_payload",
            "EXAMPLE_payload",
            true,
        ),
        ("tables", "Ä", "ä", false),
        ("files", "Payload/a.bin", "payload/A.BIN", true),
        ("files", "Payload/", "payload/a.bin", true),
        ("files", "Payload/", "PAYLOAD/nested/", true),
        ("files", "payload/", "payload_other/a.bin", false),
        ("files", "Ä/", "ä/a.bin", false),
    ] {
        let entries: Vec<shared::extensions::ExtensionEntry> =
            serde_json::from_value(serde_json::json!([
                {"id": "a", "version": 1, "sha256": "", "built_from": "core",
                 "owns": {(kind): [first, first]}},
                {"id": "b", "version": 1, "sha256": "", "built_from": "core",
                 "owns": {(kind): [second, second]}}
            ]))
            .unwrap();
        let problems = shared::extensions::ownership_problems(&entries);
        assert_eq!(
            !problems.is_empty(),
            conflict,
            "{kind}: {first} vs {second}: {problems:?}"
        );
        cases.push(serde_json::json!({"entries": entries, "conflict": conflict}));
    }
    let duplicate_ids: Vec<shared::extensions::ExtensionEntry> =
        serde_json::from_value(serde_json::json!([
            {"id": "same", "version": 1, "sha256": "", "built_from": "core",
             "owns": {"tables": ["first"]}},
            {"id": "same", "version": 1, "sha256": "", "built_from": "core",
             "owns": {"tables": ["second"]}}
        ]))
        .unwrap();
    assert!(!shared::extensions::ownership_problems(&duplicate_ids).is_empty());
    cases.push(serde_json::json!({"entries": duplicate_ids, "conflict": true}));
    let Some(python) = python() else {
        return;
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let code = r#"
import importlib.util, json, sqlite3, sys
spec = importlib.util.spec_from_file_location('oracle', sys.argv[1])
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
for case in json.loads(sys.argv[2]):
    assert bool(oracle.ownership_problems(case['entries'])) == case['conflict'], case
def entry(id, kind, name):
    return {'id': id, 'owns': {kind: [name]}}
connection = sqlite3.connect(':memory:')
for kind in ('tables', 'indexes'):
    for first, second in [('extra_payload', 'EXTRA_PAYLOAD'),
                          ('idx_extra_payload', 'IDX_EXTRA_PAYLOAD'),
                          ('Mixed_Ä', 'mIXED_Ä'), ('Ä', 'ä'), ('Σ', 'σ'), ('ß', 'ss')]:
        same = bool(connection.execute('SELECT ? = ? COLLATE NOCASE', (first, second)).fetchone()[0])
        a, b = entry('example.a', kind, first), entry('example.b', kind, second)
        assert bool(oracle.ownership_problems([a, b])) == same, (kind, first, second)
        assert bool(oracle.ownership_problems([b, a])) == same, (kind, first, second)
    for code in range(128):
        first, second = 'prefix_' + chr(code), 'prefix_' + chr(code).upper()
        same = bool(connection.execute('SELECT ? = ? COLLATE NOCASE', (first, second)).fetchone()[0])
        assert bool(oracle.ownership_problems([entry('a', kind, first), entry('b', kind, second)])) == same
for first, second in [('Payload/a.bin', 'payload/A.BIN'), ('Payload/', 'payload/a.bin'),
                      ('Payload/', 'PAYLOAD/nested/'), ('Payload/', 'Payload/')]:
    a, b = entry('a', 'files', first), entry('b', 'files', second)
    assert oracle.ownership_problems([a, b]), (first, second)
    assert oracle.ownership_problems([b, a]), (second, first)
for first, second in [('payload/', 'payload_other/a.bin'), ('Ä/', 'ä/a.bin')]:
    assert not oracle.ownership_problems([entry('a', 'files', first), entry('b', 'files', second)])
assert not oracle.ownership_problems([entry('a', 'gltf_extensions', 'Payload'), entry('b', 'gltf_extensions', 'payload')])
assert oracle.ownership_problems([entry('a', 'gltf_extensions', 'Payload'), entry('b', 'gltf_extensions', 'Payload')])
assert oracle.ownership_problems([entry('same', 'tables', 'a'), entry('same', 'tables', 'b')])
for kind in ('tables', 'indexes', 'files', 'gltf_extensions'):
    repeated = {'id': 'a', 'owns': {kind: ['Payload', 'Payload', 'PAYLOAD']}}
    assert not oracle.ownership_problems([repeated])
connection.executescript('CREATE TABLE extra(a); CREATE INDEX idx_extra ON extra(a);')
assert oracle.index_line(connection, 'IDX_EXTRA') == 'index IDX_EXTRA CREATE INDEX idx_extra ON extra(a)'
"#;
    let result = Command::new(python)
        .arg("-B")
        .arg("-I")
        .arg("-c")
        .arg(code)
        .arg(script)
        .arg(serde_json::to_string(&cases).unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// A real data set, not a fixture: `MUDCRAB_EXTENSIONS_SET=<folder> cargo nextest run -p converter
/// --test extensions_hash --run-ignored all`. Every listed entry, the converter's own and any
/// finalized payload, is verified by [`verify_set`].
#[test]
#[ignore = "needs a converted data set: set MUDCRAB_EXTENSIONS_SET"]
fn real_data_set_hashes_agree_in_rust_and_python() {
    let Some(set) = std::env::var_os("MUDCRAB_EXTENSIONS_SET") else {
        eprintln!("skipped: MUDCRAB_EXTENSIONS_SET is unset; no converted data set was supplied");
        return;
    };
    verify_set(Path::new(&set)).unwrap();
}

#[test]
fn generic_gltf_locations_and_pointer_escaping_match_python() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let connection = rusqlite::Connection::open(root.join("skyrim_world.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE lod_build(build_identity TEXT); INSERT INTO lod_build VALUES('core');",
        )
        .unwrap();
    drop(connection);
    fs::create_dir(root.join("meshes")).unwrap();
    let name = "EXAMPLE_payload~/";
    let document = serde_json::json!({
        "extensionsUsed": [name],
        "extensions": {name: {"value": 1}},
        "materials": [{"extensions": {name: {"value": 2}}}],
        "nodes": [{"extensions": {name: {"value": 3}}}],
        "meshes": [{"primitives": [{"extensions": {name: {"value": 4}}}]}],
        "extras": {"key~/": {"extensions": {name: {"value": 5}}}}
    });
    fs::write(
        root.join("meshes/example.gltf"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let mut entry = shared::extensions::ExtensionEntry {
        id: "example.payload".into(),
        version: 1,
        owns: shared::extensions::ExtensionOwns {
            gltf_extensions: vec![name.into(), name.into()],
            ..Default::default()
        },
        sha256: String::new(),
        built_from: "core".into(),
        stats: Default::default(),
    };
    entry.sha256 = converter::extensions::recompute_hash(root, &entry).unwrap();
    let mut manifest = converter::cache::ConversionManifest::default();
    converter::extensions::refresh_manifest_with_finalized(&mut manifest, root, &[entry]).unwrap();
    manifest
        .save(&root.join("conversion-manifest.json"))
        .unwrap();
    verify_set(root).unwrap();
}

#[test]
fn json_numbers_hash_default_serde_parsed_values_in_both_languages() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let entry = shared::extensions::ExtensionEntry {
        id: "example.numbers".into(),
        version: 1,
        owns: shared::extensions::ExtensionOwns {
            gltf_extensions: vec!["EXAMPLE_numbers".into()],
            ..Default::default()
        },
        sha256: String::new(),
        built_from: "core".into(),
        stats: Default::default(),
    };
    let mut cases = Vec::new();
    // Native integers are exact; floats hash their parsed binary64 value. Default
    // serde_json rounding is not always Python float(token) / float_roundtrip.
    for (literal, encoded) in [
        ("1", "i1"),
        ("1.0", "f3ff0000000000000"),
        ("9007199254740992", "i9007199254740992"),
        ("9007199254740993", "i9007199254740993"),
        ("9007199254740994", "i9007199254740994"),
        ("9007199254740993.0", "f4340000000000001"),
        ("-9007199254740992", "i-9007199254740992"),
        ("-9007199254740993", "i-9007199254740993"),
        ("9223372036854775807", "i9223372036854775807"),
        ("-9223372036854775808", "i-9223372036854775808"),
        ("-9223372036854775807", "i-9223372036854775807"),
        ("18446744073709551615", "i18446744073709551615"),
        ("-9223372036854775809", "fc3e0000000000000"),
        ("18446744073709551616", "f43f0000000000000"),
        ("18446744073709551617", "f43f0000000000000"),
        (
            "340282366920938463463374607431768211457",
            "f47f0000000000000",
        ),
        ("18446744073709549568", "i18446744073709549568"),
        ("0", "i0"),
        ("-0", "f8000000000000000"),
        ("-0.0", "f8000000000000000"),
        ("0.0", "f0000000000000000"),
        ("0.1", "f3fb999999999999a"),
        ("1e23", "f44b52d02c7e14af6"),
        ("1.2345678901234567", "f3ff3c0ca428c59fb"),
        ("3.141592653589793", "f400921fb54442d18"),
        ("2.2250738585072014e-308", "f0010000000000000"),
        ("5e-324", "f0000000000000001"),
        ("1e-400", "f0000000000000000"),
        ("-1e-400", "f8000000000000000"),
        ("0e2147483648", "f0000000000000000"),
        ("-0e2147483648", "f8000000000000000"),
        ("1e-2147483648", "f0000000000000000"),
        ("1.7976931348623157e308", "f7fefffffffffffff"),
        ("123456789012345678901.2345", "f441ac53a7e04bcd9"),
        ("0.1234567890123456789012345", "f3fbf9add3746f65f"),
        ("-18446744073709551616", "fc3f0000000000000"),
    ] {
        let value: serde_json::Value = serde_json::from_str(literal).unwrap();
        assert_eq!(
            shared::extensions::encode_json(&value).unwrap(),
            encoded,
            "{literal}"
        );
        let document = format!(
            r#"{{"extensionsUsed":["EXAMPLE_numbers"],"extensions":{{"EXAMPLE_numbers":{literal}}}}}"#
        );
        fs::write(root.join("numbers.gltf"), &document).unwrap();
        let hash = converter::extensions::recompute_hash(root, &entry).unwrap();
        cases.push(serde_json::json!({
            "literal": literal, "encoded": encoded, "document": document, "hash": hash
        }));
    }
    assert_ne!(
        cases[0]["hash"], cases[1]["hash"],
        "1 and 1.0 have different parsed number types"
    );
    assert_ne!(
        cases[2]["hash"], cases[3]["hash"],
        "adjacent large native integers must differ"
    );
    assert_eq!(
        cases[13]["hash"], cases[14]["hash"],
        "beyond-native integer tokens may collapse to the same parsed float"
    );
    let rejected = [
        "NaN",
        "Infinity",
        "-Infinity",
        "1e309",
        "-1e309",
        "1e2147483648",
        "-1e2147483648",
        "1.7976931348623159e308",
        "18446744073709551616e308",
        "01",
        "1.",
        "1e",
    ];
    for literal in rejected {
        assert!(
            serde_json::from_str::<serde_json::Value>(literal).is_err(),
            "accepted invalid or overflowing number {literal}"
        );
    }
    let Some(python) = python() else {
        return;
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let code = r#"
import importlib.util, json, pathlib, sys
spec = importlib.util.spec_from_file_location('oracle', sys.argv[1])
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
root = pathlib.Path(sys.argv[2])
entry = json.loads(sys.argv[3])
for case in json.loads(sys.argv[4]):
    assert oracle.encode_json(oracle.parse_json(case['literal'])) == case['encoded'], case
    (root / 'numbers.gltf').write_text(case['document'], encoding='utf-8')
    assert oracle.extension_hash(root, entry) == case['hash'], case
for literal in json.loads(sys.argv[5]):
    try:
        oracle.parse_json(literal)
    except ValueError:
        pass
    else:
        raise AssertionError(f'accepted invalid or overflowing number {literal}')
for integer in (-(1 << 63) - 1, 1 << 64, 10 ** 400):
    try:
        oracle.encode_json(integer)
    except ValueError as error:
        assert 'use parse_json' in str(error), error
    else:
        raise AssertionError(f'accepted unparsed out-of-range integer {integer}')
"#;
    let result = Command::new(python)
        .args(["-B", "-I", "-c", code])
        .arg(script)
        .arg(root)
        .arg(serde_json::to_string(&entry).unwrap())
        .arg(serde_json::to_string(&cases).unwrap())
        .arg(serde_json::to_string(&rejected).unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn gltf_extensions_used_requires_an_array_in_both_languages() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let entry = shared::extensions::ExtensionEntry {
        id: "example.payload".into(),
        version: 1,
        owns: shared::extensions::ExtensionOwns {
            gltf_extensions: vec!["EXAMPLE_payload".into()],
            ..Default::default()
        },
        sha256: String::new(),
        built_from: "core".into(),
        stats: Default::default(),
    };
    let empty = shared::extensions::ExtensionHasher::new(&entry.id, entry.version).finish();
    let mut cases = Vec::new();
    for used in [
        serde_json::Value::Null,
        serde_json::json!("EXAMPLE_payload_extra"),
        serde_json::json!({"EXAMPLE_payload": true}),
        serde_json::json!(true),
        serde_json::json!(["EXAMPLE_payload"]),
    ] {
        let document = serde_json::json!({
            "extensionsUsed": used,
            "extensions": {"EXAMPLE_payload": {"value": 1}}
        });
        fs::write(
            root.join("example.gltf"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let hash = converter::extensions::recompute_hash(root, &entry).unwrap();
        assert_eq!(hash == empty, !used.is_array(), "{used}");
        cases.push(serde_json::json!({"document": document, "hash": hash}));
    }
    let Some(python) = python() else {
        return;
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let code = r#"
import importlib.util, json, pathlib, sys
spec = importlib.util.spec_from_file_location('oracle', sys.argv[1])
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
root = pathlib.Path(sys.argv[2])
entry = json.loads(sys.argv[3])
for case in json.loads(sys.argv[4]):
    (root / 'example.gltf').write_text(json.dumps(case['document']), encoding='utf-8')
    assert oracle.extension_hash(root, entry) == case['hash'], case
"#;
    let result = Command::new(python)
        .args(["-B", "-I", "-c", code])
        .arg(script)
        .arg(root)
        .arg(serde_json::to_string(&entry).unwrap())
        .arg(serde_json::to_string(&cases).unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn python_gltf_parser_rejects_invalid_lengths() {
    let Some(python) = python() else {
        return;
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let code = r#"
import importlib.util, pathlib, struct, sys, tempfile
spec = importlib.util.spec_from_file_location('oracle', sys.argv[1])
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
with tempfile.TemporaryDirectory() as root:
    path = pathlib.Path(root) / 'example.glb'
    header = lambda magic, version, length, chunk, kind: struct.pack('<IIIII', magic, version, length, chunk, kind)
    valid = header(oracle.GLB_MAGIC, 2, 24, 4, oracle.GLB_JSON_CHUNK) + b'{}  '
    path.write_bytes(valid)
    assert oracle.glb_json(path) == {}
    for malformed in [
        valid[:12],
        header(0, 2, 24, 4, oracle.GLB_JSON_CHUNK) + b'{}  ',
        header(oracle.GLB_MAGIC, 1, 24, 4, oracle.GLB_JSON_CHUNK) + b'{}  ',
        header(oracle.GLB_MAGIC, 2, 28, 4, oracle.GLB_JSON_CHUNK) + b'{}  ',
        header(oracle.GLB_MAGIC, 2, 20, 4, oracle.GLB_JSON_CHUNK) + b'{}  ',
        header(oracle.GLB_MAGIC, 2, 24, 4, 0) + b'{}  ',
        header(oracle.GLB_MAGIC, 2, 24, 0xfffffffc, oracle.GLB_JSON_CHUNK) + b'{}  ',
        header(oracle.GLB_MAGIC, 2, 23, 3, oracle.GLB_JSON_CHUNK) + b'{} ',
    ]:
        path.write_bytes(malformed)
        try:
            oracle.glb_json(path)
        except ValueError:
            pass
        else:
            raise AssertionError(f'accepted malformed GLB: {malformed!r}')
    # A non-JSON chunk after the bounded first JSON chunk is permitted.
    path.write_bytes(header(oracle.GLB_MAGIC, 2, 36, 4, oracle.GLB_JSON_CHUNK)
                     + b'{}  ' + struct.pack('<II', 4, 0x004e4942) + b'data')
    assert oracle.glb_json(path) == {}
    # Both glTF readers must preserve serde_json's number type and default ULP rounding.
    payload = b'[1,1.0,-0,9007199254740993.0]'
    expected = '[i1,f3ff0000000000000,f8000000000000000,f4340000000000001]'
    text_path = pathlib.Path(root) / 'numbers.gltf'
    text_path.write_bytes(payload)
    assert oracle.encode_json(oracle.glb_json(text_path)) == expected
    payload += b' ' * (-len(payload) % 4)
    path.write_bytes(header(oracle.GLB_MAGIC, 2, 20 + len(payload), len(payload), oracle.GLB_JSON_CHUNK)
                     + payload)
    assert oracle.encode_json(oracle.glb_json(path)) == expected
"#;
    let result = Command::new(python)
        .args(["-B", "-I", "-c", code])
        .arg(script)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn python_oracle_retains_the_pinned_hash_streams() {
    let Some(python) = python() else {
        return;
    };
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/extension_hash_reference.py");
    let code = r#"
import importlib.util, json, pathlib, sqlite3, sys, tempfile
spec = importlib.util.spec_from_file_location('oracle', sys.argv[1])
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
with tempfile.TemporaryDirectory() as root:
    root = pathlib.Path(root)
    db = sqlite3.connect(root / oracle.DATABASE)
    db.executescript('''
        CREATE TABLE door_links (ref_id INTEGER PRIMARY KEY, destination_ref_id INTEGER,
                                 pos_x REAL, pos_y REAL, pos_z REAL, note TEXT, data BLOB);
        INSERT INTO door_links VALUES (2, 1, 0.5, -1.25, 1e-7, 'Whiterun \u2713', x'00ff');
        INSERT INTO door_links VALUES (1, 2, 3.0, 4.0, NULL, NULL, NULL);
        CREATE INDEX idx_door_links_destination ON door_links(destination_ref_id);
        CREATE TABLE extra_payload(a INTEGER);
        INSERT INTO extra_payload VALUES (7);
        CREATE INDEX idx_extra_payload ON extra_payload(a);
    ''')
    db.close()
    entry = {'id': 'mudcrab.door_links', 'version': 1,
             'owns': {'tables': ['door_links'], 'indexes': ['idx_door_links_destination']}}
    entry['sha256'] = '5c9f99438d7658c66aafebaa45045f6d553f0d52521309be28b1ef108db20e87'
    assert oracle.extension_hash(root, entry) == entry['sha256']
    (root / oracle.MANIFEST).write_text(json.dumps({'extensions': [entry]}), encoding='utf-8')
    assert not oracle.check(root), 'legacy standalone SQL streams remain checkable'
    (root / 'meshes').mkdir()
    rock = {'asset': {'version': '2.0'}, 'extensionsUsed': ['MUDCRAB_material', 'OPEN_SKYRIM_material'],
            'materials': [{'extensions': {'OPEN_SKYRIM_material': {'shaderFlags1': 1},
                                          'MUDCRAB_material': {'uvScale': [1, 2.5], 'blendSource': 'SRC_ALPHA'}}},
                          {'extensions': {'OPEN_SKYRIM_material': {'shaderFlags1': 2}}}]}
    import json, struct
    payload = json.dumps(rock).encode('utf-8')
    payload += b' ' * (-len(payload) % 4)
    mesh = struct.pack('<III', oracle.GLB_MAGIC, 2, 20 + len(payload))
    mesh += struct.pack('<II', len(payload), oracle.GLB_JSON_CHUNK) + payload
    (root / 'meshes/rock.glb').write_bytes(mesh)
    entry = {'id': 'mudcrab.material', 'version': 1, 'owns': {'gltf_extensions': ['MUDCRAB_material']}}
    assert oracle.extension_hash(root, entry) == 'a4917e5beaf2c06dcdeea549a57f76552bc8ef780476b01bafd338ffd1996b41'
    node = {'extensionsUsed': ['EXAMPLE_payload'],
            'nodes': [{'extensions': {'EXAMPLE_payload': {'value': 1}}}]}
    primitive = {'extensionsUsed': ['EXAMPLE_payload'],
                 'meshes': [{'primitives': [{'extensions': {'EXAMPLE_payload': {'value': 1}}}]}]}
    assert oracle.gltf_extension_value(node, 'EXAMPLE_payload') is not None
    assert oracle.gltf_extension_value(node, 'EXAMPLE_payload') != oracle.gltf_extension_value(primitive, 'EXAMPLE_payload')
    entry['owns']['gltf_extensions'] *= 2
    assert oracle.extension_hash(root, entry) == 'a4917e5beaf2c06dcdeea549a57f76552bc8ef780476b01bafd338ffd1996b41'
    entry['sha256'] = oracle.extension_hash(root, entry)
    (root / oracle.MANIFEST).write_text(json.dumps({'extensions': [entry]}), encoding='utf-8')
    assert not oracle.check(root), 'legacy standalone material streams remain checkable'
    (root / 'payload').mkdir()
    (root / 'payload/a.bin').write_bytes(b'payload')
    entry = {'id': 'example.payload', 'version': 1, 'built_from': 'core',
             'owns': {'tables': ['extra_payload'], 'indexes': ['idx_extra_payload'], 'files': ['payload/']}}
    pinned = '9eaa74826afac6eab8915e2ba2e02f62a36ef70b4336bd48442bc35684fb9f21'
    assert oracle.extension_hash(root, entry) == pinned
    for kind in entry['owns']:
        entry['owns'][kind] *= 2
    assert oracle.extension_hash(root, entry) == pinned
    assert oracle.owned_files(root, ['payload/', 'payload/a.bin']) == ['payload/a.bin']
    for alias in ['payload/./a.bin', 'payload/../payload/a.bin', 'payload//a.bin',
                  'payload\\a.bin', '../outside.bin', '/absolute.bin', 'Q:/absolute.bin', '',
                  'meshes/./rock.glb', 'meshes/../meshes/rock.glb', 'meshes//rock.glb', 'meshes\\rock.glb',
                  'payload/trailing.', 'payload/trailing ', 'payload/control\n.bin']:
        try:
            oracle.owned_files(root, [alias])
        except ValueError:
            pass
        else:
            raise AssertionError(f'accepted noncanonical owned path: {alias!r}')
    for name in ['', 'bad\nname', 'bad\rname', 'bad\x00name', 'bad\x7fname', 'bad\x85name']:
        for kind in ('tables', 'indexes', 'files', 'gltf_extensions'):
            invalid = {'id': 'example.invalid', 'version': 1, 'owns': {kind: [name]}}
            try:
                oracle.extension_hash(root, invalid)
            except ValueError:
                pass
            else:
                raise AssertionError(f'accepted invalid {kind} name: {name!r}')
        try:
            oracle.extension_hash(root, {'id': name, 'version': 1})
        except ValueError:
            pass
        else:
            raise AssertionError(f'accepted invalid id: {name!r}')

    entry['sha256'] = pinned
    manifest = root / oracle.MANIFEST
    manifest.write_text(json.dumps({'extensions': [entry]}), encoding='utf-8')
    assert oracle.check(root), 'finalized payload without core identity must be refused'
    db = sqlite3.connect(root / oracle.DATABASE)
    db.execute('CREATE TABLE lod_build(build_identity)')
    db.commit()
    assert oracle.check(root), 'zero identity rows must not certify a finalized payload'
    db.execute("INSERT INTO lod_build VALUES ('core')")
    db.commit()
    assert not oracle.check(root)
    entry['built_from'] = 'older-core'
    manifest.write_text(json.dumps({'extensions': [entry]}), encoding='utf-8')
    assert oracle.check(root), 'stale finalized payload must be refused'
    entry['built_from'] = 'core'
    manifest.write_text(json.dumps({'extensions': [entry]}), encoding='utf-8')
    db.execute("INSERT INTO lod_build VALUES ('other-core')")
    db.commit()
    try:
        oracle.check(root)
    except ValueError as error:
        assert 'more than one' in str(error), error
    else:
        raise AssertionError('accepted multiple core identity rows')
    db.execute('DELETE FROM lod_build')
    for identity in ('', 7, None):
        db.execute('INSERT INTO lod_build VALUES (?)', (identity,))
        db.commit()
        try:
            problems = oracle.check(root)
        except ValueError as error:
            assert 'must be TEXT' in str(error), error
        else:
            assert problems, f'accepted invalid core identity {identity!r}'
        db.execute('DELETE FROM lod_build')
    db.execute('DROP TABLE lod_build')
    db.execute('CREATE TABLE lod_build(other_column)')
    db.commit()
    manifest.write_text(json.dumps({}), encoding='utf-8')
    assert not oracle.check(root), 'core-only data does not require a core identity'
    db.close()
"#;
    let result = Command::new(python)
        .arg("-B")
        .arg("-I")
        .arg("-c")
        .arg(code)
        .arg(script)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
