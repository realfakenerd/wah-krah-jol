//! Placed-reference links, activate parents, ownership, locks and map markers (#269 gap 3).
//!
//! Projected from all canonical winning placed records, so the legacy
//! and in-house readers publish the same rows (db-schema.md, "Reference Links and Details").
//! Layouts follow xEdit 4.1.5f `Core/wbDefinitionsTES5.pas` and the in-house schema.

use super::{extractors::SubrecordView, load_order::LoadOrder, records::RawRecord};
use rusqlite::{Result, Transaction, params};
use std::collections::HashMap;

/// Tables this module owns. A complete export drops and recreates them.
pub(crate) const REFERENCE_LINK_TABLES: [&str; 4] = [
    "reference_linked_refs",
    "reference_activate_parents",
    "reference_details",
    "reference_light_data",
];

pub(crate) const REFERENCE_LINK_TABLES_SQL: &str = "
CREATE TABLE IF NOT EXISTS reference_light_data (
    reference_id INTEGER PRIMARY KEY,
    data_size INTEGER NOT NULL,
    fov_offset REAL,
    fade_offset REAL,
    end_distance_cap REAL,
    shadow_depth_bias REAL
);
CREATE TABLE IF NOT EXISTS reference_linked_refs (
    reference_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    keyword_id INTEGER,
    linked_ref_id INTEGER,
    PRIMARY KEY (reference_id, position)
);
CREATE INDEX IF NOT EXISTS idx_reference_linked_refs_target ON reference_linked_refs(linked_ref_id);
CREATE TABLE IF NOT EXISTS reference_activate_parents (
    reference_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    parent_ref_id INTEGER,
    delay REAL,
    PRIMARY KEY (reference_id, position)
);
CREATE INDEX IF NOT EXISTS idx_reference_activate_parents_parent ON reference_activate_parents(parent_ref_id);
CREATE TABLE IF NOT EXISTS reference_details (
    reference_id INTEGER PRIMARY KEY,
    owner_id INTEGER,
    owner_rank INTEGER,
    activate_parent_flags INTEGER,
    lock_level INTEGER,
    lock_key_id INTEGER,
    lock_flags INTEGER,
    open_by_default INTEGER NOT NULL DEFAULT 0,
    map_marker INTEGER NOT NULL DEFAULT 0,
    map_marker_flags INTEGER,
    map_marker_type INTEGER,
    location_id INTEGER,
    location_ref_type_id INTEGER,
    encounter_zone_id INTEGER,
    attached_ref_id INTEGER
);
";

/// Record types an XLKR reference or XAPR parent may name.
const LINKED_REFERENCE: &[&[u8; 4]] = &[
    b"PLYR", b"ACHR", b"REFR", b"PGRE", b"PHZD", b"PMIS", b"PARW", b"PBAR", b"PBEA", b"PCON",
    b"PFLA",
];

/// XLKR's first field shares the linked-reference targets as well as accepting keywords.
const LINKED_KEYWORD: &[&[u8; 4]] = &[
    b"KYWD", b"PLYR", b"ACHR", b"REFR", b"PGRE", b"PHZD", b"PMIS", b"PARW", b"PBAR", b"PBEA",
    b"PCON", b"PFLA",
];

/// Record types an XATR attached reference may name.
const ATTACHED_REFERENCE: &[&[u8; 4]] = &[
    b"REFR", b"PGRE", b"PHZD", b"PMIS", b"PARW", b"PBAR", b"PBEA", b"PCON", b"PFLA",
];

/// What the projection needs beyond the record itself.
pub(crate) struct Context<'a> {
    pub(crate) master: &'a HashMap<u32, RawRecord>,
    /// The load order, to remap the `XLOC` key that neither reader remaps; `None` in subset
    /// exports, where the key stays NULL.
    pub(crate) order: Option<&'a LoadOrder>,
}

impl Context<'_> {
    /// Keep typed winning records and hardcoded nonzero IDs without plugin records. The in-house
    /// reader clears invalid links and the legacy reader keeps them, so this makes both agree.
    fn resolve(&self, id: Option<u32>, targets: &[&[u8; 4]]) -> Option<u32> {
        id.filter(|id| {
            *id != 0
                && self.master.get(id).map_or(*id < 0x800, |record| {
                    !record.is_deleted() && targets.contains(&&record.record_type)
                })
        })
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// The subrecord when its length is one the in-house schema accepts; the in-house reader leaves a
/// field of another length out of its runtime projection, so both readers skip it here.
fn sized<'a>(bytes: Option<&'a [u8]>, sizes: &[usize]) -> Option<&'a [u8]> {
    bytes.filter(|bytes| sizes.contains(&bytes.len()))
}

/// Writes the link and detail rows of one winning placed reference.
pub(crate) fn insert(
    tx: &Transaction<'_>,
    context: &Context<'_>,
    record: &RawRecord,
    view: &SubrecordView<'_>,
) -> Result<()> {
    let id = record.form_id;
    for table in REFERENCE_LINK_TABLES {
        tx.prepare_cached(&format!("DELETE FROM {table} WHERE reference_id=?1"))?
            .execute([id])?;
    }

    // xEdit cce1c42403b279957e1e78bfe38100b29f39f72c, wbDefinitionsTES5.pas,
    // REFR/XLIG: four little-endian floats and an optional four-byte tail (16/20 bytes).
    // Preserve presence and malformed size separately from absent XLIG. Non-finite fields
    // are NULL; the canonical subrecord archive retains the original bits.
    if let Some(bytes) = view.find(b"XLIG") {
        let real = |offset: usize| {
            sized(Some(bytes), &[16, 20])
                .and_then(|bytes| bytes.get(offset..offset + 4))
                .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("XLIG float")))
                .filter(|value| value.is_finite())
        };
        tx.prepare_cached("INSERT INTO reference_light_data VALUES (?1,?2,?3,?4,?5,?6)")?
            .execute(params![
                id,
                bytes.len() as i64,
                real(0),
                real(4),
                real(8),
                real(12)
            ])?;
    }

    // XLKR: one subrecord per link, a keyword and a reference, or (14 official references) the
    // reference alone.
    for (position, link) in view
        .find_all(b"XLKR")
        .filter_map(|bytes| sized(Some(bytes), &[4, 8]))
        .enumerate()
    {
        let (keyword, reference) = match link.len() {
            8 => (u32_at(link, 0), u32_at(link, 4)),
            _ => (None, u32_at(link, 0)),
        };
        tx.prepare_cached(
            "INSERT INTO reference_linked_refs(reference_id, position, keyword_id, linked_ref_id) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            id,
            position as i64,
            context.resolve(keyword, LINKED_KEYWORD),
            context.resolve(reference, LINKED_REFERENCE),
        ])?;
    }

    // XAPR: one subrecord per activate parent, a reference then an f32 delay.
    for (position, parent) in view
        .find_all(b"XAPR")
        .filter_map(|bytes| sized(Some(bytes), &[8]))
        .enumerate()
    {
        let delay = f32::from_le_bytes(parent[4..8].try_into().expect("eight-byte XAPR"));
        tx.prepare_cached(
            "INSERT INTO reference_activate_parents(reference_id, position, parent_ref_id, delay) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            id,
            position as i64,
            context.resolve(u32_at(parent, 0), LINKED_REFERENCE),
            delay.is_finite().then_some(f64::from(delay)),
        ])?;
    }

    let link = |tag: &[u8; 4], targets: &[&[u8; 4]]| {
        context.resolve(
            sized(view.find(tag), &[4]).and_then(|bytes| u32_at(bytes, 0)),
            targets,
        )
    };
    let owner = link(b"XOWN", &[b"FACT", b"ACHR", b"NPC_"]);
    let owner_rank = sized(view.find(b"XRNK"), &[4]).and_then(|bytes| u32_at(bytes, 0));
    let activate_parent_flags = sized(view.find(b"XAPD"), &[1]).map(|bytes| bytes[0]);
    // XLOC: level byte, three unused, key FormID, flags byte (0x04 leveled lock), padding. It is
    // untyped bytes in the in-house schema, so neither reader remaps the key; it is remapped here
    // through the supplying plugin's master list.
    let lock = view.find(b"XLOC").filter(|bytes| !bytes.is_empty());
    let lock_key = lock.and_then(|bytes| u32_at(bytes, 4)).and_then(|raw| {
        let order = context.order?;
        let local = crate::records::source_identity::header_runtime_id(
            b"KEYM",
            raw,
            record.load_order as usize,
            order,
        )?;
        context.resolve(Some(local), &[b"KEYM"])
    });
    let open_by_default = view.find(b"ONAM").is_some();
    let map_marker = view.find(b"XMRK").is_some();
    let location = link(b"XLCN", &[b"LCTN"]);
    let location_ref_type = link(b"XLRL", &[b"LCRT", b"LCTN"]);
    let encounter_zone = link(b"XEZN", &[b"ECZN"]);
    let attached = link(b"XATR", ATTACHED_REFERENCE);
    let any = owner.is_some()
        || owner_rank.is_some()
        || activate_parent_flags.is_some()
        || lock.is_some()
        || open_by_default
        || map_marker
        || location.is_some()
        || location_ref_type.is_some()
        || encounter_zone.is_some()
        || attached.is_some();
    if any {
        tx.prepare_cached(
            "INSERT INTO reference_details(reference_id, owner_id, owner_rank, activate_parent_flags, lock_level, lock_key_id, lock_flags, open_by_default, map_marker, map_marker_flags, map_marker_type, location_id, location_ref_type_id, encounter_zone_id, attached_ref_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?
        .execute(params![
            id,
            owner,
            owner_rank.map(|rank| rank as i32),
            activate_parent_flags,
            lock.and_then(|bytes| bytes.first()).copied(),
            lock_key,
            lock.and_then(|bytes| bytes.get(8)).copied(),
            open_by_default,
            map_marker,
            map_marker
                .then(|| sized(view.find(b"FNAM"), &[1]).map(|bytes| bytes[0]))
                .flatten(),
            map_marker
                .then(|| sized(view.find(b"TNAM"), &[2]).map(|bytes| bytes[0]))
                .flatten(),
            location,
            location_ref_type,
            encounter_zone,
            attached,
        ])?;
    }
    Ok(())
}
