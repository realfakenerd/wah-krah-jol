//! Versioned payload declarations and deterministic content hashes.
//!
//! Optional derived payloads own whole tables, indexes, files or glTF extension names without
//! changing the upstream core contract. [`ExtensionEntry`] and [`ExtensionOwns`] are independent
//! of their enclosing manifest: both base manifests and overlay documents may embed them.
//! [`DataExtensions::from_manifest_json`] reads an `extensions` array with an explicit current
//! core identity supplied by the reader.
//!
//! A reader asks for its own ID and supported versions. Missing, stale or unsupported payloads
//! remain distinguishable, while [`ExtensionHasher`] implements the canonical content digest.
//! Reference: `crates/converter/tests/support/extension_hash_reference.py`.
//! Contract: `docs/specs/converters/extensions.md`.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;

use color_eyre::eyre::{Context, Result, bail, ensure};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Name of the content hash algorithm, the first line of every hashed stream.
pub const HASH_ALGORITHM: &str = "mudcrab-ext-hash-v1";

/// Load-door teleport links (`XTEL`).
pub const DOOR_LINKS: &str = "mudcrab.door_links";
/// What one extension wrote. Every list is sorted; a `files` entry ending in `/` covers every file
/// below that directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionOwns {
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub indexes: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub gltf_extensions: Vec<String>,
}

/// One entry of the manifest's `extensions` list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionEntry {
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub owns: ExtensionOwns,
    pub sha256: String,
    /// The core identity the extension was derived from (`lod_build.build_identity`).
    pub built_from: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stats: BTreeMap<String, u64>,
}

impl ExtensionEntry {
    /// Reject names that would inject control characters into the line-framed hash stream, and
    /// owned file paths that are not canonical dataset-relative paths. Readers and publishers
    /// share this one check, so a declaration a publisher refuses to hash is never reported as
    /// present by a reader.
    pub fn validate_names(&self) -> Result<()> {
        for name in std::iter::once(&self.id)
            .chain(&self.owns.tables)
            .chain(&self.owns.indexes)
            .chain(&self.owns.files)
            .chain(&self.owns.gltf_extensions)
        {
            ensure!(
                !name.is_empty() && !name.chars().any(char::is_control),
                "extension declaration has an empty name or control characters: {name:?}"
            );
        }
        for file in &self.owns.files {
            validate_owned_file_path(file)?;
        }
        Ok(())
    }
}

/// Checks that an owned `files` entry is a canonical dataset-relative path: `/` separators only,
/// no drive or stream colon, no empty, `.` or `..` components, no component ending in a dot or
/// space (Windows aliases), and an optional trailing `/` marking a whole directory.
pub fn validate_owned_file_path(entry: &str) -> Result<()> {
    let relative = entry.strip_suffix('/').unwrap_or(entry);
    ensure!(
        !relative.is_empty()
            && !entry.contains('\\')
            && !entry.contains(':')
            && !entry.chars().any(char::is_control)
            && relative.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.ends_with('.')
                    && !part.ends_with(' ')
            }),
        "owned file path {entry:?} must be a canonical dataset-relative path"
    );
    Ok(())
}

/// A rejected optional declaration. An identifiable rejected entry still disables its reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionIssue {
    pub id: Option<String>,
    pub reason: String,
}

#[derive(Default, Deserialize)]
struct ManifestExtensions {
    #[serde(default)]
    extensions: Option<serde_json::Value>,
}

/// Decode optional declarations independently. A bad entry never poisons its valid neighbors.
/// Duplicate IDs are all rejected rather than choosing one declaration arbitrarily.
pub fn parse_entries(value: &serde_json::Value) -> (Vec<ExtensionEntry>, Vec<ExtensionIssue>) {
    let Some(values) = value.as_array() else {
        return (
            Vec::new(),
            vec![ExtensionIssue {
                id: None,
                reason: "extensions is not an array; ignored optional declarations".into(),
            }],
        );
    };
    let mut entries = Vec::new();
    let mut issues = Vec::new();
    let mut ids = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for value in values {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(id) = &id
            && !ids.insert(id.clone())
        {
            duplicates.insert(id.clone());
            issues.push(ExtensionIssue {
                id: Some(id.clone()),
                reason: format!("extension {id} is listed twice"),
            });
        }
        let decoded = serde_json::from_value::<ExtensionEntry>(value.clone())
            .map_err(|error| error.to_string())
            .and_then(|entry| {
                entry
                    .validate_names()
                    .map(|()| entry)
                    .map_err(|error| error.to_string())
            });
        match decoded {
            Ok(entry) => entries.push(entry),
            Err(reason) => issues.push(ExtensionIssue { id, reason }),
        }
    }
    entries.retain(|entry| !duplicates.contains(&entry.id));
    (entries, issues)
}

/// Whether a data set carries an extension a feature needs, at a version it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionStatus {
    Present {
        version: u32,
    },
    Missing,
    /// Derived from another core build than the data set's current one.
    Stale {
        built_from: String,
    },
    /// A format this runtime does not read.
    UnsupportedVersion {
        version: u32,
        supported: RangeInclusive<u32>,
    },
}

impl ExtensionStatus {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }
}

impl fmt::Display for ExtensionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Present { version } => write!(f, "present (version {version})"),
            Self::Missing => write!(
                f,
                "not in this converted data set; convert it again with this converter"
            ),
            Self::Stale { built_from } => write!(
                f,
                "built from another conversion ({built_from}); convert the data set again"
            ),
            Self::UnsupportedVersion { version, supported } => write!(
                f,
                "version {version}, but this build reads versions {} to {}",
                supported.start(),
                supported.end()
            ),
        }
    }
}

/// The extensions of one loaded data set.
#[derive(Clone, Debug, Default)]
pub struct DataExtensions {
    entries: BTreeMap<String, ExtensionEntry>,
    core_identity: Option<String>,
    issues: Vec<ExtensionIssue>,
}

impl DataExtensions {
    /// Reads the `extensions` list from a conversion manifest's text. `core_identity` is the data
    /// set's current core build identity, when it has one; without it no extension counts as stale.
    pub fn from_manifest_json(text: &str, core_identity: Option<String>) -> Result<Self> {
        ensure!(
            text.trim_start().starts_with('{'),
            "the manifest must be a JSON object"
        );
        let manifest: ManifestExtensions =
            serde_json::from_str(text).context("reading the manifest's extensions list")?;
        let (entries, issues) = manifest
            .extensions
            .as_ref()
            .map(parse_entries)
            .unwrap_or_default();
        Ok(Self {
            entries: entries
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect(),
            core_identity,
            issues,
        })
    }

    /// Base-layout convenience loader for readers that need all declared freshness anchors.
    /// Empty, rejected-only and unanchored registries do not consult the database.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("conversion-manifest.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut data = Self::from_manifest_json(&text, None)?;
        let database = root.join("skyrim_world.db");
        if data
            .entries
            .values()
            .any(|entry| !entry.built_from.is_empty())
            && database.is_file()
        {
            data.core_identity = core_build_identity(&Connection::open_with_flags(
                &database,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?)?;
        }
        Ok(data)
    }

    pub fn issues(&self) -> impl Iterator<Item = &ExtensionIssue> {
        self.issues.iter()
    }

    /// Includes rejected declarations so a malformed entry cannot bypass a reader's gate.
    pub fn is_declared(&self, id: &str) -> bool {
        self.entries.contains_key(id)
            || self
                .issues
                .iter()
                .any(|issue| issue.id.as_deref() == Some(id))
    }

    pub fn entries(&self) -> impl Iterator<Item = &ExtensionEntry> {
        self.entries.values()
    }

    pub fn get(&self, id: &str) -> Option<&ExtensionEntry> {
        self.entries.get(id)
    }

    /// The status of extension `id` for a reader of `supported` versions.
    pub fn status(&self, id: &str, supported: RangeInclusive<u32>) -> ExtensionStatus {
        let Some(entry) = self.entries.get(id) else {
            return ExtensionStatus::Missing;
        };
        if !supported.contains(&entry.version) {
            return ExtensionStatus::UnsupportedVersion {
                version: entry.version,
                supported,
            };
        }
        if let Some(core) = &self.core_identity
            && *core != entry.built_from
        {
            return ExtensionStatus::Stale {
                built_from: entry.built_from.clone(),
            };
        }
        ExtensionStatus::Present {
            version: entry.version,
        }
    }
}

/// The schemas of `connection` that can hold dataset objects: `main` and every attached database,
/// never `temp`. Callers may attach a core database to a derived-payload database, or vice versa.
fn dataset_schemas(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA database_list")?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.into_iter().filter(|name| name != "temp").collect())
}

/// The one schema holding the table or index `name` (`kind` is `table` or `index`, names compare
/// ASCII case-insensitively like SQLite's own). `None` when no schema has it.
///
/// Two schemas holding the same object, or a `temp` object shadowing it, is an error: an unqualified
/// query would silently pick one of them, and a certified overlay table must never be answered by a
/// base table (or the other way round). For a table, a *view* of that name in any schema (including
/// `temp`) is refused the same way: a view would answer unqualified queries while the extension
/// hasher reads the real table.
pub fn locate_object(connection: &Connection, kind: &str, name: &str) -> Result<Option<String>> {
    let kinds: &[&str] = if kind == "table" {
        &["table", "view"]
    } else {
        &[kind]
    };
    let mut found: Option<String> = None;
    for schema in dataset_schemas(connection)? {
        let mut statement = connection.prepare(&format!(
            "SELECT type FROM {}.sqlite_master WHERE name = ?1 COLLATE NOCASE",
            quote_identifier(&schema)
        ))?;
        let types = statement
            .query_map([name], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut present = false;
        for found_type in types {
            if !kinds.contains(&found_type.as_str()) {
                continue;
            }
            ensure!(
                found_type == kind,
                "{name} is a {found_type} in the {schema} database; it would shadow the {kind}"
            );
            present = true;
        }
        if present {
            if let Some(first) = &found {
                bail!("{kind} {name} exists in both the {first} and {schema} databases");
            }
            found = Some(schema);
        }
    }
    let mut statement =
        connection.prepare("SELECT type FROM sqlite_temp_master WHERE name = ?1 COLLATE NOCASE")?;
    let temp = statement
        .query_map([name], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        !temp
            .iter()
            .any(|found_type| kinds.contains(&found_type.as_str())),
        "{kind} {name} is shadowed by a temporary object"
    );
    Ok(found)
}

/// The core build identity a data set's extensions are derived from, if the database has one. The
/// `lod_build` table is found in whichever one schema of `connection` has it.
/// An absent table or no row means no identity; schema, value-type and read errors are propagated.
pub fn core_build_identity(connection: &Connection) -> Result<Option<String>> {
    let Some(schema) = locate_object(connection, "table", "lod_build")? else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT build_identity FROM {}.lod_build LIMIT 2",
            quote_identifier(&schema)
        ))
        .with_context(|| format!("reading core build identity from {schema}.lod_build"))?;
    let identities = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("reading core build identity from {schema}.lod_build"))?;
    ensure!(
        identities.len() <= 1,
        "{schema}.lod_build has multiple core build identities"
    );
    Ok(identities.into_iter().next())
}

/// Problems with ownership across a list: two extensions owning the same item, or an id twice.
/// SQL names and file ownership compare with ASCII case equivalence on every platform.
/// Directory prefixes conflict too. glTF names remain case-sensitive. Declared spelling remains
/// part of the hash stream.
pub fn ownership_problems<'a>(
    entries: impl IntoIterator<Item = &'a ExtensionEntry>,
) -> Vec<String> {
    let mut seen: BTreeMap<(&'static str, Cow<'a, str>), &'a str> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut files: Vec<(&str, String)> = Vec::new();
    let mut problems = Vec::new();
    for entry in entries {
        if !ids.insert(entry.id.as_str()) {
            problems.push(format!("extension {} is listed twice", entry.id));
        }
        let kinds: [(&'static str, &'a Vec<String>); 4] = [
            ("table", &entry.owns.tables),
            ("index", &entry.owns.indexes),
            ("file", &entry.owns.files),
            ("gltf_extension", &entry.owns.gltf_extensions),
        ];
        for (kind, names) in kinds {
            let mut local = BTreeSet::new();
            for name in names {
                let key: Cow<'a, str> = if kind == "gltf_extension" {
                    Cow::Borrowed(name)
                } else {
                    Cow::Owned(name.to_ascii_lowercase())
                };
                if !local.insert(key.clone()) {
                    continue;
                }
                if kind == "file" {
                    for (other_id, other) in &files {
                        if *other_id != entry.id
                            && other != key.as_ref()
                            && ((other.ends_with('/') && key.starts_with(other.as_str()))
                                || (key.ends_with('/') && other.starts_with(key.as_ref())))
                        {
                            problems.push(format!(
                                "file {name} of {} overlaps file {other} of {other_id}",
                                entry.id
                            ));
                        }
                    }
                    files.push((&entry.id, key.to_string()));
                }
                let owner = *seen.entry((kind, key)).or_insert(entry.id.as_str());
                if owner != entry.id {
                    problems.push(format!(
                        "{kind} {name} is owned by {owner} and {}",
                        entry.id
                    ));
                }
            }
        }
    }
    problems
}

/// Builds an extension's `mudcrab-ext-hash-v1` content hash. Parts may be added in any order; the
/// stream is put in the defined order by [`ExtensionHasher::finish`].
#[derive(Debug)]
pub struct ExtensionHasher {
    id: String,
    version: u32,
    tables: BTreeMap<String, TableSection>,
    indexes: BTreeMap<String, String>,
    files: BTreeMap<String, String>,
    gltf: BTreeMap<String, BTreeMap<String, String>>,
}

/// Rows share one allocation instead of repeatedly growing a string for every SQL row.
/// Ranges retain canonical bytewise ordering without moving the encoded bytes themselves.
#[derive(Debug)]
struct TableSection {
    header: String,
    rows: String,
    order: Vec<std::ops::Range<usize>>,
    columns: String,
}

impl ExtensionHasher {
    pub fn new(id: &str, version: u32) -> Self {
        Self {
            id: id.to_owned(),
            version,
            tables: BTreeMap::new(),
            indexes: BTreeMap::new(),
            files: BTreeMap::new(),
            gltf: BTreeMap::new(),
        }
    }

    /// Adds every row and the column declarations of `table`.
    /// Canonical sorting retains encoded rows in memory until `finish`; text/blob bytes use
    /// two hex characters per byte, plus one range per row. This is not a streaming API.
    pub fn add_table(&mut self, connection: &Connection, table: &str) -> Result<()> {
        ensure!(
            !table.is_empty() && !table.chars().any(char::is_control),
            "invalid table name {table:?}"
        );
        let schema = locate_object(connection, "table", table)?
            .ok_or_else(|| color_eyre::eyre::eyre!("table {table} is missing"))?;
        let quoted = format!("{}.{}", quote_identifier(&schema), quote_identifier(table));
        let mut columns = Vec::new();
        let mut info = connection.prepare(&format!(
            "PRAGMA {}.table_info({})",
            quote_identifier(&schema),
            quote_identifier(table)
        ))?;
        let mut rows = info.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let declared: String = row.get(2)?;
            columns.push(format!("{name}:{declared}"));
        }
        ensure!(!columns.is_empty(), "table {table} is missing");
        let mut encoded = String::new();
        let mut order = Vec::new();
        let mut select = connection.prepare(&format!("SELECT * FROM {quoted}"))?;
        let count = select.column_count();
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            let start = encoded.len();
            for index in 0..count {
                if index > 0 {
                    encoded.push(',');
                }
                encode_sql(row.get_ref(index)?, &mut encoded);
            }
            order.push(start..encoded.len());
        }
        order.sort_unstable_by(|left, right| encoded[left.clone()].cmp(&encoded[right.clone()]));
        self.tables.insert(
            table.to_owned(),
            TableSection {
                header: format!("table {table}"),
                rows: encoded,
                order,
                columns: format!("columns {}", columns.join(",")),
            },
        );
        Ok(())
    }

    /// Adds an index's stored `CREATE INDEX` statement.
    pub fn add_index(&mut self, connection: &Connection, index: &str) -> Result<()> {
        ensure!(
            !index.is_empty() && !index.chars().any(char::is_control),
            "invalid index name {index:?}"
        );
        let Some(schema) = locate_object(connection, "index", index)? else {
            bail!("index {index} is missing");
        };
        let sql: Option<String> = connection
            .query_row(
                &format!(
                    "SELECT sql FROM {}.sqlite_master WHERE type = 'index' AND name = ?1 COLLATE NOCASE",
                    quote_identifier(&schema)
                ),
                [index],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .with_context(|| format!("reading index {index}"))?
            .flatten();
        let Some(sql) = sql else {
            bail!("index {index} is missing");
        };
        self.indexes
            .insert(index.to_owned(), format!("index {index} {sql}"));
        Ok(())
    }

    /// Adds one owned file by its path relative to the data set root (`/` separators).
    pub fn add_file(&mut self, relative_path: &str, bytes: &[u8]) {
        self.add_file_digest(relative_path, &hex(&Sha256::digest(bytes)));
    }

    /// Adds one owned file whose SHA-256 (64 lowercase hex digits) the caller already computed.
    pub fn add_file_digest(&mut self, relative_path: &str, sha256_hex: &str) {
        self.files.insert(
            relative_path.to_owned(),
            format!("file {relative_path} {sha256_hex}"),
        );
    }

    /// Adds the values of glTF extension `name` in one mesh document, if the mesh uses it.
    /// Returns whether any owned payload was hashed; unrepresentable float values are errors.
    pub fn add_gltf(
        &mut self,
        name: &str,
        relative_path: &str,
        document: &serde_json::Value,
    ) -> Result<bool> {
        let used = document["extensionsUsed"]
            .as_array()
            .is_some_and(|used| used.iter().any(|value| value.as_str() == Some(name)));
        if !used {
            return Ok(false);
        }
        let mut parts = Vec::new();
        collect_gltf_extension(document, name, "", &mut parts)?;
        if parts.is_empty() {
            return Ok(false);
        }
        let digest = hex(&Sha256::digest(parts.join(";").as_bytes()));
        self.gltf.entry(name.to_owned()).or_default().insert(
            relative_path.to_owned(),
            format!("gltf {name} {relative_path} {digest}"),
        );
        Ok(true)
    }

    /// The lowercase hex SHA-256 of the stream.
    pub fn finish(self) -> String {
        let mut digest = Sha256::new();
        let mut line = |text: &str| {
            digest.update(text.as_bytes());
            digest.update(b"\n");
        };
        line(HASH_ALGORITHM);
        line(&self.id);
        line(&self.version.to_string());
        for section in self.tables.values() {
            line(&section.header);
            for range in &section.order {
                line(&section.rows[range.clone()]);
            }
            line(&section.columns);
        }
        for text in self.indexes.values() {
            line(text);
        }
        for text in self.files.values() {
            line(text);
        }
        for meshes in self.gltf.values() {
            for text in meshes.values() {
                line(text);
            }
        }
        hex(&digest.finalize())
    }
}

/// Collect every owned occurrence in deterministic object-key/array order. JSON pointers
/// distinguish identical values at different locations; root/material streams retain their order.
fn collect_gltf_extension(
    value: &serde_json::Value,
    name: &str,
    pointer: &str,
    parts: &mut Vec<String>,
) -> Result<()> {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(extension) = object
                .get("extensions")
                .and_then(|extensions| extensions.get(name))
            {
                parts.push(format!(
                    "{pointer}/extensions/{}={}",
                    escape_pointer(name),
                    encode_json(extension)?
                ));
            }
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            for key in keys {
                collect_gltf_extension(
                    &object[key],
                    name,
                    &format!("{pointer}/{}", escape_pointer(key)),
                    parts,
                )?;
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_gltf_extension(item, name, &format!("{pointer}/{index}"), parts)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// Hashes the database parts of an entry (tables and indexes) without filesystem content.
#[cfg(test)]
fn hash_database_extension(
    connection: &Connection,
    id: &str,
    version: u32,
    owns: &ExtensionOwns,
) -> Result<String> {
    ensure!(
        owns.files.is_empty() && owns.gltf_extensions.is_empty(),
        "extension {id} owns files or glTF extensions; hash them with ExtensionHasher"
    );
    let mut hasher = ExtensionHasher::new(id, version);
    for table in &owns.tables {
        hasher.add_table(connection, table)?;
    }
    for index in &owns.indexes {
        hasher.add_index(connection, index)?;
    }
    Ok(hasher.finish())
}

/// `name` as a quoted SQLite identifier.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    push_hex(bytes, &mut text);
    text
}

/// Append the exact ext-hash-v1 lowercase byte encoding without formatting or
/// allocating a temporary string for every SQL text/blob value.
fn push_hex(bytes: &[u8], text: &mut String) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    text.reserve(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
}

fn push_real(value: f64, out: &mut String) {
    use fmt::Write;
    let _ = write!(out, "f{:016x}", value.to_bits());
}

fn encode_sql(value: ValueRef<'_>, out: &mut String) {
    use fmt::Write;
    match value {
        ValueRef::Null => out.push('n'),
        ValueRef::Integer(value) => {
            let _ = write!(out, "i{value}");
        }
        ValueRef::Real(value) => push_real(value, out),
        ValueRef::Text(bytes) => {
            out.push('t');
            push_hex(bytes, out);
        }
        ValueRef::Blob(bytes) => {
            out.push('b');
            push_hex(bytes, out);
        }
    }
}

/// Canonical JSON of parsed values: sorted keys, UTF-8 hex strings, exact native integers and
/// binary64 float bits. Integer `1` and floating-point `1.0` have different encodings.
pub fn encode_json(value: &serde_json::Value) -> Result<String> {
    let mut out = String::new();
    encode_json_into(value, &mut out)?;
    Ok(out)
}

fn encode_json_into(value: &serde_json::Value, out: &mut String) -> Result<()> {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(integer) = number.as_u64() {
                out.push('i');
                out.push_str(&integer.to_string());
            } else if let Some(integer) = number.as_i64() {
                out.push('i');
                out.push_str(&integer.to_string());
            } else {
                let real = number
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "glTF JSON number {number} is outside the finite binary64 range"
                        )
                    })?;
                push_real(real, out);
            }
        }
        Value::String(text) => {
            out.push('t');
            out.push_str(&hex(text.as_bytes()));
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                encode_json_into(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('t');
                out.push_str(&hex(key.as_bytes()));
                out.push(':');
                encode_json_into(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const OTHER_ID: &str = "example.other";
    const STALE_ID: &str = "example.stale";
    const MISSING_ID: &str = "example.missing";

    #[test]
    fn fast_sql_hex_matches_the_original_byte_encoding() {
        let bytes: Vec<u8> = (0..=255).cycle().take(65537).collect();
        for bytes in [&[][..], &bytes[..1], &bytes[..256], bytes.as_slice()] {
            let original: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(hex(bytes), original);
            for (value, prefix) in [(ValueRef::Text(bytes), "t"), (ValueRef::Blob(bytes), "b")] {
                let mut encoded = String::from("existing,");
                encode_sql(value, &mut encoded);
                assert_eq!(encoded, format!("existing,{prefix}{original}"));
            }
        }
    }

    #[test]
    fn table_arena_matches_the_original_rows_for_all_sql_types_and_duplicates() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE values_to_hash (n, i, f, t, b);
             INSERT INTO values_to_hash VALUES (NULL, -9223372036854775808, -0.125, '✓', X'00FF');
             INSERT INTO values_to_hash VALUES (NULL, 42, 1e100, '', X'');
             INSERT INTO values_to_hash SELECT * FROM values_to_hash;",
            )
            .unwrap();
        let mut hasher = ExtensionHasher::new("fixture", 1);
        hasher.add_table(&connection, "values_to_hash").unwrap();
        let section = &hasher.tables["values_to_hash"];
        let mut statement = connection.prepare("SELECT * FROM values_to_hash").unwrap();
        let mut rows = statement.query([]).unwrap();
        let mut original = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            let mut line = String::new();
            for index in 0..5 {
                if index != 0 {
                    line.push(',');
                }
                encode_sql(row.get_ref(index).unwrap(), &mut line);
            }
            original.push(line);
        }
        original.sort_unstable();
        let arena: Vec<_> = section
            .order
            .iter()
            .map(|range| &section.rows[range.clone()])
            .collect();
        assert_eq!(arena, original);
        let mut expected = Sha256::new();
        for line in [HASH_ALGORITHM, "fixture", "1", &section.header]
            .into_iter()
            .chain(original.iter().map(String::as_str))
            .chain([section.columns.as_str()])
        {
            expected.update(line.as_bytes());
            expected.update(b"\n");
        }
        assert_eq!(hasher.finish(), hex(&expected.finalize()));
    }

    // These values also pin the neutral Python oracle in the converter integration tests.
    const PINNED_DOOR_LINKS: &str =
        "5c9f99438d7658c66aafebaa45045f6d553f0d52521309be28b1ef108db20e87";

    const PINNED_MATERIAL: &str =
        "a4917e5beaf2c06dcdeea549a57f76552bc8ef780476b01bafd338ffd1996b41";

    fn sample_database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
        CREATE TABLE statics (id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT);
        INSERT INTO statics VALUES (16, 'RockA', 'meshes/rock.glb');
        CREATE TABLE door_links (ref_id INTEGER PRIMARY KEY, destination_ref_id INTEGER,
                                 pos_x REAL, pos_y REAL, pos_z REAL, note TEXT, data BLOB);
        INSERT INTO door_links VALUES (2, 1, 0.5, -1.25, 1e-7, 'Whiterun ✓', x'00ff');
        INSERT INTO door_links VALUES (1, 2, 3.0, 4.0, NULL, NULL, NULL);
        CREATE INDEX idx_door_links_destination ON door_links(destination_ref_id);
        ",
            )
            .unwrap();
        connection
    }

    fn door_links_owns() -> ExtensionOwns {
        ExtensionOwns {
            tables: vec!["door_links".into()],
            indexes: vec!["idx_door_links_destination".into()],
            ..Default::default()
        }
    }

    #[test]
    fn database_hash_matches_the_python_reference() {
        let connection = sample_database();
        let hash = hash_database_extension(&connection, DOOR_LINKS, 1, &door_links_owns()).unwrap();
        assert_eq!(hash, PINNED_DOOR_LINKS);
    }

    #[test]
    fn file_digest_equals_file_bytes() {
        let bytes = b"payload bytes";
        let mut by_bytes = ExtensionHasher::new(DOOR_LINKS, 1);
        by_bytes.add_file("payload/a.bin", bytes);
        let mut by_digest = ExtensionHasher::new(DOOR_LINKS, 1);
        by_digest.add_file_digest("payload/a.bin", &hex(&Sha256::digest(bytes)));
        assert_eq!(by_bytes.finish(), by_digest.finish());
    }

    #[test]
    fn gltf_hash_matches_the_python_reference() {
        let rock = serde_json::json!({
            "asset": {"version": "2.0"},
            "extensionsUsed": ["MUDCRAB_material", "OPEN_SKYRIM_material"],
            "materials": [
                {"extensions": {"OPEN_SKYRIM_material": {"shaderFlags1": 1},
                                "MUDCRAB_material": {"uvScale": [1, 2.5], "blendSource": "SRC_ALPHA"}}},
                {"extensions": {"OPEN_SKYRIM_material": {"shaderFlags1": 2}}}
            ]
        });
        let plain = serde_json::json!({"asset": {"version": "2.0"}});
        let mut hasher = ExtensionHasher::new("mudcrab.material", 1);
        hasher
            .add_gltf("MUDCRAB_material", "meshes/rock.glb", &rock)
            .unwrap();
        hasher
            .add_gltf("MUDCRAB_material", "meshes/plain.glb", &plain)
            .unwrap();
        assert_eq!(hasher.finish(), PINNED_MATERIAL);
    }

    #[test]
    fn gltf_hash_covers_nodes_primitives_and_occurrence_paths() {
        let hash = |document: &serde_json::Value| {
            let mut hasher = ExtensionHasher::new("example.payload", 1);
            hasher
                .add_gltf("EXAMPLE_payload", "meshes/example.gltf", document)
                .unwrap();
            hasher.finish()
        };
        let document = serde_json::json!({
            "extensionsUsed": ["EXAMPLE_payload"],
            "nodes": [{"extensions": {"EXAMPLE_payload": {"value": 1}}}],
            "meshes": [{"primitives": [{"extensions": {"EXAMPLE_payload": {"value": 2}}}]}]
        });
        let original = hash(&document);
        for pointer in [
            "/nodes/0/extensions/EXAMPLE_payload/value",
            "/meshes/0/primitives/0/extensions/EXAMPLE_payload/value",
        ] {
            let mut changed = document.clone();
            *changed.pointer_mut(pointer).unwrap() = 3.into();
            assert_ne!(hash(&changed), original, "unhashed occurrence: {pointer}");
        }
        let mut moved = document.clone();
        moved["scenes"] = moved.as_object_mut().unwrap().remove("nodes").unwrap();
        assert_ne!(
            hash(&moved),
            original,
            "occurrence location is part of the hash"
        );
    }

    #[test]
    fn canonical_json_does_not_round_distinct_large_integers_together() {
        for (first, second) in [
            (
                serde_json::json!(9_007_199_254_740_992u64),
                serde_json::json!(9_007_199_254_740_993u64),
            ),
            (serde_json::json!(u64::MAX - 1), serde_json::json!(u64::MAX)),
            (serde_json::json!(i64::MIN), serde_json::json!(i64::MIN + 1)),
        ] {
            assert_ne!(encode_json(&first).unwrap(), encode_json(&second).unwrap());
        }
        assert_ne!(
            encode_json(&serde_json::json!(1)).unwrap(),
            encode_json(&serde_json::json!(1.0)).unwrap()
        );
        assert_eq!(
            encode_json(&serde_json::json!(u64::MAX)).unwrap(),
            "i18446744073709551615"
        );
        let first: serde_json::Value = serde_json::from_str("18446744073709551616").unwrap();
        let second: serde_json::Value = serde_json::from_str("18446744073709551617").unwrap();
        assert!(first.is_f64() && second.is_f64());
        assert_eq!(
            first, second,
            "the standard parser has already rounded these tokens"
        );
        assert_eq!(encode_json(&first).unwrap(), encode_json(&second).unwrap());
        assert_eq!(encode_json(&serde_json::json!(0)).unwrap(), "i0");
        assert_eq!(
            encode_json(&serde_json::from_str("-0").unwrap()).unwrap(),
            "f8000000000000000"
        );
        assert_eq!(
            encode_json(&serde_json::json!(0.0)).unwrap(),
            "f0000000000000000"
        );
        let integer = "9".repeat(400);
        assert!(serde_json::from_str::<serde_json::Value>(&integer).is_err());
        assert!(serde_json::from_str::<serde_json::Value>("1e999").is_err());
    }

    #[test]
    fn index_without_stored_sql_is_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE payload(a UNIQUE);")
            .unwrap();
        let mut hasher = ExtensionHasher::new("example.payload", 1);
        assert!(
            hasher
                .add_index(&connection, "sqlite_autoindex_payload_1")
                .is_err()
        );
    }

    #[test]
    fn a_row_change_changes_the_hash() {
        let connection = sample_database();
        connection
            .execute(
                "UPDATE door_links SET pos_x = 0.5000001 WHERE ref_id = 2",
                [],
            )
            .unwrap();
        let hash = hash_database_extension(&connection, DOOR_LINKS, 1, &door_links_owns()).unwrap();
        assert_ne!(hash, PINNED_DOOR_LINKS);
    }

    #[test]
    fn a_missing_table_is_an_error() {
        let connection = Connection::open_in_memory().unwrap();
        let error =
            hash_database_extension(&connection, DOOR_LINKS, 1, &door_links_owns()).unwrap_err();
        assert!(error.to_string().contains("door_links is missing"));
    }

    fn entry(id: &str, version: u32, built_from: &str) -> ExtensionEntry {
        ExtensionEntry {
            id: id.into(),
            version,
            owns: ExtensionOwns::default(),
            sha256: String::new(),
            built_from: built_from.into(),
            stats: BTreeMap::new(),
        }
    }

    #[test]
    fn status_reports_each_case() {
        let manifest = serde_json::json!({
            "schema_version": 26,
            "complete": true,
            "extensions": [entry(DOOR_LINKS, 1, "core"), entry(OTHER_ID, 3, "core"),
                           entry(STALE_ID, 1, "older")]
        });
        let data =
            DataExtensions::from_manifest_json(&manifest.to_string(), Some("core".into())).unwrap();
        assert_eq!(
            data.status(DOOR_LINKS, 1..=1),
            ExtensionStatus::Present { version: 1 }
        );
        assert_eq!(data.status(MISSING_ID, 1..=1), ExtensionStatus::Missing);
        assert_eq!(
            data.status(OTHER_ID, 1..=2),
            ExtensionStatus::UnsupportedVersion {
                version: 3,
                supported: 1..=2
            }
        );
        assert_eq!(
            data.status(STALE_ID, 1..=1),
            ExtensionStatus::Stale {
                built_from: "older".into()
            }
        );
    }

    #[test]
    fn overlay_declarations_use_the_same_generic_contract_and_explicit_base_identity() {
        let document = serde_json::json!({
            "format": "example.overlay", "base": {"build_identity": "core"},
            "extensions": [entry("example.overlay_payload", 1, "core")]
        });
        let data =
            DataExtensions::from_manifest_json(&document.to_string(), Some("core".into())).unwrap();
        assert_eq!(
            data.status("example.overlay_payload", 1..=1),
            ExtensionStatus::Present { version: 1 }
        );
        let stale =
            DataExtensions::from_manifest_json(&document.to_string(), Some("new-core".into()))
                .unwrap();
        assert!(matches!(
            stale.status("example.overlay_payload", 1..=1),
            ExtensionStatus::Stale { .. }
        ));
    }

    #[test]
    fn a_main_manifest_has_no_extensions() {
        let data =
            DataExtensions::from_manifest_json(r#"{"schema_version": 26, "complete": true}"#, None)
                .unwrap();
        assert_eq!(data.entries().count(), 0);
        assert_eq!(data.status(DOOR_LINKS, 1..=1), ExtensionStatus::Missing);
    }

    #[test]
    fn sql_ownership_uses_ascii_case_but_preserves_other_name_rules() {
        for (first, second, conflicts) in [
            ("extra_payload", "EXTRA_PAYLOAD", true),
            ("idx_lod_tree_block", "IDX_LOD_TREE_BLOCK", true),
            ("Mixed_Ä", "mIXED_Ä", true),
            ("Ä", "ä", false),
            ("Σ", "σ", false),
            ("ß", "ss", false),
        ] {
            let mut a = entry("example.a", 1, "core");
            let mut b = entry("example.b", 1, "core");
            a.owns.tables = vec![first.into()];
            b.owns.tables = vec![second.into()];
            assert_eq!(!ownership_problems([&a, &b]).is_empty(), conflicts);
            a.owns.tables.clear();
            b.owns.tables.clear();
            a.owns.indexes = vec![first.into()];
            b.owns.indexes = vec![second.into()];
            assert_eq!(!ownership_problems([&b, &a]).is_empty(), conflicts);
        }
        let mut a = entry("example.a", 1, "core");
        let mut b = entry("example.b", 1, "core");
        a.owns.files = vec!["meshes/Asset.glb".into()];
        b.owns.files = vec!["meshes/asset.glb".into()];
        assert_eq!(ownership_problems([&a, &b]).len(), 1);
        b.owns.files = a.owns.files.clone();
        assert_eq!(ownership_problems([&a, &b]).len(), 1);
    }

    #[test]
    fn core_identity_preserves_absent_and_empty_row_compatibility() {
        let connection = Connection::open_in_memory().unwrap();
        assert_eq!(core_build_identity(&connection).unwrap(), None);
        connection
            .execute_batch(
                "ATTACH ':memory:' AS base;
            CREATE TABLE base.lod_build(id INTEGER PRIMARY KEY, build_identity TEXT);",
            )
            .unwrap();
        assert_eq!(core_build_identity(&connection).unwrap(), None);
        connection
            .execute_batch("INSERT INTO base.lod_build VALUES(1, 'core');")
            .unwrap();
        assert_eq!(
            core_build_identity(&connection).unwrap(),
            Some("core".into())
        );
        connection
            .execute_batch("UPDATE base.lod_build SET build_identity = '';")
            .unwrap();
        assert_eq!(
            core_build_identity(&connection).unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn core_identity_rejects_malformed_columns_and_values() {
        for schema in ["main", "base"] {
            for sql in [
                "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, renamed_identity TEXT);",
                "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, build_identity);
                 INSERT INTO lod_build VALUES(1, 42);",
                "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, build_identity);
                 INSERT INTO lod_build VALUES(1, X'636f7265');",
                "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, build_identity);
                 INSERT INTO lod_build VALUES(1, NULL);",
            ] {
                let connection = Connection::open_in_memory().unwrap();
                if schema == "base" {
                    connection
                        .execute_batch("ATTACH ':memory:' AS base;")
                        .unwrap();
                }
                connection
                    .execute_batch(&sql.replace("lod_build", &format!("{schema}.lod_build")))
                    .unwrap();
                let error = core_build_identity(&connection).unwrap_err();
                let context = format!("{error:#}");
                assert!(context.contains("reading core build identity"), "{context}");
                assert!(context.contains(schema), "{context}");
                assert!(error.downcast_ref::<rusqlite::Error>().is_some());
            }
        }
    }

    #[test]
    fn core_identity_rejects_ambiguous_or_shadowed_objects() {
        for sql in [
            "CREATE TABLE lod_build(build_identity TEXT);
             ATTACH ':memory:' AS base; CREATE TABLE base.lod_build(build_identity TEXT);",
            "CREATE TABLE lod_build(build_identity TEXT);
             CREATE TEMP TABLE lod_build(build_identity TEXT);",
            "CREATE VIEW lod_build AS SELECT 'core' AS build_identity;",
        ] {
            let connection = Connection::open_in_memory().unwrap();
            connection.execute_batch(sql).unwrap();
            assert!(core_build_identity(&connection).is_err());
        }
    }

    #[test]
    fn duplicate_ids_and_shared_items_are_refused() {
        let manifest = serde_json::json!({"extensions": [entry(DOOR_LINKS, 1, "c"), entry(DOOR_LINKS, 1, "c")]});
        let data = DataExtensions::from_manifest_json(&manifest.to_string(), None).unwrap();
        assert_eq!(data.status(DOOR_LINKS, 1..=1), ExtensionStatus::Missing);
        assert!(data.is_declared(DOOR_LINKS));
        assert!(
            data.issues()
                .any(|issue| issue.reason.contains("listed twice"))
        );
        let duplicate = entry("example.a", 1, "c");
        assert_eq!(
            ownership_problems([&duplicate, &duplicate]),
            vec!["extension example.a is listed twice"]
        );
        let mut a = entry(DOOR_LINKS, 1, "c");
        a.owns.tables = vec!["door_links".into()];
        let mut b = entry(OTHER_ID, 1, "c");
        b.owns.tables = vec!["door_links".into()];
        assert_eq!(
            ownership_problems([&a, &b]),
            vec!["table door_links is owned by mudcrab.door_links and example.other".to_owned()]
        );
    }

    #[test]
    fn malformed_optional_entries_do_not_poison_valid_neighbors() {
        let good = entry("example.good", 1, "core");
        for malformed in [
            serde_json::json!({"id": DOOR_LINKS}),
            serde_json::json!({"id": DOOR_LINKS, "version": 1, "sha256": "", "built_from": "core", "stats": {"rows": 1.5}}),
            serde_json::json!({"id": DOOR_LINKS, "version": 2, "owns": {"future_payload": ["data"]}, "sha256": "", "built_from": "core"}),
            serde_json::json!(42),
        ] {
            let manifest = serde_json::json!({"extensions": [malformed, good]});
            let data =
                DataExtensions::from_manifest_json(&manifest.to_string(), Some("core".into()))
                    .unwrap();
            assert_eq!(data.entries().count(), 1);
            assert_eq!(
                data.status("example.good", 1..=1),
                ExtensionStatus::Present { version: 1 }
            );
            assert_eq!(data.status(DOOR_LINKS, 1..=1), ExtensionStatus::Missing);
            assert!(data.issues().next().is_some());
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!(false),
        ] {
            let data = DataExtensions::from_manifest_json(
                &serde_json::json!({"extensions": value}).to_string(),
                None,
            )
            .unwrap();
            assert_eq!(data.entries().count(), 0);
        }
    }

    #[test]
    fn file_ownership_refuses_case_aliases_and_nested_prefixes_in_either_order() {
        let mut a = entry("example.a", 1, "core");
        a.owns.files = vec!["Payload/".into()];
        for path in ["payload/a.bin", "PAYLOAD/nested/", "payload/"] {
            let mut b = entry("example.b", 1, "core");
            b.owns.files = vec![path.into()];
            assert!(!ownership_problems([&a, &b]).is_empty());
            assert!(!ownership_problems([&b, &a]).is_empty());
        }
        assert!(ownership_problems([&a]).is_empty());
    }

    #[test]
    fn multiple_core_identity_rows_are_refused() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE lod_build(build_identity TEXT); INSERT INTO lod_build VALUES('a'), ('b');").unwrap();
        assert!(
            core_build_identity(&connection)
                .unwrap_err()
                .to_string()
                .contains("multiple core build identities")
        );
    }

    #[test]
    fn declared_control_characters_are_rejected_without_rejecting_the_manifest() {
        for kind in ["tables", "indexes", "files"] {
            let mut value = serde_json::to_value(entry("example.bad", 1, "core")).unwrap();
            value["owns"][kind] = serde_json::json!(["bad\nname"]);
            let (entries, issues) = parse_entries(&serde_json::json!([value]));
            assert!(entries.is_empty());
            assert!(issues[0].reason.contains("control characters"));
        }
    }

    #[test]
    fn declared_file_paths_must_be_canonical_dataset_relative_paths() {
        for path in [
            "../outside.bin",
            "payload/./a.bin",
            "payload/../a.bin",
            "payload//a.bin",
            "payload\\a.bin",
            "/absolute.bin",
            "Q:/absolute.bin",
            "payload/a.bin.",
            "payload/a.bin ",
            "./",
            "/",
        ] {
            let mut value = serde_json::to_value(entry("example.bad", 1, "core")).unwrap();
            value["owns"]["files"] = serde_json::json!([path]);
            let (entries, issues) = parse_entries(&serde_json::json!([value]));
            assert!(entries.is_empty(), "accepted {path:?}");
            assert_eq!(issues.len(), 1, "{path:?}");
            assert_eq!(issues[0].id.as_deref(), Some("example.bad"));
            assert!(issues[0].reason.contains("canonical"), "{path:?}");
        }
        for path in ["payload/a.bin", "payload/", "a.bin"] {
            let mut value = serde_json::to_value(entry("example.ok", 1, "core")).unwrap();
            value["owns"]["files"] = serde_json::json!([path]);
            let (entries, issues) = parse_entries(&serde_json::json!([value]));
            assert_eq!(entries.len(), 1, "rejected {path:?}");
            assert!(issues.is_empty());
        }
    }
}
