//! Pure pack-scoped plugin allocation. No persistence, filesystem reads or load ordering.
use crate::{
    form_key::{FormKey, IdentityError, NamedFormKey, PluginId, validate_plugin_name},
    plugin_selection::normalize_plugin_name,
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

/// Source header flags; medium is retained but no Skyrim medium packing is invented.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginFlags {
    pub master: bool,
    pub light: bool,
    pub medium: bool,
}
/// Mutable source facts, kept separate from the numeric/name identity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginFacts {
    pub flags: PluginFlags,
    /// Original TES4 HEDR version; None means unavailable, never an assumed version.
    pub header_version: Option<f32>,
    /// Original ordered file-local MAST names, not global slots.
    pub masters: Vec<String>,
    /// Cache validation only; changing content never changes identity allocation.
    pub content_hash: Option<[u8; 32]>,
}
/// Immutable table row; a retained but absent name remains addressable for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    pub id: PluginId,
    pub normalized_name: String,
    pub display_name: String,
    pub loaded: bool,
    pub facts: PluginFacts,
}
/// Construction/lookup rejection; callers decide local recovery and warning aggregation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    InvalidName(IdentityError),
    InvalidIdentity(IdentityError),
    DuplicateName(String),
    Exhausted,
    UnknownPlugin(String),
    UnknownId(PluginId),
}
impl fmt::Display for TableError {
    /// Keep allocation and lookup errors explicit for callers' counted diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plugin table: {self:?}")
    }
}
impl std::error::Error for TableError {}
/// A numeric key has meaning only with this table. Fresh tables begin new allocation scopes.
#[derive(Debug, Clone, Default)]
pub struct PluginTable {
    entries: Vec<PluginEntry>,
    names: BTreeMap<String, PluginId>,
}
impl PluginTable {
    /// Allocate sorted normalized unique names deterministically, starting at one.
    pub fn fresh(plugins: Vec<(String, PluginFacts)>) -> Result<Self, TableError> {
        Self::default().retained(plugins)
    }
    /// API-only retained mode: preserve old numbers/tombstones, append new sorted names.
    /// No persistence/lineage policy is provided. Duplicate input fails before any mutation.
    /// Preserve malformed MAST names for per-reference UnknownOwner diagnostics.
    pub fn retained(&self, plugins: Vec<(String, PluginFacts)>) -> Result<Self, TableError> {
        let mut sorted = BTreeMap::new();
        for (display, facts) in plugins {
            validate_plugin_name(&display).map_err(TableError::InvalidName)?;
            let name = normalize_plugin_name(&display);
            if sorted.insert(name.clone(), (display, facts)).is_some() {
                return Err(TableError::DuplicateName(name));
            }
        }
        let additional = sorted
            .keys()
            .filter(|name| !self.names.contains_key(*name))
            .count();
        if self
            .entries
            .len()
            .checked_add(additional)
            .is_none_or(|n| n > u32::MAX as usize)
        {
            return Err(TableError::Exhausted);
        }
        let mut result = self.clone();
        for entry in &mut result.entries {
            entry.loaded = false;
        }
        for (name, (display, facts)) in sorted {
            if let Some(id) = result.names.get(&name) {
                if let Some(entry) = result.entries.get_mut((id.get() - 1) as usize) {
                    entry.loaded = true;
                    entry.facts = facts;
                }
            } else {
                let id = PluginId::new((result.entries.len() + 1) as u32)
                    .map_err(|_| TableError::Exhausted)?;
                result.names.insert(name.clone(), id);
                result.entries.push(PluginEntry {
                    id,
                    normalized_name: name,
                    display_name: display,
                    loaded: true,
                    facts,
                });
            }
        }
        Ok(result)
    }
    /// Lookup includes retained/not-loaded names and never allocates.
    pub fn id(&self, name: &str) -> Option<PluginId> {
        self.names.get(&normalize_plugin_name(name)).copied()
    }
    /// Inspect immutable metadata for this table's id.
    pub fn entry(&self, id: PluginId) -> Option<&PluginEntry> {
        self.entries.get((id.get() - 1) as usize)
    }
    /// Inspect deterministic allocation order, which is separate from load order.
    pub fn entries(&self) -> &[PluginEntry] {
        &self.entries
    }
    /// Intern portable identity, including retained names, without inventing missing owners.
    pub fn intern(&self, named: &NamedFormKey) -> Result<Option<FormKey>, TableError> {
        match named.parts() {
            None => Ok(None),
            Some((plugin, local)) => {
                let id = self
                    .id(plugin)
                    .ok_or_else(|| TableError::UnknownPlugin(plugin.to_owned()))?;
                FormKey::new(id, local)
                    .map(Some)
                    .map_err(TableError::InvalidIdentity)
            }
        }
    }
    /// Convert numeric identity to portable display name/local, validating table membership.
    pub fn named(&self, key: FormKey) -> Result<NamedFormKey, TableError> {
        let entry = self
            .entry(key.plugin())
            .ok_or(TableError::UnknownId(key.plugin()))?;
        NamedFormKey::new(&entry.display_name, key.local()).map_err(TableError::InvalidIdentity)
    }
    /// Detect light owners using the same extension-or-flag convention as today's converter.
    pub fn is_light(&self, id: PluginId) -> bool {
        self.entry(id)
            .is_some_and(|e| e.facts.flags.light || e.normalized_name.ends_with(".esl"))
    }
    /// Validate a supplied loaded order without guessing absent or duplicate entries.
    pub(crate) fn check_order(
        &self,
        order: &[PluginId],
    ) -> Result<(), crate::legacy_id_view::LegacyError> {
        let mut seen = HashSet::new();
        for &id in order {
            if !self.entry(id).is_some_and(|e| e.loaded) {
                return Err(crate::legacy_id_view::LegacyError::MissingPlugin(id));
            }
            if !seen.insert(id) {
                return Err(crate::legacy_id_view::LegacyError::DuplicatePlugin(id));
            }
        }
        Ok(())
    }
}
