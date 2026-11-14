use serde::Serialize;
use std::process::{Command, ExitCode};

/// Version of this JSON document's own shape; bump when keys change meaning.
const METADATA_SCHEMA: u32 = 1;

#[derive(Serialize)]
struct ReleaseMetadata {
    metadata_schema: u32,
    version: String,
    tag: String,
    git_commit: String,
    converter_schema: u32,
    min_runtime_converter_schema: u32,
    world_database_schema: u32,
    min_runtime_world_database_schema: u32,
    cell_cache_version: u32,
    streaming_cost_schema: u32,
}

/// Accept a full-length Git object id: 40 hex digits (SHA-1) or 64 (SHA-256).
fn validate_commit(commit: &str) -> Result<(), String> {
    let hex = commit.chars().all(|c| c.is_ascii_hexdigit());
    if hex && matches!(commit.len(), 40 | 64) {
        Ok(())
    } else {
        Err(format!(
            "invalid git commit {commit:?}: expected 40 or 64 hex digits"
        ))
    }
}

/// Build the release metadata JSON. `tag` falls back to `v{version}`.
fn render_metadata(version: &str, tag: Option<&str>, commit: &str) -> Result<String, String> {
    validate_commit(commit)?;
    let meta = ReleaseMetadata {
        metadata_schema: METADATA_SCHEMA,
        version: version.to_string(),
        tag: tag.map_or_else(|| format!("v{version}"), str::to_string),
        git_commit: commit.to_string(),
        converter_schema: shared::LOD_CONVERTER_SCHEMA_VERSION,
        min_runtime_converter_schema: shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION,
        world_database_schema: shared::WORLD_DATABASE_SCHEMA_VERSION,
        min_runtime_world_database_schema: shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION,
        cell_cache_version: shared::CELL_CACHE_VERSION,
        streaming_cost_schema: shared::streaming_costs::STREAMING_COST_SCHEMA_VERSION,
    };
    serde_json::to_string_pretty(&meta).map_err(|e| format!("failed to serialize metadata: {e}"))
}

fn git_head() -> Result<String, String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("failed to run `git rev-parse HEAD`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git rev-parse HEAD` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run() -> Result<String, String> {
    let commit = match std::env::var("RELEASE_COMMIT") {
        Ok(commit) => commit,
        Err(_) => git_head()?,
    };
    let tag = std::env::var("RELEASE_TAG").ok();
    render_metadata(env!("CARGO_PKG_VERSION"), tag.as_deref(), &commit)
}

/// Print release identifiers and schema versions as pretty-printed JSON to stdout.
///
/// Use `RELEASE_COMMIT` and `RELEASE_TAG` when set; otherwise resolve the commit
/// with `git rev-parse HEAD` and derive the tag from the Cargo package version.
/// Exits non-zero with a message on stderr if the commit cannot be resolved,
/// is not a 40 or 64 digit hex id, or the JSON cannot be serialized.
fn main() -> ExitCode {
    match run() {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("release_metadata: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn json_has_all_keys() {
        let json = render_metadata("1.2.3", Some("v1.2.3"), COMMIT).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in [
            "metadata_schema",
            "version",
            "tag",
            "git_commit",
            "converter_schema",
            "min_runtime_converter_schema",
            "world_database_schema",
            "min_runtime_world_database_schema",
            "cell_cache_version",
            "streaming_cost_schema",
        ] {
            assert!(keys.contains(&key), "missing key {key}");
        }
        assert_eq!(value["metadata_schema"], METADATA_SCHEMA);
        assert_eq!(value["git_commit"], COMMIT);
        assert!(json.find("metadata_schema").unwrap() < json.find("version").unwrap());
    }

    #[test]
    fn commit_validation() {
        assert!(validate_commit(COMMIT).is_ok());
        assert!(validate_commit(&"A".repeat(64)).is_ok());
        assert!(validate_commit("").is_err());
        assert!(validate_commit("nothex").is_err());
        assert!(validate_commit(&"g".repeat(40)).is_err());
        assert!(validate_commit("abc123").is_err());
        assert!(render_metadata("1.2.3", None, "").is_err());
    }

    #[test]
    fn tag_falls_back_to_version() {
        let json = render_metadata("1.2.3", None, COMMIT).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["tag"], "v1.2.3");
    }
}
