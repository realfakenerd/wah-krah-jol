//! Pure resolution of original per-file words; not used by existing conversion paths.
use crate::{
    form_key::{FormKey, PluginId},
    plugin_table::PluginTable,
};

/// A native source word and its containing plugin, never a packed runtime ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormId {
    pub raw: u32,
    pub containing_plugin: PluginId,
}
/// Reference fields may permit raw-zero null; record-identity contexts can retain local zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroPolicy {
    Null,
    Identity,
}
/// Per-reference outcomes. Callers must skip/clear bad data with a counted warning and continue.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Resolved(FormKey),
    Null,
    /// Index exceeds MAST count: project onto supplier, matching permissive runtime headers.
    /// This is recovery, not authenticated original-owner proof.
    SelfProjected {
        key: FormKey,
        index: u8,
    },
    UnknownSupplier(PluginId),
    UnknownOwner(String),
    InvalidWidth {
        owner: PluginId,
        local: u32,
        maximum: u32,
    },
    /// Missing/nonfinite versions cannot authorize extended light locals.
    IncompatibleHeaderVersion {
        owner: PluginId,
        supplier: PluginId,
        local: u32,
        owner_version: Option<f32>,
        supplier_version: Option<f32>,
    },
}
impl Resolution {
    /// Inspect only successfully resolved/projected identities; null and bad data have no key.
    pub fn key(&self) -> Option<FormKey> {
        match self {
            Self::Resolved(key) | Self::SelfProjected { key, .. } => Some(*key),
            _ => None,
        }
    }
}
/// Validate extended-range support without assuming an absent header is new enough.
fn extended(version: Option<f32>) -> bool {
    version.is_some_and(|v| v.is_finite() && v >= 1.71)
}

/// Resolve through the containing plugin's original master list, independent of global slots.
/// Out-of-range indices have exactly one recovery policy: SelfProjected after validation.
/// No logging/global errors occur here; later consumers must count/report rejected fields.
pub fn resolve(table: &PluginTable, source: SourceFormId, zero: ZeroPolicy) -> Resolution {
    let Some(supplier) = table.entry(source.containing_plugin).filter(|e| e.loaded) else {
        return Resolution::UnknownSupplier(source.containing_plugin);
    };
    if source.raw == 0 && zero == ZeroPolicy::Null {
        return Resolution::Null;
    }
    let index = (source.raw >> 24) as usize;
    let owner = if let Some(name) = supplier.facts.masters.get(index) {
        let Some(owner) = table
            .id(name)
            .and_then(|id| table.entry(id))
            .filter(|e| e.loaded)
        else {
            return Resolution::UnknownOwner(name.clone());
        };
        owner
    } else {
        supplier
    };
    let local = source.raw & 0x00ff_ffff;
    if table.is_light(owner.id) {
        if local > 0xfff {
            return Resolution::InvalidWidth {
                owner: owner.id,
                local,
                maximum: 0xfff,
            };
        }
        if local < 0x800
            && (!extended(owner.facts.header_version) || !extended(supplier.facts.header_version))
        {
            return Resolution::IncompatibleHeaderVersion {
                owner: owner.id,
                supplier: supplier.id,
                local,
                owner_version: owner.facts.header_version,
                supplier_version: supplier.facts.header_version,
            };
        }
    }
    // A source word inherently has a 24-bit local; construction is still checked.
    match FormKey::new(owner.id, local) {
        Ok(key) if index > supplier.facts.masters.len() => Resolution::SelfProjected {
            key,
            index: index as u8,
        },
        Ok(key) => Resolution::Resolved(key),
        Err(_) => Resolution::InvalidWidth {
            owner: owner.id,
            local,
            maximum: 0x00ff_ffff,
        },
    }
}
