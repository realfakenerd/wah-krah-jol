//! Original localized IDs survive publication separately from safe runtime text.
mod common;

use converter::{
    AssetPipeline, PipelineConfig,
    config::RecordReader,
    esm::{inhouse, types::ArchivedRecordData},
};
use dummy_content::{esm, inhouse_dialogue as native, layout};
use rusqlite::Connection;
use std::{fs, path::Path};

/// Generate a small native world without depending on authored record schemas.
fn world() -> Vec<u8> {
    esm::plugin(&esm::Plugin {
        author: layout::GENERATED_AUTHOR,
        worldspace: layout::GENERATED_WORLDSPACE,
        cells: &[esm::PRESET_EXTERIOR_CELL],
        model_path: layout::GENERATED_MODEL_PATH,
        diffuse: layout::GENERATED_DIFFUSE_PATH,
        normal_texture: layout::GENERATED_NORMAL_PATH,
    })
    .unwrap()
}

/// STRINGS offsets are relative to their data region; malformed entries stay bounded.
fn strings(entries: &[(u32, &str)], malformed_id: Option<u32>, prefixed: bool) -> Vec<u8> {
    let mut directory = Vec::new();
    let mut payload = Vec::new();
    if let Some(id) = malformed_id {
        directory.extend(id.to_le_bytes());
        if prefixed {
            directory.extend(0u32.to_le_bytes());
            payload.extend(u32::MAX.to_le_bytes());
        } else {
            directory.extend(u32::MAX.to_le_bytes());
        }
    }
    for &(id, text) in entries {
        directory.extend(id.to_le_bytes());
        directory.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
        // These fixtures use ASCII/Latin-1 plus Euro, all representable in CP1252.
        // Keep expected Unicode text unchanged while writing the native English encoding.
        let mut encoded: Vec<u8> = text
            .chars()
            .map(|character| {
                if character == '\u{20AC}' {
                    return 0x80;
                }
                let codepoint = u32::from(character);
                assert!(codepoint < 0x80 || (0xA0..=0xFF).contains(&codepoint));
                u8::try_from(codepoint).unwrap()
            })
            .collect();
        encoded.push(0);
        if prefixed {
            payload.extend(u32::try_from(encoded.len()).unwrap().to_le_bytes());
        }
        payload.extend(encoded);
    }
    [
        u32::try_from(entries.len() + usize::from(malformed_id.is_some()))
            .unwrap()
            .to_le_bytes()
            .to_vec(),
        u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec(),
        directory,
        payload,
    ]
    .concat()
}

/// A selected plugin's loose table must win over the previous record owner's bank.
fn write_strings(data: &Path, plugin: &str, bank: &str, bytes: &[u8]) {
    let folder = data.join("Strings");
    fs::create_dir_all(&folder).unwrap();
    let stem = Path::new(plugin).file_stem().unwrap().to_str().unwrap();
    fs::write(folder.join(format!("{stem}_english.{bank}")), bytes).unwrap();
}

/// One independently framed message name with an unrelated editor ID preceding it.
fn message(id: u32, string_id: u32) -> Vec<u8> {
    native::named(
        b"MESG",
        id,
        &[native::subrecord(b"FULL", &string_id.to_le_bytes())],
    )
}

/// Read the established runtime archive instead of accepting only diagnostic typed data.
fn published(database: &Connection, id: u32) -> ArchivedRecordData {
    let bytes: Vec<u8> = database
        .query_row("SELECT data FROM records WHERE form_id=?", [id], |row| {
            row.get(0)
        })
        .unwrap();
    rkyv::from_bytes::<ArchivedRecordData, rkyv::rancor::Error>(&bytes).unwrap()
}

/// Keep repeated native signatures in physical order, including safe empty text leaves.
fn runtime_fields(database: &Connection, id: u32, tag: &[u8; 4]) -> Vec<Vec<u8>> {
    published(database, id)
        .subrecords
        .into_iter()
        .filter(|field| &field.tag == tag)
        .map(|field| field.data)
        .collect()
}

type LocalizedRow = (u32, String, String, String, u32, Option<String>, String);

/// Publication records every original key and distinguishes missing from resolved-empty.
fn localized(database: &Connection, id: u32) -> Vec<LocalizedRow> {
    database
        .prepare(
            "SELECT field_index,signature,field_name,string_table,string_id,resolved_text,status \
             FROM inhouse_localized_fields WHERE form_id=? ORDER BY field_index",
        )
        .unwrap()
        .query_map([id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Compare public metadata with independently chosen native values and matched roles.
fn expected(
    index: u32,
    signature: &str,
    field: &str,
    bank: &str,
    id: u32,
    text: Option<&str>,
    status: &str,
) -> (u32, String, String, String, u32, Option<String>, String) {
    (
        index,
        signature.to_owned(),
        field.to_owned(),
        bank.to_owned(),
        id,
        text.map(str::to_owned),
        status.to_owned(),
    )
}

/// Retain source bytes and source winning priority independently of global FormID slots.
fn source(database: &Connection, id: u32) -> (u32, u32, Vec<u8>) {
    database
        .query_row(
            "SELECT source_form_id,load_order,payload FROM inhouse_source_records WHERE form_id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

/// Fast actual publication preserves missing, resolved and repeated fields and original IDs.
#[test]
fn localized_fields_preserve_zero_missing_resolved_and_repeated_keys() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    fs::create_dir(&data).unwrap();
    let generated = world();
    fs::write(data.join("Skyrim.esm"), &generated).unwrap();
    // Missing17 is first so the pre-fix RED is a canonical-field assertion,
    // before any query against the newly introduced provenance table.
    let ids = [17, 0, 18, 100, 101, 42, 100];
    let mut records = Vec::new();
    for (index, string_id) in ids.into_iter().enumerate() {
        records.extend(message(
            0x0100_3500 + u32::try_from(index).unwrap(),
            string_id,
        ));
    }
    let mut response_fields = Vec::new();
    for (index, string_id) in [17u32, 100, 0].into_iter().enumerate() {
        let mut header = [0u8; 24];
        header[12] = u8::try_from(index + 1).unwrap();
        response_fields.push(native::subrecord(b"TRDT", &header));
        response_fields.push(native::subrecord(b"NAM1", &string_id.to_le_bytes()));
    }
    response_fields.push(native::subrecord(b"RNAM", &18u32.to_le_bytes()));
    let info = native::named(b"INFO", 0x0100_3510, &response_fields);
    records.extend(&info);
    fs::write(
        data.join("LocalizedFields.esp"),
        native::plugin(&generated, &["Skyrim.esm"], 0x80, &records),
    )
    .unwrap();
    // Even a physical zero-key entry must never turn the null sentinel into invented text.
    write_strings(
        &data,
        "LocalizedFields.esp",
        "strings",
        &strings(
            &[
                (0, "Forbidden zero-key display text"),
                (100, "Resolved STRINGS résumé"),
                (101, ""),
            ],
            Some(42),
            false,
        ),
    );
    write_strings(
        &data,
        "LocalizedFields.esp",
        "ilstrings",
        &strings(&[(100, "Resolved ILSTRINGS €")], Some(17), true),
    );
    let plugins = temp.path().join("plugins.txt");
    fs::write(&plugins, "*Skyrim.esm\n*LocalizedFields.esp\n").unwrap();
    let output = temp.path().join("bundle");
    inhouse::export_record_bundle_typed(&data, &plugins, &output, &[*b"MESG", *b"INFO"]).unwrap();
    let database = Connection::open(output.join("skyrim_world.db")).unwrap();
    for (index, string_id) in ids.into_iter().enumerate() {
        let rid = 0x0100_3500 + u32::try_from(index).unwrap();
        let text = match string_id {
            100 => Some("Resolved STRINGS résumé"),
            101 => Some(""),
            _ => None,
        };
        let status = if string_id == 0 {
            "null_id"
        } else if text.is_some() {
            "resolved"
        } else {
            "missing"
        };
        let runtime = [text.unwrap_or_default().as_bytes(), &[0]].concat();
        assert_eq!(runtime_fields(&database, rid, b"FULL"), [runtime]);
        assert_eq!(
            localized(&database, rid),
            [expected(
                1, "FULL", "name", "strings", string_id, text, status
            )]
        );
        let framed = message(rid, string_id);
        assert_eq!(source(&database, rid), (rid, 1, framed[24..].to_vec()));
    }
    assert_eq!(
        runtime_fields(&database, 0x0100_3510, b"NAM1"),
        [
            b"\0".to_vec(),
            "Resolved ILSTRINGS €\0".as_bytes().to_vec(),
            b"\0".to_vec()
        ]
    );
    assert_eq!(
        runtime_fields(&database, 0x0100_3510, b"RNAM"),
        [b"\0".to_vec()]
    );
    assert_eq!(
        localized(&database, 0x0100_3510),
        [
            expected(2, "NAM1", "response_text", "ilstrings", 17, None, "missing"),
            expected(
                4,
                "NAM1",
                "response_text",
                "ilstrings",
                100,
                Some("Resolved ILSTRINGS €"),
                "resolved"
            ),
            expected(6, "NAM1", "response_text", "ilstrings", 0, None, "null_id"),
            expected(7, "RNAM", "prompt", "strings", 18, None, "missing"),
        ]
    );
    assert_eq!(source(&database, 0x0100_3510).2, info[24..]);
    let diagnostics: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("inhouse-reader-diagnostics.json")).unwrap())
            .unwrap();
    assert_eq!(diagnostics["adapter"]["localizedfields.esp"][0], 4);
    assert_eq!(
        diagnostics["adapter"]["localizedfields.esp"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        diagnostics["adapter"]["localizedfields.esp"][1]
            .as_str()
            .unwrap()
            .len()
            < 1024
    );
    assert!(
        diagnostics["adapter"]["localizedfields.esp"][1]
            .as_str()
            .unwrap()
            .contains("malformed strings entry for localized ID 42")
    );
    assert_eq!(diagnostics["adapter_notices"]["localizedfields.esp"][0], 3);
    assert!(
        diagnostics["adapter_notices"]["localizedfields.esp"][1]
            .as_str()
            .unwrap()
            .contains("missing localized ID 17")
    );
}

/// English plugin/bank bytes use CP1252 even if they also form valid UTF-8; VMAD uses UTF-8.
#[test]
fn english_text_encoding_is_explicit_and_distinct_from_vmad() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path();
    let data = directory.join("Data");
    fs::create_dir(&data).unwrap();
    let generated = world();
    fs::write(data.join("Skyrim.esm"), &generated).unwrap();
    let vmad = [
        5u16.to_le_bytes().as_slice(),
        2u16.to_le_bytes().as_slice(),
        1u16.to_le_bytes().as_slice(),
        2u16.to_le_bytes().as_slice(),
        &[0xC3, 0xA9, 0],
        0u16.to_le_bytes().as_slice(),
    ]
    .concat();
    let inline = native::named(
        b"ACTI",
        0x0100_3F01,
        &[
            native::subrecord(b"VMAD", &vmad),
            native::subrecord(b"FULL", &[0xC3, 0xA9, 0]),
        ],
    );
    fs::write(
        data.join("InlineEncoding.esp"),
        native::plugin(&generated, &["Skyrim.esm"], 0, &inline),
    )
    .unwrap();
    fs::write(
        data.join("BankEncoding.esp"),
        native::plugin(
            &generated,
            &["Skyrim.esm"],
            0x80,
            &message(0x0100_3F02, 100),
        ),
    )
    .unwrap();
    let bank = [
        1u32.to_le_bytes().as_slice(),
        3u32.to_le_bytes().as_slice(),
        100u32.to_le_bytes().as_slice(),
        0u32.to_le_bytes().as_slice(),
        &[0xC3, 0xA9, 0],
    ]
    .concat();
    write_strings(&data, "BankEncoding.esp", "strings", &bank);
    let list = directory.join("plugins.txt");
    fs::write(
        &list,
        "*Skyrim.esm\n*InlineEncoding.esp\n*BankEncoding.esp\n",
    )
    .unwrap();
    let output = directory.join("encoding-output");
    inhouse::export_record_bundle(&data, &list, &output).unwrap();
    let db = Connection::open(output.join("skyrim_world.db")).unwrap();
    let expected = "\u{00C3}\u{00A9}";
    assert_eq!(
        runtime_fields(&db, 0x0100_3F01, b"FULL"),
        vec![[expected.as_bytes(), &[0]].concat()]
    );
    assert_eq!(
        runtime_fields(&db, 0x0200_3F02, b"FULL"),
        vec![[expected.as_bytes(), &[0]].concat()]
    );
    let resolved: String = db
        .query_row(
            "SELECT resolved_text FROM inhouse_localized_fields WHERE form_id=?",
            [0x0200_3F02u32],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolved, expected);
    assert_eq!(runtime_fields(&db, 0x0100_3F01, b"VMAD"), vec![vmad]);
    let paths = converter::esm::read_plugins_txt(&list, &data).unwrap();
    let order = converter::esm::load_order::LoadOrder::read(&paths).unwrap();
    let decoded = converter::records::read_plugins(&paths, &order).unwrap();
    let record = &decoded.records[&0x0100_3F01];
    let script = record
        .fields
        .iter()
        .find(|field| field.signature == *b"VMAD")
        .unwrap();
    assert!(
        serde_json::to_string(&script.value)
            .unwrap()
            .contains("\"\u{00E9}\""),
        "VMAD script name uses its independent UTF-8 encoding"
    );
    let payload: Vec<u8> = db
        .query_row(
            "SELECT payload FROM inhouse_source_records WHERE form_id=?",
            [0x0100_3F01u32],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        payload,
        inline[24..],
        "original ambiguous bytes remain independently preserved"
    );
}

/// Readable banks retain winning IDs and bound missing-text notices across full and light slots.
#[tokio::test]
async fn winning_full_and_light_banks_preserve_ids_and_bound_missing_text_notices() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(
        &data,
        layout::DEFAULT_SEED,
        layout::Formats::parse("dds,nif,pex,esm").unwrap(),
    )
    .unwrap();
    let generated = fs::read(data.join("Skyrim.esm")).unwrap();
    let earlier = [message(0x0100_3500, 100), message(0x0100_3501, 100)].concat();
    fs::write(
        data.join("EarlierFull.esm"),
        native::plugin(&generated, &["Skyrim.esm"], 0x81, &earlier),
    )
    .unwrap();
    fs::write(
        data.join("Filler.esm"),
        native::plugin(&generated, &["Skyrim.esm"], 1, &[]),
    )
    .unwrap();
    fs::write(
        data.join("FirstLight.esl"),
        native::plugin(
            &generated,
            &["Skyrim.esm"],
            0x280,
            &[message(0x0100_0ABA, 100), message(0x0100_0ABB, 0)].concat(),
        ),
    )
    .unwrap();
    let mut winning = [
        message(0x0100_3500, 17),
        message(0x0200_0ABA, 18),
        message(0x0300_3501, 100),
        native::record(
            b"CELL",
            0x0300_3510,
            0,
            &[
                native::subrecord(b"FULL", &0u32.to_le_bytes()),
                native::subrecord(b"DATA", &1u16.to_le_bytes()),
            ]
            .concat(),
        ),
    ]
    .concat();
    for index in 0..128 {
        winning.extend(message(0x0300_3600 + index, 17));
    }
    fs::write(
        data.join("WinnerFull.esp"),
        native::plugin(
            &generated,
            &["Skyrim.esm", "EarlierFull.esm", "FirstLight.esl"],
            0x80,
            &winning,
        ),
    )
    .unwrap();
    let second_light = [message(0x0100_3500, 17), message(0x0400_0ABA, 100)].concat();
    fs::write(
        data.join("SecondLight.esl"),
        native::plugin(
            &generated,
            &[
                "Skyrim.esm",
                "EarlierFull.esm",
                "FirstLight.esl",
                "WinnerFull.esp",
            ],
            0x280,
            &second_light,
        ),
    )
    .unwrap();
    write_strings(
        &data,
        "EarlierFull.esm",
        "strings",
        &strings(
            &[
                (17, "Stale full-owner text"),
                (100, "Earlier surviving neighbor"),
            ],
            None,
            false,
        ),
    );
    write_strings(
        &data,
        "FirstLight.esl",
        "strings",
        &strings(
            &[
                (0, "Forbidden light zero text"),
                (18, "Stale light-owner text"),
                (100, "Earlier losing light text"),
            ],
            None,
            false,
        ),
    );
    write_strings(
        &data,
        "WinnerFull.esp",
        "strings",
        &strings(&[(100, "Winning full text")], None, false),
    );
    write_strings(
        &data,
        "SecondLight.esl",
        "strings",
        &strings(&[(100, "Distinct second-light text")], None, false),
    );
    let plugins = temp.path().join("plugins.txt");
    fs::write(&plugins, "*Skyrim.esm\n*EarlierFull.esm\n*Filler.esm\n*FirstLight.esl\n*WinnerFull.esp\n*SecondLight.esl\n").unwrap();
    let selected = converter::esm::read_plugins_txt(&plugins, &data).unwrap();
    let names = selected
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "Skyrim.esm",
            "EarlierFull.esm",
            "Filler.esm",
            "FirstLight.esl",
            "WinnerFull.esp",
            "SecondLight.esl"
        ],
        "fixture preserves deliberate full/light priorities after normalization"
    );
    let output = temp.path().join("pack");
    let mut config = PipelineConfig::new(&data, &output);
    config.plugins_file = Some(plugins);
    config.record_reader = RecordReader::Inhouse;
    config.no_lod = true;
    config.cpu_jobs = 2;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = AssetPipeline::run_async(config, tx).await.unwrap();
    drain.await.unwrap();
    common::assert_reader_notice_publication(&report, &output);
    let database = Connection::open(output.join("skyrim_world.db")).unwrap();
    for (rid, raw, priority, key, text, status) in [
        (0x0100_3500, 0x0100_3500, 5, 17, None, "missing"),
        (0xFE00_0ABA, 0x0200_0ABA, 4, 18, None, "missing"),
        (0xFE00_0ABB, 0x0100_0ABB, 3, 0, None, "null_id"),
        (
            0x0300_3501,
            0x0300_3501,
            4,
            100,
            Some("Winning full text"),
            "resolved",
        ),
        (
            0xFE00_1ABA,
            0x0400_0ABA,
            5,
            100,
            Some("Distinct second-light text"),
            "resolved",
        ),
        (
            0x0100_3501,
            0x0100_3501,
            1,
            100,
            Some("Earlier surviving neighbor"),
            "resolved",
        ),
    ] {
        assert_eq!(
            runtime_fields(&database, rid, b"FULL"),
            [[text.unwrap_or_default().as_bytes(), &[0]].concat()]
        );
        assert_eq!(
            localized(&database, rid),
            [expected(1, "FULL", "name", "strings", key, text, status)]
        );
        let framed = message(raw, key);
        assert_eq!(
            source(&database, rid),
            (raw, priority, framed[24..].to_vec())
        );
    }
    for index in 0..128 {
        let rid = 0x0300_3600 + index;
        assert_eq!(runtime_fields(&database, rid, b"FULL"), [b"\0".to_vec()]);
        assert_eq!(
            localized(&database, rid),
            [expected(1, "FULL", "name", "strings", 17, None, "missing")]
        );
    }
    // A retained empty wire field must still project an unresolved runtime name as SQL NULL.
    assert_eq!(
        runtime_fields(&database, 0x0300_3510, b"FULL"),
        [b"\0".to_vec()]
    );
    let interior_name: Option<String> = database
        .query_row(
            "SELECT interior_name FROM cells WHERE id=?",
            [0x0300_3510u32],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(interior_name, None);
    let diagnostics: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("inhouse-reader-diagnostics.json")).unwrap())
            .unwrap();
    for (plugin, count) in [("winnerfull.esp", 129), ("secondlight.esl", 1)] {
        assert!(diagnostics["adapter"].get(plugin).is_none());
        assert_eq!(diagnostics["adapter_notices"][plugin][0], count);
        let summary = diagnostics["adapter_notices"][plugin].as_array().unwrap();
        assert_eq!(
            summary.len(),
            2,
            "retain one bounded example, not every record"
        );
        assert!(!summary[1].as_str().unwrap().is_empty());
        assert!(summary[1].as_str().unwrap().len() < 1024);
        let notices: Vec<_> = report
            .notices
            .iter()
            .filter(|notice| {
                notice.contains(plugin)
                    && notice.contains("localized IDs missing from readable string tables")
            })
            .collect();
        assert_eq!(notices.len(), 1, "{:?}", report.notices);
        assert!(notices[0].contains(&format!("{count} localized IDs")));
        assert!(notices[0].contains("first:"));
        assert!(notices[0].len() < 2304);
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MoveSqlCell {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

type MoveSqlRows = Vec<Vec<MoveSqlCell>>;
type MoveSqlTables = std::collections::BTreeMap<String, MoveSqlRows>;

/// Compare every SQL cell with its storage type, including physical index tables.
fn move_sql_snapshot(database: &Connection) -> (MoveSqlRows, MoveSqlTables) {
    /// Sort rows independently of SQLite's physical row traversal order.
    fn scan(database: &Connection, sql: &str) -> Vec<Vec<MoveSqlCell>> {
        let mut statement = database.prepare(sql).unwrap();
        let columns = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        let mut found = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            let cells = (0..columns)
                .map(|index| match row.get_ref(index).unwrap() {
                    rusqlite::types::ValueRef::Null => MoveSqlCell::Null,
                    rusqlite::types::ValueRef::Integer(v) => MoveSqlCell::Integer(v),
                    rusqlite::types::ValueRef::Real(v) => MoveSqlCell::Real(v.to_bits()),
                    rusqlite::types::ValueRef::Text(v) => MoveSqlCell::Text(v.to_vec()),
                    rusqlite::types::ValueRef::Blob(v) => MoveSqlCell::Blob(v.to_vec()),
                })
                .collect::<Vec<_>>();
            found.push(cells);
        }
        found.sort();
        found
    }
    let schema = scan(database, "SELECT type,name,tbl_name,sql FROM sqlite_schema");
    let names = database
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let tables = names
        .into_iter()
        .map(|name| {
            let escaped = name.replace('"', "\"\"");
            let rows = scan(database, &format!("SELECT * FROM \"{escaped}\""));
            (name, rows)
        })
        .collect();
    (schema, tables)
}

/// Compare typed records by identity while rejecting duplicate output rows.
fn move_typed_snapshot(path: &Path) -> std::collections::BTreeMap<u32, serde_json::Value> {
    let mut result = std::collections::BTreeMap::new();
    for line in fs::read_to_string(path).unwrap().lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let id = u32::try_from(row["form_id"].as_u64().unwrap()).unwrap();
        assert!(result.insert(id, row).is_none(), "duplicate typed identity");
    }
    result
}

#[test]
/// A moved database retains all typed facts, SQL tables, localization and cache bytes.
fn moved_localized_projection_complete_public_bundle_and_work_copy_remain_exact() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let data = root.join("original-data");
    fs::create_dir(&data).unwrap();
    let generated = world();
    fs::write(data.join("Skyrim.esm"), &generated).unwrap();
    let ids = [100u32, 0, 17, 101];
    let mut body = Vec::new();
    for (index, string_id) in ids.into_iter().enumerate() {
        body.extend(message(0x0100_5500 + index as u32, string_id));
    }
    let native_plugin = native::plugin(&generated, &["Skyrim.esm"], 0x80, &body);
    fs::write(data.join("MoveFixture.esp"), &native_plugin).unwrap();
    write_strings(
        &data,
        "MoveFixture.esp",
        "strings",
        &strings(
            &[(0, "Forbidden zero"), (100, "Resolved \u{20ac}"), (101, "")],
            None,
            false,
        ),
    );
    let plugins = root.join("original-plugins.txt");
    fs::write(&plugins, "*Skyrim.esm\n*MoveFixture.esp\n").unwrap();
    let originals = [
        data.join("Skyrim.esm"),
        data.join("MoveFixture.esp"),
        data.join("Strings/MoveFixture_english.strings"),
        plugins.clone(),
    ]
    .into_iter()
    .map(|path| {
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect::<Vec<_>>();
    let schema =
        converter::records::schema_format::Schema::parse(converter::records::SCHEMA_BYTES).unwrap();
    let signatures = schema
        .records
        .iter()
        .map(|record| record.signature.as_bytes().try_into().unwrap())
        .collect::<Vec<[u8; 4]>>();
    assert!(signatures.contains(b"MESG"));
    let normal = root.join("default-output");
    let copied = root.join("copied-output");
    let work = root.join("fresh-database-work");
    let a = inhouse::export_record_bundle_typed(&data, &plugins, &normal, &signatures).unwrap();
    let b = inhouse::export_record_bundle_typed_with_database_work_dir(
        &data,
        &plugins,
        &copied,
        &signatures,
        Some(&work),
    )
    .unwrap();
    assert_eq!(a, b);
    for (path, bytes) in originals {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "original mutated: {}",
            path.display()
        );
    }
    let normal_db = Connection::open_with_flags(
        normal.join("skyrim_world.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let copied_db = Connection::open_with_flags(
        copied.join("skyrim_world.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(move_sql_snapshot(&normal_db), move_sql_snapshot(&copied_db));
    let normal_typed = move_typed_snapshot(&normal.join("typed-records.jsonl"));
    let copied_typed = move_typed_snapshot(&copied.join("typed-records.jsonl"));
    assert_eq!(normal_typed, copied_typed);
    let source_count: usize = copied_db
        .query_row("SELECT count(*) FROM inhouse_source_records", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(copied_typed.len(), source_count);
    assert!(
        copied_db
            .query_row("SELECT count(*) FROM record_overrides", [], |row| row
                .get::<_, u64>(0))
            .unwrap()
            >= 4
    );
    for (index, string_id) in ids.into_iter().enumerate() {
        let rid = 0x0100_5500 + index as u32;
        let text = match string_id {
            100 => Some("Resolved \u{20ac}"),
            101 => Some(""),
            _ => None,
        };
        let status = if string_id == 0 {
            "null_id"
        } else if text.is_some() {
            "resolved"
        } else {
            "missing"
        };
        assert_eq!(
            runtime_fields(&copied_db, rid, b"FULL"),
            vec![runtime_string_for_move_test(text.unwrap_or_default())]
        );
        assert_eq!(
            localized(&copied_db, rid),
            vec![expected(
                1, "FULL", "name", "strings", string_id, text, status
            )]
        );
        assert_eq!(
            source(&copied_db, rid),
            (rid, 1, message(rid, string_id)[24..].to_vec())
        );
        let typed = &copied_typed[&rid];
        assert_eq!(
            typed["fields"][1]["value"],
            serde_json::json!({"LocalizedString":string_id})
        );
        assert_eq!(
            typed["fields"][1]["canonical_bytes"],
            serde_json::json!(string_id.to_le_bytes())
        );
        assert_eq!(typed["localization"]["1"]["status"], status);
        assert_eq!(typed["localization"]["1"]["text"], serde_json::json!(text));
        assert_eq!(
            typed["source_payload_sha256"],
            format!("{:x}", Sha256::digest(&message(rid, string_id)[24..]))
        );
    }
    for name in [
        "cell_cache.rkyv",
        "cell_cache_preserved.rkyv",
        "records-schema.json",
        "record-reader.json",
    ] {
        assert_eq!(
            fs::read(normal.join(name)).unwrap(),
            fs::read(copied.join(name)).unwrap()
        );
    }
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let source_bytes = fs::read(work.join(name)).unwrap();
        let final_bytes = fs::read(copied.join(name)).unwrap();
        assert_eq!(source_bytes, final_bytes);
        assert_eq!(Sha256::digest(&source_bytes), Sha256::digest(&final_bytes));
    }
    let diagnostics: serde_json::Value =
        serde_json::from_slice(&fs::read(copied.join("inhouse-reader-diagnostics.json")).unwrap())
            .unwrap();
    let normal_diagnostics: serde_json::Value =
        serde_json::from_slice(&fs::read(normal.join("inhouse-reader-diagnostics.json")).unwrap())
            .unwrap();
    assert_eq!(diagnostics, normal_diagnostics);
    assert!(diagnostics["adapter"].get("movefixture.esp").is_none());
    assert_eq!(diagnostics["adapter_notices"]["movefixture.esp"][0], 1);
    assert!(
        diagnostics["adapter_notices"]["movefixture.esp"][1]
            .as_str()
            .unwrap()
            .contains("missing localized ID 17")
    );
    assert!(!work.join("typed-records.jsonl").exists());
    assert!(!work.join("cell_cache.rkyv").exists());
}

/// Localized native IDs are projected to terminated UTF-8 runtime strings.
fn runtime_string_for_move_test(text: &str) -> Vec<u8> {
    [text.as_bytes(), &[0]].concat()
}

#[test]
/// Compare retained facts and both cache policies across full and deferred-view diagnostics.
fn archive_only_preserves_complete_facts_and_caches_while_explicit_views_defer() {
    use converter::esm::inhouse::RecordBundleMode;
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let retained = temp.path();
    let data = retained.join("original-data");
    fs::create_dir(&data).unwrap();
    let generated = world(); // Actual exterior CELL/LAND/reference and authored terrain/LTEX.
    fs::write(data.join("Skyrim.esm"), &generated).unwrap();
    let mut body = Vec::new();
    for (id, key) in [(0x0100_5500, 100), (0x0100_5501, 0), (0x0100_5502, 17)] {
        body.extend(message(id, key));
    }
    let mut vmad = Vec::new();
    vmad.extend(5u16.to_le_bytes());
    vmad.extend(2u16.to_le_bytes());
    vmad.extend(1u16.to_le_bytes());
    vmad.extend(12u16.to_le_bytes());
    vmad.extend(b"ArchiveProbe");
    vmad.push(0);
    vmad.extend(0u16.to_le_bytes());
    let before = native::named(b"ACTI", 0x0100_5600, &[native::subrecord(b"VMAD", &vmad)]);
    let broken_payload = [5u32.to_le_bytes().as_slice(), &[0x78, 0x9c, 0]].concat();
    let broken = native::record(b"STAT", 0x0100_5601, 0x40000, &broken_payload);
    let after = native::named(
        b"STAT",
        0x0100_5602,
        &[native::subrecord(b"OBND", &[0; 12])],
    );
    body.extend(before);
    body.extend(broken);
    body.extend(after);
    // Remove the otherwise available terrain texture. Preserved ID4 versus runtime placeholder0
    // forces the test to exercise the canonical LTEX lookup and both distinct cache policies.
    body.extend(native::record(b"LTEX", 4, 0x20, &[]));
    let original_patch = native::plugin(&generated, &["Skyrim.esm"], 0x80, &body);
    fs::write(data.join("ArchiveProbe.esp"), &original_patch).unwrap();
    write_strings(
        &data,
        "ArchiveProbe.esp",
        "strings",
        &strings(
            &[
                (0, "Never use null key"),
                (100, "Resolved \u{20ac} archive"),
            ],
            None,
            false,
        ),
    );
    let order = retained.join("original-order.txt");
    fs::write(&order, "*Skyrim.esm\n*ArchiveProbe.esp\n").unwrap();
    let originals = [
        data.join("Skyrim.esm"),
        data.join("ArchiveProbe.esp"),
        data.join("Strings/ArchiveProbe_english.strings"),
        order.clone(),
    ]
    .into_iter()
    .map(|path| {
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect::<Vec<_>>();
    let schema =
        converter::records::schema_format::Schema::parse(converter::records::SCHEMA_BYTES).unwrap();
    let signatures = schema
        .records
        .iter()
        .map(|record| record.signature.as_bytes().try_into().unwrap())
        .collect::<Vec<[u8; 4]>>();
    let full = retained.join("full-default-output");
    let archive = retained.join("archive-output");
    let copied = retained.join("archive-copied-output");
    let work = retained.join("archive-database-work");
    let count = inhouse::export_record_bundle_typed(&data, &order, &full, &signatures).unwrap();
    assert!(count > 0, "a nonempty cache pair is required");
    for (output, working) in [(&archive, None), (&copied, Some(work.as_path()))] {
        assert_eq!(
            inhouse::export_record_bundle_typed_with_options(
                &data,
                &order,
                output,
                &signatures,
                working,
                RecordBundleMode::RecordArchiveOnly
            )
            .unwrap(),
            count
        );
        let full_db = Connection::open(full.join("skyrim_world.db")).unwrap();
        let archive_db = Connection::open(output.join("skyrim_world.db")).unwrap();
        let (full_schema, full_tables) = move_sql_snapshot(&full_db);
        let (archive_schema, archive_tables) = move_sql_snapshot(&archive_db);
        assert_eq!(
            full_schema, archive_schema,
            "no SQL object silently omitted"
        );
        let deferred = [
            "worldspaces",
            "cells",
            "references",
            "exterior_spatial",
            "land",
            "statics",
            "lights",
            "npcs",
            "actor_models",
            "actor_model_parts",
            "movement_types",
            "race_movement_links",
            "race_animation_sources",
            "movement_game_settings",
            "waters",
            "texture_sets",
            "landscape_textures",
            "grass_types",
            "landscape_texture_grasses",
            "scripts",
            "script_properties",
            "script_property_values",
            "room_bounds",
            "room_portals",
            "room_links",
            "reference_rooms",
            "room_graph_stats",
            "projection_omission_stats",
            "projection_omission_samples",
            "cell_environment",
            "cell_regions",
            "cell_lighting",
            "worldspace_environment",
            "lighting_templates",
            "weathers",
            "weather_colors",
            "weather_directional_ambient",
            "climates",
            "climate_weathers",
            "region_weathers",
            "image_spaces",
            "reference_linked_refs",
            "reference_activate_parents",
            "reference_details",
            "reference_light_data",
            "door_links",
            "emittance",
        ];
        for (name, rows) in &full_tables {
            if deferred.contains(&name.as_str()) {
                assert!(
                    archive_tables[name].is_empty(),
                    "view was not deferred: {name}"
                );
            } else if name.starts_with("exterior_spatial_") {
                // Physical RTree structures are explicitly scoped: empty-tree nodes differ from full.
                assert!(
                    [
                        "exterior_spatial_node",
                        "exterior_spatial_parent",
                        "exterior_spatial_rowid"
                    ]
                    .contains(&name.as_str())
                );
                if name == "exterior_spatial_node" {
                    assert_eq!(
                        archive_tables[name].len(),
                        1,
                        "empty RTree root node retained"
                    );
                } else {
                    assert!(
                        archive_tables[name].is_empty(),
                        "empty RTree mapping retained"
                    );
                }
            } else {
                assert_eq!(
                    *rows, archive_tables[name],
                    "unexpected nondeferred SQL change: {name}"
                );
            }
        }
        for table in ["cells", "references", "scripts"] {
            assert!(
                !full_tables[table].is_empty(),
                "asymmetric full fixture must populate {table}"
            );
        }
        let full_typed = move_typed_snapshot(&full.join("typed-records.jsonl"));
        let archive_typed = move_typed_snapshot(&output.join("typed-records.jsonl"));
        assert_eq!(
            full_typed, archive_typed,
            "entire typed DOM, occurrence order and all metadata"
        );
        assert!(
            archive_typed
                .values()
                .all(|row| row.as_object().unwrap().len() == 20)
        );
        for id in [0x0100_5600u32, 0x0100_5602] {
            assert!(archive_typed.contains_key(&id), "broken-mod neighbor lost");
            let present: bool = archive_db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE form_id=?1)",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "canonical neighbor lost");
        }
        let broken_count: (u32, u32) = archive_db.query_row(
            "SELECT count(*),sum(status='skipped') FROM record_overrides WHERE source_form_id=?1",
            [0x0100_5601u32], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(
            broken_count,
            (1, 1),
            "complete physical skipped history retained"
        );
        for status in ["resolved", "null_id", "missing"] {
            let present: bool = archive_db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM inhouse_localized_fields WHERE status=?1)",
                    [status],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "localized key policy lost: {status}");
        }
        for name in [
            "cell_cache.rkyv",
            "cell_cache_preserved.rkyv",
            "records-schema.json",
            "inhouse-reader-diagnostics.json",
        ] {
            assert_eq!(
                fs::read(full.join(name)).unwrap(),
                fs::read(output.join(name)).unwrap(),
                "preserved artifact {name}"
            );
        }
        assert_ne!(
            fs::read(output.join("cell_cache.rkyv")).unwrap(),
            fs::read(output.join("cell_cache_preserved.rkyv")).unwrap(),
            "missing LTEX must retain distinct preserved/runtime policies"
        );
        let scope: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("record-bundle-scope.json")).unwrap())
                .unwrap();
        assert_eq!(
            scope["status"],
            "COMPLETE_DIAGNOSTIC_ARCHIVE_RUNTIME_VIEWS_DEFERRED"
        );
        assert_eq!(scope["runtime_ready"], false);
        assert_eq!(scope["runtime_views_materialized"], false);
        assert_eq!(scope["whole_runtime_SQL_equivalence_claim"], false);
        let preserved = scope["preserved_sql_tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        let deferred_manifest = scope["deferred_runtime_tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(preserved.is_disjoint(&deferred_manifest));
        // R6 creates add-on provenance even when empty; archive scope must classify it.
        assert!(preserved.contains("inhouse_schema_addons"));
        assert!(archive_tables["inhouse_schema_addons"].is_empty());
        assert_eq!(deferred_manifest, deferred.into_iter().collect());
        for table in archive_tables.keys() {
            assert!(
                preserved.contains(table.as_str())
                    || deferred_manifest.contains(table.as_str())
                    || [
                        "exterior_spatial_node",
                        "exterior_spatial_parent",
                        "exterior_spatial_rowid"
                    ]
                    .contains(&table.as_str())
                    || [
                        "conversion_cache",
                        "lod",
                        "lod_chunks",
                        "lod_build",
                        "lod_chunks_spatial",
                        "lod_chunks_spatial_node",
                        "lod_chunks_spatial_parent",
                        "lod_chunks_spatial_rowid"
                    ]
                    .contains(&table.as_str()),
                "unclassified SQL table requires explicit scope review: {table}"
            );
        }
        assert_eq!(
            scope["unchanged_auxiliary_tables"],
            serde_json::json!([
                "conversion_cache",
                "lod",
                "lod_chunks",
                "lod_build",
                "lod_chunks_spatial",
                "lod_chunks_spatial_node",
                "lod_chunks_spatial_parent",
                "lod_chunks_spatial_rowid"
            ])
        );
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("record-reader.json")).unwrap()).unwrap();
        assert_eq!(identity["bundle_mode"], "record_archive_only");
        assert_eq!(identity["runtime_ready"], false);
    }
    assert!(
        !full.join("record-bundle-scope.json").exists(),
        "default API output contract changed"
    );
    for name in ["skyrim_world.db", "inhouse-reader-diagnostics.json"] {
        let working = fs::read(work.join(name)).unwrap();
        let published = fs::read(copied.join(name)).unwrap();
        assert_eq!(
            Sha256::digest(&working),
            Sha256::digest(&published),
            "complete work-copy pin {name}"
        );
        assert_eq!(working, published, "complete work-copy bytes {name}");
    }
    for (path, bytes) in originals {
        assert_eq!(fs::read(path).unwrap(), bytes, "original input changed");
    }
}

/// Selection skips an inverted dependency before publishing the surviving archive.
#[test]
fn inverted_dependency_archive_publishes_only_surviving_plugins() {
    use converter::esm::inhouse::RecordBundleMode;
    let temp = tempfile::tempdir().unwrap();
    let retained = temp.path();
    let data = retained.join("original-data");
    fs::create_dir(&data).unwrap();
    let generated = world();
    // Both plugins exist, but Dependent.esp names a master that loads after it.
    // Selection excludes that dependent while preserving the usable owner.
    let owner = native::plugin(&generated, &[], 0, &[]);
    let dependent = native::plugin(&generated, &["Owner.esp"], 0, &[]);
    fs::write(data.join("Owner.esp"), &owner).unwrap();
    fs::write(data.join("Dependent.esp"), &dependent).unwrap();
    let order = retained.join("original-order.txt");
    let order_text = "*Dependent.esp\n*Owner.esp\n";
    fs::write(&order, order_text).unwrap();
    let output = retained.join("failed-archive-output");
    let result = inhouse::export_record_bundle_typed_with_options(
        &data,
        &order,
        &output,
        &[*b"ACTI"],
        None,
        RecordBundleMode::RecordArchiveOnly,
    );
    assert_eq!(result.unwrap(), 0);
    let scope: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("record-bundle-scope.json")).unwrap())
            .unwrap();
    assert_eq!(
        scope["status"],
        "COMPLETE_DIAGNOSTIC_ARCHIVE_RUNTIME_VIEWS_DEFERRED"
    );
    assert_eq!(scope["runtime_ready"], false);
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("record-reader.json")).unwrap()).unwrap();
    assert_eq!(identity["bundle_mode"], "record_archive_only");
    assert_eq!(identity["runtime_ready"], false);
    let database = Connection::open(output.join("skyrim_world.db")).unwrap();
    let plugins: Vec<String> = database
        .prepare("SELECT name FROM plugins ORDER BY priority")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(plugins, ["Owner.esp"]);
    assert_eq!(fs::read(data.join("Owner.esp")).unwrap(), owner);
    assert_eq!(fs::read(data.join("Dependent.esp")).unwrap(), dependent);
    assert_eq!(fs::read_to_string(order).unwrap(), order_text);
}

/// Missing-master selection fails before the archive output or identity is created.
#[test]
fn missing_master_archive_preflight_creates_no_output() {
    use converter::esm::inhouse::RecordBundleMode;
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("original-data");
    fs::create_dir(&data).unwrap();
    let generated = world();
    let patch = native::plugin(&generated, &["AbsentMaster.esm"], 0, &[]);
    fs::write(data.join("MissingDependency.esp"), &patch).unwrap();
    let order = temp.path().join("original-order.txt");
    let order_text = "*MissingDependency.esp\n";
    fs::write(&order, order_text).unwrap();
    let output = temp.path().join("failed-archive-output");
    let result = inhouse::export_record_bundle_typed_with_options(
        &data,
        &order,
        &output,
        &[*b"ACTI"],
        None,
        RecordBundleMode::RecordArchiveOnly,
    );
    assert!(
        result.is_err(),
        "missing master must fail the public export"
    );
    assert!(!output.exists());
    assert!(!output.join("record-reader.json").exists());
    assert_eq!(fs::read(data.join("MissingDependency.esp")).unwrap(), patch);
    assert_eq!(fs::read_to_string(order).unwrap(), order_text);
}
