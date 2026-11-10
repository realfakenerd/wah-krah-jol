"""Neutral test oracle for the mudcrab-ext-hash-v1 extension hash stream.

SQL and file ownership use ASCII case equivalence on every platform.
Directory ownership includes nested paths; index lookup accepts SQL case aliases.
Run with a dataset root to print problems and exit 1 when invalid.
"""


from __future__ import annotations

import argparse
import hashlib
import json
import math
import sqlite3
import struct
import sys
from pathlib import Path


ALGORITHM = "mudcrab-ext-hash-v1"
DATABASE = "skyrim_world.db"
MANIFEST = "conversion-manifest.json"
GLB_MAGIC = 0x46546C67
GLB_JSON_CHUNK = 0x4E4F534A
ASCII_CASE = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")


def _hex_text(text: str) -> str:
    return text.encode("utf-8").hex()


def _real(value: float) -> str:
    return "f" + struct.pack(">d", value).hex()


def encode_sql(value) -> str:
    """One SQLite value as `n`, `i<dec>`, `f<bits>`, `t<utf8 hex>` or `b<hex>`."""
    if value is None:
        return "n"
    if isinstance(value, bool):  # sqlite3 never returns bool, but keep ints honest
        return f"i{int(value)}"
    if isinstance(value, int):
        return f"i{value}"
    if isinstance(value, float):
        return _real(value)
    if isinstance(value, str):
        return "t" + _hex_text(value)
    if isinstance(value, (bytes, bytearray, memoryview)):
        return "b" + bytes(value).hex()
    raise TypeError(f"unsupported SQLite value {value!r}")


def validate_name(name: str) -> None:
    """Names must not inject control characters into the line-framed hash stream."""
    if not name or any(ord(char) < 32 or 127 <= ord(char) <= 159 for char in name):
        raise ValueError(f"name {name!r} is empty or contains a control character")


def _parsed_number(token: str) -> int | float:
    """Model serde_json 1.0.150's default number parsing, without float_roundtrip.

    json.loads validates the token grammar. Keep serde_json's u64 significand,
    discarded digits and separate binary64 scaling: Python's float(token) can
    round differently. This is a test oracle, not a production JSON parser.
    """
    negative = token.startswith("-")
    unsigned = token[1:] if negative else token
    mantissa, separator, explicit_exp = unsigned.lower().partition("e")
    integer, decimal, fraction = mantissa.partition(".")
    significand = 0
    exponent = 0
    long_integer = False
    for digit in integer:
        candidate = significand * 10 + int(digit)
        if long_integer or candidate > (1 << 64) - 1:
            long_integer = True
            exponent += 1
        else:
            significand = candidate
    if not long_integer and not decimal and not separator:
        if not negative:
            return significand
        if 0 < significand <= 1 << 63:
            return -significand
        return -float(significand)  # Includes integer -0 as negative binary64 zero.
    for digit in fraction:
        candidate = significand * 10 + int(digit)
        if candidate > (1 << 64) - 1:
            break  # serde_json ignores every remaining fractional digit.
        significand = candidate
        exponent -= 1
    if separator:
        negative_exp = explicit_exp.startswith("-")
        magnitude = 0
        for digit in explicit_exp.lstrip("+-"):
            magnitude = magnitude * 10 + int(digit)
            if magnitude > (1 << 31) - 1:
                if significand and not negative_exp:
                    raise ValueError("glTF JSON number is outside the finite binary64 range")
                return -0.0 if negative else 0.0
        exponent += -magnitude if negative_exp else magnitude
        exponent = max(-(1 << 31), min((1 << 31) - 1, exponent))
    number = float(significand)
    while abs(exponent) > 308:
        if number == 0.0:
            break
        if exponent >= 0:
            raise ValueError("glTF JSON number is outside the finite binary64 range")
        number /= 1e308
        exponent += 308
    else:
        power = float("1e" + str(abs(exponent)))
        number = number * power if exponent >= 0 else number / power
    if not math.isfinite(number):
        raise ValueError("glTF JSON number is outside the finite binary64 range")
    return -number if negative else number


def _reject_constant(token: str):
    raise ValueError(f"invalid JSON number {token}")


def parse_json(source):
    """Parse JSON with default serde_json integer types and binary64 rounding."""
    return json.loads(source, parse_int=_parsed_number, parse_float=_parsed_number,
                      parse_constant=_reject_constant)


def encode_json(value) -> str:
    """Canonical parsed JSON: sorted keys, native exact integers, binary64 bits, UTF-8 strings."""
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        if not -(1 << 63) <= value <= (1 << 64) - 1:
            raise ValueError("integer outside serde_json's native range; use parse_json on its JSON token")
        return f"i{value}"
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError("glTF JSON number is outside the finite binary64 range")
        return _real(value)
    if isinstance(value, str):
        return "t" + _hex_text(value)
    if isinstance(value, list):
        return "[" + ",".join(encode_json(item) for item in value) + "]"
    if isinstance(value, dict):
        keys = sorted(value, key=lambda key: key.encode("utf-8"))
        return "{" + ",".join(f"t{_hex_text(key)}:{encode_json(value[key])}" for key in keys) + "}"
    raise TypeError(f"unsupported JSON value {value!r}")


def _open_read_only(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True)


def _quote(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def table_lines(connection: sqlite3.Connection, table: str) -> list[str]:
    validate_name(table)
    columns = connection.execute(f"PRAGMA table_info({_quote(table)})").fetchall()
    if not columns:
        raise LookupError(f"table {table} is missing")
    rows = sorted(
        ",".join(encode_sql(value) for value in row)
        for row in connection.execute(f"SELECT * FROM {_quote(table)}")
    )
    declared = ",".join(f"{column[1]}:{column[2]}" for column in columns)
    return [f"table {table}", *rows, f"columns {declared}"]


def index_line(connection: sqlite3.Connection, index: str) -> str:
    validate_name(index)
    row = connection.execute(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ? COLLATE NOCASE", (index,)
    ).fetchone()
    if row is None or row[0] is None:
        raise LookupError(f"index {index} is missing or has no stored SQL")
    return f"index {index} {row[0]}"


def owned_files(root: Path, entries: list[str]) -> list[str]:
    """Expand directory entries (ending in `/`) to every file below them, sorted bytewise.

    A directory that does not exist covers no files; an exact file entry that is missing is an error.
    """
    paths: set[str] = set()
    for entry in entries:
        relative = entry[:-1] if entry.endswith('/') else entry
        if (not relative or "\\" in entry or ":" in entry or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in entry)
                or any(part in ("", ".", "..") or part.endswith((".", " ")) for part in relative.split('/'))):
            raise ValueError(f"owned file path {entry!r} must be a canonical dataset-relative path")
        if entry.endswith("/"):
            folder = root / entry
            if not folder.is_dir():
                continue  # a directory entry covers whatever is below it, which can be nothing
            paths.update(p.relative_to(root).as_posix() for p in folder.rglob("*") if p.is_file())
        else:
            if not (root / entry).is_file():
                raise LookupError(f"file {entry} is missing")
            paths.add(entry)
    return sorted(paths, key=lambda path: path.encode("utf-8"))


def glb_json(path: Path) -> dict:
    """Validate GLB lengths against the open file before reading its JSON payload."""
    with path.open("rb") as source:
        if path.suffix.lower() == ".gltf":
            return parse_json(source.read())
        source.seek(0, 2)
        size = source.tell()
        source.seek(0)
        if size < 20:
            raise ValueError(f"{path} has a truncated GLB header")
        magic, version, length = struct.unpack("<III", source.read(12))
        if magic != GLB_MAGIC or version != 2:
            raise ValueError(f"{path} is not a version 2 GLB")
        if length != size:
            raise ValueError(f"{path} declares a GLB length different from its file size")
        chunk_length, chunk_type = struct.unpack("<II", source.read(8))
        if chunk_type != GLB_JSON_CHUNK:
            raise ValueError(f"{path} has no JSON chunk first")
        if chunk_length > size - 20 or chunk_length % 4:
            raise ValueError(f"{path} has an invalid JSON chunk length")
        payload = source.read(chunk_length)
        if len(payload) != chunk_length:
            raise ValueError(f"{path} has a truncated JSON chunk")
        return parse_json(payload)


def gltf_extension_value(document: dict, name: str) -> str | None:
    """All extension occurrences, located by JSON pointers in stable object/array order."""
    parts = []

    def pointer(segment: str) -> str:
        return segment.replace("~", "~0").replace("/", "~1")

    def visit(value, path: str) -> None:
        if isinstance(value, dict):
            extensions = value.get("extensions")
            if isinstance(extensions, dict) and name in extensions:
                parts.append(f"{path}/extensions/{pointer(name)}={encode_json(extensions[name])}")
            for key in sorted(value, key=lambda key: key.encode("utf-8")):
                visit(value[key], f"{path}/{pointer(key)}")
        elif isinstance(value, list):
            for index, item in enumerate(value):
                visit(item, f"{path}/{index}")

    visit(document, "")
    return ";".join(parts) if parts else None


def mesh_files(root: Path) -> list[Path]:
    meshes = [p for p in root.rglob("*") if p.suffix.lower() in (".glb", ".gltf") and p.is_file()]
    return sorted(meshes, key=lambda p: p.relative_to(root).as_posix().encode("utf-8"))


def extension_lines(
    root: Path, entry: dict, meshes: list[Path] | None = None, database: Path | None = None
) -> list[str]:
    """Hash owned SQL, file and glTF content; an optional database supplies SQL objects."""
    validate_name(entry["id"])
    owns = entry.get("owns", {})
    if set(owns) - {"tables", "indexes", "files", "gltf_extensions"}:
        raise ValueError("unsupported ownership kind")
    for names in owns.values():
        for name in names:
            validate_name(name)
    lines = [ALGORITHM, str(entry["id"]), str(int(entry["version"]))]
    tables = sorted(set(owns.get("tables", [])))
    indexes = sorted(set(owns.get("indexes", [])))
    if tables or indexes:
        connection = _open_read_only(database if database is not None else root / DATABASE)
        try:
            for table in tables:
                lines.extend(table_lines(connection, table))
            for index in indexes:
                lines.append(index_line(connection, index))
        finally:
            connection.close()
    for path in owned_files(root, owns.get("files", [])):
        lines.append(f"file {path} {hashlib.sha256((root / path).read_bytes()).hexdigest()}")
    gltf_names = sorted(set(owns.get("gltf_extensions", [])))
    if gltf_names:
        if meshes is None:
            meshes = mesh_files(root)
        documents = [(m.relative_to(root).as_posix(), glb_json(m)) for m in meshes]
        for name in gltf_names:
            for path, document in documents:
                used = document.get("extensionsUsed") if isinstance(document, dict) else None
                if not isinstance(used, list) or name not in used:
                    continue
                value = gltf_extension_value(document, name)
                if value is not None:
                    digest = hashlib.sha256(value.encode("utf-8")).hexdigest()
                    lines.append(f"gltf {name} {path} {digest}")
    return lines


def extension_hash(
    root: Path, entry: dict, meshes: list[Path] | None = None, database: Path | None = None
) -> str:
    digest = hashlib.sha256()
    for line in extension_lines(root, entry, meshes, database):
        digest.update(line.encode("utf-8"))
        digest.update(b"\n")
    return digest.hexdigest()


def load_extensions(root: Path) -> list[dict]:
    manifest = json.loads((root / MANIFEST).read_text(encoding="utf-8"))
    return list(manifest.get("extensions", []))


def ownership_problems(extensions: list[dict]) -> list[str]:
    """Duplicate IDs and cross-entry ASCII-case-equivalent or nested ownership."""
    seen: dict[tuple[str, str], str] = {}
    files: list[tuple[str, str]] = []
    problems = []
    ids = set()
    for entry in extensions:
        if entry["id"] in ids:
            problems.append(f"extension {entry['id']} is listed twice")
        ids.add(entry["id"])
        owns = entry.get("owns", {})
        for kind in ("tables", "indexes", "files", "gltf_extensions"):
            local = set()
            for name in owns.get(kind, []):
                key = name if kind == "gltf_extensions" else name.translate(ASCII_CASE)
                if key in local:
                    continue
                local.add(key)
                owner = seen.setdefault((kind, key), entry["id"])
                if owner != entry["id"]:
                    problems.append(f"{kind[:-1]} {name} is owned by {owner} and {entry['id']}")
                if kind == "files":
                    for previous, previous_owner in files:
                        nested = ((key.endswith('/') and previous.startswith(key))
                                  or (previous.endswith('/') and key.startswith(previous)))
                        if previous_owner != entry["id"] and key != previous and nested:
                            problems.append(f"file {name} of {entry['id']} overlaps file {previous} of {previous_owner}")
                    files.append((key, entry["id"]))
    return problems


def core_build_identity(root: Path) -> str | None:
    """A finalized entry needs exactly one nonempty TEXT core identity."""
    if not (root / DATABASE).is_file():
        return None
    connection = _open_read_only(root / DATABASE)
    try:
        row = connection.execute(
            "SELECT type FROM sqlite_master WHERE name = 'lod_build' COLLATE NOCASE"
        ).fetchone()
        if row is None:
            return None
        if row[0] != "table":
            raise ValueError("lod_build must be a table")
        identities = connection.execute("SELECT build_identity FROM lod_build LIMIT 2").fetchall()
    finally:
        connection.close()
    if len(identities) > 1:
        raise ValueError("lod_build has more than one core identity row")
    if not identities:
        return None
    identity = identities[0][0]
    if not isinstance(identity, str):
        raise ValueError("core build identity must be TEXT")
    return identity


def check(root: Path) -> list[str]:
    """Problems with the data set's extension list; empty means every hash matches."""
    extensions = load_extensions(root)
    problems = ownership_problems(extensions)
    # Standalone legacy hash fixtures omit provenance; finalized entries declare built_from.
    finalized = [entry for entry in extensions if "built_from" in entry]
    identity = core_build_identity(root) if finalized else None
    if finalized and not identity:
        problems.append("finalized extensions require a nonempty core build identity")
    meshes = mesh_files(root) if any(entry.get("owns", {}).get("gltf_extensions") for entry in extensions) else None
    for entry in extensions:
        if identity and "built_from" in entry and entry["built_from"] != identity:
            problems.append(f"{entry['id']}: built_from does not match the core build identity")
        try:
            actual = extension_hash(root, entry, meshes)
        except (LookupError, ValueError) as error:
            problems.append(f"{entry['id']}: {error}")
            continue
        if actual != entry.get("sha256"):
            problems.append(f"{entry['id']}: sha256 {entry.get('sha256')} in the manifest, {actual} on disk")
    return problems


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Check a dataset's extension hashes.")
    parser.add_argument("root", type=Path, help="the dataset root")
    args = parser.parse_args(argv)
    try:
        problems = check(args.root)
    except (
        OSError, ValueError, TypeError, LookupError, AttributeError, OverflowError,
        sqlite3.DatabaseError, struct.error,
    ) as error:
        problems = [str(error)]
    for problem in problems:
        print(f"PROBLEM {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
