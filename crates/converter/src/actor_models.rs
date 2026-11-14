//! Actor model catalog: effective Traits root -> skin -> armature -> biped parts and head path.
//!
//! The exporter publishes this catalog to the world database (`actor_models`,
//! `actor_model_parts`). The tables are additive and optional: consumers must
//! check that they exist before reading them. Nothing here reads raw
//! `records.data`; the caller hands the merged record map in and gets rows back.

use crate::esm::{extractors::SubrecordView, load_order::StableId, records::RawRecord};
use std::collections::{BTreeSet, HashMap, HashSet};

/// `DefaultRace`, the armature race every humanoid body is shared through.
pub const DEFAULT_RACE_FORM_ID: u32 = 25;

/// `RACE/DATA` flag bit for a race whose heads come from FaceGen.
const RACE_FACEGEN_HEAD: u32 = 0x2;

/// `ARMA/BOD2` biped slot bits, the slot number minus 30.
const BIPED_BODY: u32 = 1 << 2;
const BIPED_HANDS: u32 = 1 << 3;
const BIPED_FEET: u32 = 1 << 7;

/// Directory the FaceGen geometry lives in.
const FACEGEN_DIR: &str = "meshes/actors/character/facegendata/facegeom/";
/// Directory the race/sex default head NIFs live in.
const DEFAULT_HEAD_DIR: &str = "meshes/actors/character/character assets/";

/// The biped slot a resolved armature fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActorModelSlot {
    /// The main body (`BOD2` bit 2).
    Body,
    /// The hands (`BOD2` bit 3).
    Hands,
    /// The feet (`BOD2` bit 7).
    Feet,
}

impl ActorModelSlot {
    /// The slot string stored in `actor_model_parts.slot`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Hands => "hands",
            Self::Feet => "feet",
        }
    }
}

/// Why a base has no complete model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnresolvedReason {
    /// The effective Traits root has no `ACBS`; sex defaults to male.
    MissingAcbs,
    /// The `NPC_` has no `RNAM` race link.
    NoRnam,
    /// Use Traits is enabled, but TPLT is absent, zero, or malformed.
    MissingTraitsTemplate,
    /// TPLT points at a deleted record.
    DeletedTraitsTemplate,
    /// TPLT points at a record other than `NPC_` or `LVLN`.
    WrongTypeTraitsTemplate,
    /// Use Traits chain contains an NPC more than once.
    TraitsTemplateCycle,
    /// TPLT points at `LVLN`, whose NPC is selected at runtime.
    LeveledTraitsTemplate,
    /// The NPC's explicit WNAM override is zero.
    ZeroNpcSkinOverride,
    /// The NPC's explicit WNAM override does not resolve.
    MissingNpcSkinOverride,
    /// The NPC's explicit WNAM override points at a deleted record.
    DeletedNpcSkinOverride,
    /// The NPC's explicit WNAM override resolves to a non-ARMO record.
    WrongTypeNpcSkinOverride,
    /// The `RNAM` race has no record in the load order.
    NoRaceRecord,
    /// The race has no `WNAM` skin armor.
    RaceNoWnam,
    /// The `WNAM` armor has no record in the load order.
    NoArmoRecord,
    /// The armor has no `MODL` armature list.
    ArmoNoModl,
    /// No armature matched the primary or additional race, or `DefaultRace`.
    NoMatchingArma,
    /// No head is available: the race has no FaceGen flag and no default head.
    NoFacegenAndNoDefault,
    /// An armature's `BOD2` has no biped slot this catalog stores.
    UnrecognizedSlot,
    /// Multiple head-slot models remain without a normal body in a creature skin.
    AmbiguousCreatureBody,
}

impl UnresolvedReason {
    /// The reason string used in diagnostics and the retail breakdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingAcbs => "missing_acbs",
            Self::NoRnam => "no_rnam",
            Self::MissingTraitsTemplate => "missing_traits_template",
            Self::DeletedTraitsTemplate => "deleted_traits_template",
            Self::WrongTypeTraitsTemplate => "wrong_type_traits_template",
            Self::TraitsTemplateCycle => "traits_template_cycle",
            Self::LeveledTraitsTemplate => "leveled_traits_template",
            Self::ZeroNpcSkinOverride => "zero_npc_skin_override",
            Self::MissingNpcSkinOverride => "missing_npc_skin_override",
            Self::DeletedNpcSkinOverride => "deleted_npc_skin_override",
            Self::WrongTypeNpcSkinOverride => "wrong_type_npc_skin_override",
            Self::NoRaceRecord => "no_race_record",
            Self::RaceNoWnam => "race_no_wnam",
            Self::NoArmoRecord => "no_armo_record",
            Self::ArmoNoModl => "armo_no_modl",
            Self::NoMatchingArma => "no_matching_arma",
            Self::NoFacegenAndNoDefault => "no_facegen_and_no_default",
            Self::UnrecognizedSlot => "unrecognized_slot",
            Self::AmbiguousCreatureBody => "ambiguous_creature_body",
        }
    }
}

/// One `actor_models` row: the body/head summary for a non-deleted `NPC_` base.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorModelRow {
    /// The `NPC_` FormID.
    pub id: u32,
    /// The effective Traits root's race FormID, or `0` when it carries no `RNAM`.
    pub race_id: u32,
    /// `male` or `female`, from the effective Traits root's `NPC_/ACBS` bit 0.
    pub sex: &'static str,
    /// Effective `ARMO` FormID: appearance-root `WNAM`, else effective-race `WNAM`.
    pub skin_armo_id: Option<u32>,
    /// FaceGen geometry or the race/sex default head NIF.
    pub head_model: Option<String>,
    /// `facegen` or `default`.
    pub head_source: Option<&'static str>,
    /// `body`, `body_head` or `unresolved`.
    pub resolution: &'static str,
    /// `NPC_/QNAM`, the body skin tone as RGB in 0..1 on the encoded (not linearised) scale.
    /// The game hands it to the body, hands and feet shaders as their tint colour.
    pub skin_tone: Option<[f32; 3]>,
    /// `NPC_/NAM7`, the body's own-record weight in 0..100. Traits inheritance is unresolved;
    /// absent, malformed, non-finite or out-of-range values mean no weight.
    pub weight: Option<f32>,
}

/// One `actor_model_parts` row: a biped slot's model path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorModelPartRow {
    /// The `NPC_` FormID the part belongs to.
    pub npc_id: u32,
    /// The biped slot.
    pub slot: ActorModelSlot,
    /// The `MOD2`/`MOD3` path, as the record wrote it.
    pub model_path: String,
    /// The armature's own `RNAM`: the NPC's race for an exact match, else `DefaultRace`.
    pub armature_race_id: u32,
    /// The armature's skin texture set for the NPC's sex (`NAM0` male, `NAM1` female), which the
    /// game applies over the part NIF's own textures.
    pub skin_texture_set_id: Option<u32>,
}

/// One unresolved-or-notable base, with the reason it was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorModelDiagnostic {
    /// The `NPC_` FormID the diagnostic is about.
    pub npc_id: u32,
    /// The reason.
    pub reason: UnresolvedReason,
}

/// The full resolver output: rows, part rows and diagnostics.
#[derive(Debug, Default, Clone)]
pub struct ActorModelCatalog {
    /// One row per `NPC_` base, sorted by FormID.
    pub models: Vec<ActorModelRow>,
    /// Part rows, sorted by `(npc_id, slot)`.
    pub parts: Vec<ActorModelPartRow>,
    /// Diagnostics, sorted by `(npc_id, reason)`.
    pub diagnostics: Vec<ActorModelDiagnostic>,
}

/// Resolves every `NPC_` base in `records` into a body/head catalog.
///
/// `plugin_ids` maps an `NPC_` FormID to its defining plugin filename and
/// plugin-local FormID (`formid_map.plugin_name`/`formid_map.internal_id`),
/// which the FaceGen path needs. An effective Traits root with no entry still
/// resolves a body, but its head tries the default NIF.
#[must_use]
pub fn resolve_actor_models(
    records: &HashMap<u32, RawRecord>,
    plugin_ids: &HashMap<u32, StableId>,
) -> ActorModelCatalog {
    let mut catalog = ActorModelCatalog::default();
    let mut npc_ids: Vec<u32> = records
        .iter()
        .filter(|(_, record)| &record.record_type == b"NPC_" && !record.is_deleted())
        .map(|(id, _)| *id)
        .collect();
    npc_ids.sort_unstable();

    for id in npc_ids {
        let source_view = SubrecordView::new(&records[&id].subrecords);
        let Some(root_id) = traits_root(id, records, &mut catalog) else {
            catalog
                .models
                .push(unresolved_row(id, npc_weight(&source_view)));
            continue;
        };
        let view = SubrecordView::new(&records[&root_id].subrecords);
        let (female, acbs_missing) = npc_sex(&view);
        if acbs_missing {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id: id,
                reason: UnresolvedReason::MissingAcbs,
            });
        }
        let mut row = ActorModelRow {
            id,
            race_id: 0,
            sex: if female { "female" } else { "male" },
            skin_armo_id: None,
            head_model: None,
            head_source: None,
            resolution: "unresolved",
            // QNAM is not among the confirmed Traits inheritance fields. Reading the resolved
            // root view here would silently inherit the root's tone, so Traits-enabled sources
            // have no published tone until that behavior is proven.
            skin_tone: (!traits_enabled(&source_view))
                .then(|| npc_skin_tone(&source_view))
                .flatten(),
            // NAM7 follows the conservative own-record policy from QNAM; never infer Traits.
            weight: npc_weight(&source_view),
        };

        let Some(race_id) = view.get_form_id(b"RNAM").filter(|race_id| *race_id != 0) else {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id: id,
                reason: UnresolvedReason::NoRnam,
            });
            catalog.models.push(row);
            continue;
        };
        row.race_id = race_id;

        let Some(race) = records
            .get(&race_id)
            .filter(|race| &race.record_type == b"RACE" && !race.is_deleted())
        else {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id: id,
                reason: UnresolvedReason::NoRaceRecord,
            });
            catalog.models.push(row);
            continue;
        };
        let race_view = SubrecordView::new(&race.subrecords);

        let skin_id = effective_skin(records, &view, &race_view, id, &mut catalog);
        let authoring = crate::animation::authoring::read_race_authoring_fields(
            race.subrecords
                .iter()
                .map(|(tag, data)| (tag.as_slice(), data.as_slice())),
        )
        .ok();
        let creature_project = authoring
            .and_then(|row| {
                if female {
                    row.female_project
                } else {
                    row.male_project
                }
            })
            .is_some_and(|project| !project.starts_with("meshes/actors/character/"));
        let creature_head_body = race_view.find(b"DATA").is_some_and(|data| data.len() >= 36)
            && !race_uses_facegen(&race_view)
            && creature_project;
        let mut parts = resolve_body(
            records,
            skin_id,
            race_id,
            female,
            creature_head_body,
            id,
            &mut catalog,
        );
        let head = resolve_head(&race_view, female, plugin_ids.get(&root_id), &parts);
        if head.is_none() {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id: id,
                reason: UnresolvedReason::NoFacegenAndNoDefault,
            });
        }
        if let Some((path, source)) = head {
            row.head_model = Some(path);
            row.head_source = Some(source);
        }
        row.resolution = if parts.is_empty() {
            "unresolved"
        } else if row.head_model.is_some() {
            "body_head"
        } else {
            "body"
        };
        row.skin_armo_id = skin_id;
        catalog.models.push(row);
        catalog.parts.append(&mut parts);
    }

    catalog.models.sort_by_key(|row| row.id);
    catalog.parts.sort_by_key(|part| (part.npc_id, part.slot));
    catalog
        .diagnostics
        .sort_by_key(|diagnostic| (diagnostic.npc_id, diagnostic.reason));
    catalog
}

fn unresolved_row(id: u32, weight: Option<f32>) -> ActorModelRow {
    ActorModelRow {
        id,
        race_id: 0,
        sex: "male",
        skin_armo_id: None,
        head_model: None,
        head_source: None,
        resolution: "unresolved",
        skin_tone: None,
        weight,
    }
}

/// Follows Use Traits links without a depth cap. Bit 0 is ACBS `u16` at 0x12.
fn traits_root(
    npc_id: u32,
    records: &HashMap<u32, RawRecord>,
    catalog: &mut ActorModelCatalog,
) -> Option<u32> {
    let mut current = npc_id;
    let mut visited = HashSet::from([npc_id]);
    loop {
        let record = records.get(&current)?;
        let view = SubrecordView::new(&record.subrecords);
        if !traits_enabled(&view) {
            return Some(current);
        }
        let template_id = view.get_form_id(b"TPLT").filter(|id| *id != 0);
        let Some(template_id) = template_id else {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id,
                reason: UnresolvedReason::MissingTraitsTemplate,
            });
            return None;
        };
        let Some(template_record) = records.get(&template_id) else {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id,
                reason: UnresolvedReason::MissingTraitsTemplate,
            });
            return None;
        };
        if template_record.is_deleted() {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id,
                reason: UnresolvedReason::DeletedTraitsTemplate,
            });
            return None;
        }
        match &template_record.record_type {
            b"LVLN" => {
                catalog.diagnostics.push(ActorModelDiagnostic {
                    npc_id,
                    reason: UnresolvedReason::LeveledTraitsTemplate,
                });
                return None;
            }
            b"NPC_" => {}
            _ => {
                catalog.diagnostics.push(ActorModelDiagnostic {
                    npc_id,
                    reason: UnresolvedReason::WrongTypeTraitsTemplate,
                });
                return None;
            }
        }
        if !visited.insert(template_id) {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id,
                reason: UnresolvedReason::TraitsTemplateCycle,
            });
            return None;
        }
        current = template_id;
    }
}

fn traits_enabled(view: &SubrecordView<'_>) -> bool {
    view.find(b"ACBS")
        .filter(|data| data.len() >= 20)
        .is_some_and(|data| u16::from_le_bytes([data[18], data[19]]) & 0x0001 != 0)
}

fn effective_skin(
    records: &HashMap<u32, RawRecord>,
    appearance: &SubrecordView<'_>,
    race: &SubrecordView<'_>,
    npc_id: u32,
    catalog: &mut ActorModelCatalog,
) -> Option<u32> {
    if let Some(data) = appearance.find(b"WNAM") {
        let override_id =
            (data.len() >= 4).then(|| u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
        let reason = match override_id {
            Some(0) => Some(UnresolvedReason::ZeroNpcSkinOverride),
            Some(id) => match records.get(&id) {
                None => Some(UnresolvedReason::MissingNpcSkinOverride),
                Some(record) if record.is_deleted() => {
                    Some(UnresolvedReason::DeletedNpcSkinOverride)
                }
                Some(record) if &record.record_type != b"ARMO" => {
                    Some(UnresolvedReason::WrongTypeNpcSkinOverride)
                }
                Some(_) => return override_id,
            },
            None => Some(UnresolvedReason::MissingNpcSkinOverride),
        };
        if let Some(reason) = reason {
            catalog
                .diagnostics
                .push(ActorModelDiagnostic { npc_id, reason });
        }
    }
    race.get_form_id(b"WNAM").filter(|id| *id != 0)
}

/// `NPC_/ACBS` bit 0 is female; a missing `ACBS` defaults to male.
fn npc_sex(view: &SubrecordView<'_>) -> (bool, bool) {
    match view.find(b"ACBS") {
        Some(data) if data.len() >= 4 => (
            u32::from_le_bytes([data[0], data[1], data[2], data[3]]) & 1 == 1,
            false,
        ),
        _ => (false, true),
    }
}

/// `NPC_/QNAM`: three little-endian `f32` (red, green, blue). A short or non-finite value is no
/// tone at all, so the engine keeps the model's own tint rather than drawing a garbage colour.
fn npc_skin_tone(view: &SubrecordView<'_>) -> Option<[f32; 3]> {
    let data = view.find(b"QNAM").filter(|data| data.len() >= 12)?;
    let channel = |offset: usize| {
        Some(f32::from_le_bytes(
            data[offset..offset + 4].try_into().ok()?,
        ))
    };
    let tone = [channel(0)?, channel(4)?, channel(8)?];
    tone.iter().all(|value| value.is_finite()).then_some(tone)
}

/// Follows effective skin armor -> `ARMO/MODL` -> compatible `ARMA` tiers, and fills biped slots.
/// `NPC_/NAM7`: exactly one little-endian `f32` in 0..=100. A malformed, non-finite or out-of-
/// range value is no weight, so the engine keeps the heavy body.
fn npc_weight(view: &SubrecordView<'_>) -> Option<f32> {
    let data = view.find(b"NAM7").filter(|data| data.len() == 4)?;
    let weight = f32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    (weight.is_finite() && (0.0..=100.0).contains(&weight)).then_some(weight)
}

/// Follows effective skin armor -> `ARMO/MODL` -> compatible `ARMA` tiers, and fills biped slots.
fn resolve_body(
    records: &HashMap<u32, RawRecord>,
    skin_id: Option<u32>,
    race_id: u32,
    female: bool,
    creature_head_body: bool,
    npc_id: u32,
    catalog: &mut ActorModelCatalog,
) -> Vec<ActorModelPartRow> {
    let Some(skin_id) = skin_id else {
        catalog.diagnostics.push(ActorModelDiagnostic {
            npc_id,
            reason: UnresolvedReason::RaceNoWnam,
        });
        return Vec::new();
    };
    let Some(armor) = records
        .get(&skin_id)
        .filter(|armor| &armor.record_type == b"ARMO" && !armor.is_deleted())
    else {
        catalog.diagnostics.push(ActorModelDiagnostic {
            npc_id,
            reason: UnresolvedReason::NoArmoRecord,
        });
        return Vec::new();
    };
    let armatures = SubrecordView::new(&armor.subrecords).get_form_ids(b"MODL");
    if armatures.is_empty() {
        catalog.diagnostics.push(ActorModelDiagnostic {
            npc_id,
            reason: UnresolvedReason::ArmoNoModl,
        });
        return Vec::new();
    }

    // Preserve authored order across exact and additional-race compatible entries.
    let mut compatible: Vec<(u32, u32)> = Vec::new();
    let mut default: Vec<(u32, u32)> = Vec::new();
    for armature_id in armatures {
        let Some(armature) = records
            .get(&armature_id)
            .filter(|armature| &armature.record_type == b"ARMA" && !armature.is_deleted())
        else {
            continue;
        };
        let armature_view = SubrecordView::new(&armature.subrecords);
        let armature_race_id = armature_view.get_form_id(b"RNAM").unwrap_or(0);
        let exact_race = armature_race_id == race_id;
        let additional_race = armature_view.get_form_ids(b"MODL").into_iter().any(|id| {
            id != 0
                && id == race_id
                && records
                    .get(&id)
                    .is_some_and(|race| &race.record_type == b"RACE" && !race.is_deleted())
        });
        if exact_race || additional_race {
            compatible.push((armature_id, armature_race_id));
        } else if armature_race_id == DEFAULT_RACE_FORM_ID {
            default.push((armature_id, DEFAULT_RACE_FORM_ID));
        }
    }

    let mut parts: Vec<ActorModelPartRow> = Vec::new();
    let mut creature_candidates = Vec::new();
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();
    for (armature_id, armature_race_id) in compatible.into_iter().chain(default) {
        let armature = &records[&armature_id];
        let view = SubrecordView::new(&armature.subrecords);
        let bod = view.find(b"BOD2").or_else(|| view.find(b"BODT"));
        let creature_head = creature_head_body
            && bod
                .filter(|data| data.len() >= 4)
                .is_some_and(|data| u32::from_le_bytes(data[..4].try_into().unwrap()) & 1 != 0);
        let standard_slot = classify_slot(bod);
        let slot = match standard_slot.or_else(|| creature_head.then_some(ActorModelSlot::Body)) {
            Some(slot) => slot,
            None => {
                catalog.diagnostics.push(ActorModelDiagnostic {
                    npc_id,
                    reason: UnresolvedReason::UnrecognizedSlot,
                });
                continue;
            }
        };
        // An ARMA without a female model uses MOD2 for both sexes (UESP ARMA format).
        let male = || view.get_string(b"MOD2").filter(|path| !path.is_empty());
        let path = if female {
            view.get_string(b"MOD3")
                .filter(|path| !path.is_empty())
                .or_else(male)
        } else {
            male()
        };
        let Some(path) = path else {
            continue;
        };
        if standard_slot.is_none() && creature_head {
            let skin_tag = if female { b"NAM1" } else { b"NAM0" };
            let candidate = ActorModelPartRow {
                npc_id,
                slot: ActorModelSlot::Body,
                model_path: path,
                armature_race_id,
                skin_texture_set_id: view.get_form_id(skin_tag).filter(|id| *id != 0),
            };
            if !creature_candidates.iter().any(|row: &ActorModelPartRow| {
                row.model_path.eq_ignore_ascii_case(&candidate.model_path)
            }) {
                creature_candidates.push(candidate);
            }
            continue;
        }
        if parts.iter().any(|part| part.slot == slot) || !seen_paths.insert(path.to_lowercase()) {
            continue;
        }
        // No fallback to the other sex's set: an armature without one keeps the NIF's textures.
        let skin_tag = if female { b"NAM1" } else { b"NAM0" };
        parts.push(ActorModelPartRow {
            npc_id,
            slot,
            model_path: path,
            armature_race_id,
            skin_texture_set_id: view.get_form_id(skin_tag).filter(|id| *id != 0),
        });
    }

    // Many authored creatures (horse, bear, atronachs, mammoth) put their whole
    // skin in slot 30. It is a body only when a non-FaceGen creature project has
    // no ordinary body and exactly one compatible head-slot model. Never let a
    // head candidate displace an existing biped body or choose between two models.
    if !parts.iter().any(|part| part.slot == ActorModelSlot::Body) {
        if creature_candidates.len() == 1 {
            parts.push(creature_candidates.pop().unwrap());
        } else if creature_candidates.len() > 1 {
            catalog.diagnostics.push(ActorModelDiagnostic {
                npc_id,
                reason: UnresolvedReason::AmbiguousCreatureBody,
            });
        }
    }
    if parts.is_empty() {
        catalog.diagnostics.push(ActorModelDiagnostic {
            npc_id,
            reason: UnresolvedReason::NoMatchingArma,
        });
    }
    parts
}

/// The first stored biped slot in `BOD2`, or in retail `ARMA`'s `BODT`, in body, hands, feet order.
fn classify_slot(bod: Option<&[u8]>) -> Option<ActorModelSlot> {
    let data = bod.filter(|data| data.len() >= 4)?;
    let slots = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if slots & BIPED_BODY != 0 {
        Some(ActorModelSlot::Body)
    } else if slots & BIPED_HANDS != 0 {
        Some(ActorModelSlot::Hands)
    } else if slots & BIPED_FEET != 0 {
        Some(ActorModelSlot::Feet)
    } else {
        None
    }
}

/// FaceGen geometry when the race uses it, else the race/sex default head.
fn resolve_head(
    race_view: &SubrecordView<'_>,
    female: bool,
    identity: Option<&StableId>,
    parts: &[ActorModelPartRow],
) -> Option<(String, &'static str)> {
    if race_uses_facegen(race_view)
        && let Some(identity) = identity
    {
        let plugin = identity.plugin.to_lowercase();
        let local_id = identity.local_id;
        return Some((
            format!("{FACEGEN_DIR}{plugin}/{local_id:08x}.nif"),
            "facegen",
        ));
    }
    if !is_humanoid(parts) {
        return None;
    }
    let variant = match race_view
        .get_string(b"EDID")
        .unwrap_or_default()
        .to_lowercase()
    {
        name if name.contains("argonian") => "argonian",
        name if name.contains("khajiit") => "khajiit",
        _ => "",
    };
    let sex = if female { "female" } else { "male" };
    Some((
        format!("{DEFAULT_HEAD_DIR}{sex}head{variant}.nif"),
        "default",
    ))
}

/// Whether the race's `DATA` marks it as using FaceGen heads.
fn race_uses_facegen(race_view: &SubrecordView<'_>) -> bool {
    race_view
        .find(b"DATA")
        .filter(|data| data.len() >= 36)
        .is_some_and(|data| {
            u32::from_le_bytes([data[32], data[33], data[34], data[35]]) & RACE_FACEGEN_HEAD != 0
        })
}

/// A default head only fits the shared humanoid body, never a creature body.
fn is_humanoid(parts: &[ActorModelPartRow]) -> bool {
    parts.iter().any(|part| {
        let path = part.model_path.to_lowercase();
        path.contains("malebody") || path.contains("femalebody")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qnam(bytes: Vec<u8>) -> Option<[f32; 3]> {
        let subrecords = vec![(b"QNAM".to_vec(), bytes)];
        npc_skin_tone(&SubrecordView::new(&subrecords))
    }

    #[test]
    fn qnam_decodes_three_little_endian_floats() {
        // Alvor's (`00013475`) bytes from retail `Skyrim.esm`: 198/255, 176/255, 168/255.
        let alvor = qnam(
            [
                0xc8, 0xc6, 0x46, 0x3f, 0xb1, 0xb0, 0x30, 0x3f, 0xa9, 0xa8, 0x28, 0x3f,
            ]
            .to_vec(),
        )
        .unwrap();
        for (channel, encoded) in alvor.iter().zip([198.0_f32, 176.0, 168.0]) {
            assert!((channel * 255.0 - encoded).abs() < 1e-3, "{alvor:?}");
        }
        let bytes: Vec<u8> = [0.25_f32, 0.5, 1.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(qnam(bytes), Some([0.25, 0.5, 1.0]));
    }

    #[test]
    fn a_missing_short_or_non_finite_qnam_is_no_tone() {
        assert_eq!(npc_skin_tone(&SubrecordView::new(&[])), None);
        assert_eq!(qnam(vec![0; 11]), None);
        let nan: Vec<u8> = [0.5_f32, f32::NAN, 0.5]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(qnam(nan), None);
    }
}
