//! Checked two-way view of today's full/FE runtime packing for one load-order snapshot.
use crate::{
    form_key::{FormKey, PluginId},
    plugin_table::PluginTable,
};
use std::{collections::HashMap, fmt};

/// Unrepresentable/missing identities are reported explicitly; never invent stand-in IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyError {
    MissingPlugin(PluginId),
    DuplicatePlugin(PluginId),
    NoFullSlot(PluginId),
    NoLightSlot(PluginId),
    LightLocalTooWide(u32),
    UnknownFullSlot(u8),
    UnknownLightSlot(u16),
    ReservedSlot,
    /// Full slot zero/local zero encodes null, so a non-null key cannot round trip there.
    NullCollision,
    InvalidText,
}
impl fmt::Display for LegacyError {
    /// Preserve the exact category for callers' diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::InvalidText {
            f.write_str("expected exactly eight hexadecimal runtime FormID digits")
        } else {
            write!(f, "legacy FormID unavailable: {self:?}")
        }
    }
}
impl std::error::Error for LegacyError {}
#[derive(Debug, Clone, Copy)]
enum Slot {
    Full(usize),
    Light(usize),
}
/// A table plus an explicit current order; it neither selects nor reorders plugins.
/// Official/CCC order is supplied by the existing selection authority. Never mix tables.
pub struct LegacyIdView<'a> {
    table: &'a PluginTable,
    slots: HashMap<PluginId, Slot>,
    full: Vec<PluginId>,
    light: Vec<PluginId>,
}
impl<'a> LegacyIdView<'a> {
    /// Build independent full/light slot sequences. Overcapacity owners remain identifiable,
    /// but conversion to/from a legacy slot is available only within 254/4096 entries.
    pub fn from_load_order(
        table: &'a PluginTable,
        order: &[PluginId],
    ) -> Result<Self, LegacyError> {
        table.check_order(order)?;
        let mut view = Self {
            table,
            slots: HashMap::new(),
            full: Vec::new(),
            light: Vec::new(),
        };
        for &id in order {
            let slot = if table.is_light(id) {
                let slot = Slot::Light(view.light.len());
                view.light.push(id);
                slot
            } else {
                let slot = Slot::Full(view.full.len());
                view.full.push(id);
                slot
            };
            view.slots.insert(id, slot);
        }
        Ok(view)
    }
    /// Pack a non-null key exactly as Skyrim does when the owner and local are representable.
    pub fn to_legacy_u32(&self, key: FormKey) -> Result<u32, LegacyError> {
        if !self.table.entry(key.plugin()).is_some_and(|e| e.loaded) {
            return Err(LegacyError::MissingPlugin(key.plugin()));
        }
        match self
            .slots
            .get(&key.plugin())
            .ok_or(LegacyError::MissingPlugin(key.plugin()))?
        {
            Slot::Full(slot) if *slot >= 254 => Err(LegacyError::NoFullSlot(key.plugin())),
            Slot::Light(slot) if *slot >= 4096 => Err(LegacyError::NoLightSlot(key.plugin())),
            Slot::Light(_) if key.local() > 0xfff => {
                Err(LegacyError::LightLocalTooWide(key.local()))
            }
            Slot::Full(0) if key.local() == 0 => Err(LegacyError::NullCollision),
            Slot::Full(slot) => Ok((*slot as u32) << 24 | key.local()),
            Slot::Light(slot) => Ok(0xfe00_0000 | (*slot as u32) << 12 | key.local()),
        }
    }
    /// Preserve nullable legacy words explicitly, including exact zero round trips.
    pub fn to_nullable_legacy_u32(&self, key: Option<FormKey>) -> Result<u32, LegacyError> {
        key.map_or(Ok(0), |key| self.to_legacy_u32(key))
    }
    /// Decode a runtime ID using these slots; zero is explicitly null, absent slots error.
    pub fn from_legacy_u32(&self, value: u32) -> Result<Option<FormKey>, LegacyError> {
        if value == 0 {
            return Ok(None);
        }
        let (owner, local) = match value >> 24 {
            0xff => return Err(LegacyError::ReservedSlot),
            0xfe => {
                let slot = ((value >> 12) & 0xfff) as u16;
                (
                    *self
                        .light
                        .get(usize::from(slot))
                        .ok_or(LegacyError::UnknownLightSlot(slot))?,
                    value & 0xfff,
                )
            }
            slot => (
                *self
                    .full
                    .get(slot as usize)
                    .ok_or(LegacyError::UnknownFullSlot(slot as u8))?,
                value & 0x00ff_ffff,
            ),
        };
        FormKey::new(owner, local)
            .map(Some)
            .map_err(|_| LegacyError::ReservedSlot)
    }
    /// Parse exactly eight hexadecimal runtime digits (including FE light IDs), never decimal.
    pub fn parse_legacy(&self, text: &str) -> Result<Option<FormKey>, LegacyError> {
        if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(LegacyError::InvalidText);
        }
        let value = u32::from_str_radix(text, 16).map_err(|_| LegacyError::InvalidText)?;
        self.from_legacy_u32(value)
    }
}
