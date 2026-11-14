//! Pack-scoped owner/local identities. Numeric keys must never cross table boundaries.
//!
//! The name and plugin + local-ID design follow Mutagen's `FormKey`:
//! <https://github.com/Mutagen-Modding/Mutagen>. Credit to the Mutagen project
//! for the concept; no Mutagen code is copied here.
use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    num::NonZeroU64,
    str::FromStr,
};

/// Invalid identity encoding, never a whole-conversion error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    ZeroPlugin,
    LocalTooWide(u32),
    ReservedBits,
    NegativeSql,
    InvalidText,
    InvalidPluginName,
}

impl fmt::Display for IdentityError {
    /// Describe the rejected encoding without silently masking it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidText => f.write_str("expected 1-6 hexadecimal digits followed by :Plugin.esm/.esp/.esl, or Null; bare numbers, decimal syntax and overlong IDs are not accepted"),
            Self::InvalidPluginName => f.write_str("expected a plugin filename with an esm/esp/esl extension, without paths or control characters"),
            _ => write!(f, "invalid form identity: {self:?}"),
        }
    }
}
impl std::error::Error for IdentityError {}

/// Nonzero index meaningful only within its defining plugin table, never load priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PluginId(u32);
impl PluginId {
    /// Reject the reserved zero index.
    pub fn new(value: u32) -> Result<Self, IdentityError> {
        if value == 0 {
            Err(IdentityError::ZeroPlugin)
        } else {
            Ok(Self(value))
        }
    }
    /// Explicitly expose the table-local index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Validated 56-bit key: plugin in bits 24..55, local in bits 0..23.
/// `Option<FormKey>` is the explicit null representation and occupies eight bytes.
/// A non-null owner/local-zero key is valid; only nullable packed zero means null.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FormKey(NonZeroU64);
impl FormKey {
    /// Construct an identity without truncating an over-wide local.
    pub fn new(plugin: PluginId, local: u32) -> Result<Self, IdentityError> {
        if local > 0x00ff_ffff {
            return Err(IdentityError::LocalTooWide(local));
        }
        Self::from_packed((u64::from(plugin.get()) << 24) | u64::from(local))
    }
    /// Validate a non-null numeric encoding; no implicit integer conversions exist.
    pub fn from_packed(value: u64) -> Result<Self, IdentityError> {
        if value >> 56 != 0 {
            return Err(IdentityError::ReservedBits);
        }
        if value >> 24 == 0 {
            return Err(IdentityError::ZeroPlugin);
        }
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(IdentityError::ZeroPlugin)
    }
    /// Explicit internal numeric representation, never a portable external identity.
    pub const fn to_packed(self) -> u64 {
        self.0.get()
    }
    /// Owning table entry, independent of the containing source plugin.
    pub fn plugin(self) -> PluginId {
        // from_packed guarantees this private construction is nonzero and fits u32.
        PluginId((self.0.get() >> 24) as u32)
    }
    /// Full 24-bit local; light-source restrictions are the resolver's responsibility.
    pub const fn local(self) -> u32 {
        (self.0.get() & 0x00ff_ffff) as u32
    }
    /// Signed SQLite representation; the reserved high byte guarantees a positive value.
    pub const fn to_sql_i64(self) -> i64 {
        self.0.get() as i64
    }
    /// Validate a non-null SQLite integer.
    pub fn from_sql_i64(value: i64) -> Result<Self, IdentityError> {
        if value < 0 {
            return Err(IdentityError::NegativeSql);
        }
        Self::from_packed(value as u64)
    }
    /// Decode a nullable integer: zero alone is null, plugin-zero locals are invalid.
    pub fn from_nullable_packed(value: u64) -> Result<Option<Self>, IdentityError> {
        if value == 0 {
            Ok(None)
        } else {
            Self::from_packed(value).map(Some)
        }
    }
    /// Encode explicit null as zero without conflating owner/local zero.
    pub fn to_nullable_packed(value: Option<Self>) -> u64 {
        value.map_or(0, Self::to_packed)
    }
}

/// Validate a filename, allowing interior spaces and Unicode, never a filesystem path.
pub(crate) fn validate_plugin_name(name: &str) -> Result<(), IdentityError> {
    let lower = name.to_lowercase();
    if name.trim() != name
        || name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        || ![".esm", ".esp", ".esl"]
            .into_iter()
            .any(|ext| lower.strip_suffix(ext).is_some_and(|stem| !stem.is_empty()))
    {
        return Err(IdentityError::InvalidPluginName);
    }
    Ok(())
}

/// Portable identity at cross-pack/script/tool boundaries. `Null` is the sole null token.
/// The owner-name/local concept follows Mutagen; this is an independent implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NamedFormKey {
    Null,
    Key(NamedIdentity),
}
/// Validated portable payload; only checked NamedFormKey construction can create it.
#[derive(Debug, Clone)]
pub struct NamedIdentity {
    plugin: String,
    display_plugin: String,
    local: u32,
}
impl PartialEq for NamedIdentity {
    /// Display spelling does not affect portable identity equality.
    fn eq(&self, other: &Self) -> bool {
        self.plugin == other.plugin && self.local == other.local
    }
}
impl Eq for NamedIdentity {}
impl Hash for NamedIdentity {
    /// Hash the normalized owner and local consistently with equality.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.plugin.hash(state);
        self.local.hash(state);
    }
}
impl NamedFormKey {
    /// Validate a portable name/local pair and normalize name equality as the converter does.
    pub fn new(plugin: &str, local: u32) -> Result<Self, IdentityError> {
        validate_plugin_name(plugin)?;
        if local > 0x00ff_ffff {
            return Err(IdentityError::LocalTooWide(local));
        }
        Ok(Self::Key(NamedIdentity {
            plugin: crate::plugin_selection::normalize_plugin_name(plugin),
            display_plugin: plugin.to_owned(),
            local,
        }))
    }
    /// Inspect the normalized owner/local without permitting unchecked payload construction.
    pub fn parts(&self) -> Option<(&str, u32)> {
        match self {
            Self::Null => None,
            Self::Key(value) => Some((&value.plugin, value.local)),
        }
    }
}
impl FromStr for NamedFormKey {
    type Err = IdentityError;
    /// Read 1..6 hexadecimal digits followed by a colon and plugin filename.
    /// Digits-only locals are hexadecimal too; bare decimal/runtime IDs are rejected.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("Null") {
            return Ok(Self::Null);
        }
        let (local, plugin) = text.split_once(':').ok_or(IdentityError::InvalidText)?;
        if local.is_empty() || local.len() > 6 || !local.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(IdentityError::InvalidText);
        }
        let local = u32::from_str_radix(local, 16).map_err(|_| IdentityError::InvalidText)?;
        Self::new(plugin, local)
    }
}
impl fmt::Display for NamedFormKey {
    /// Emit six-digit uppercase hexadecimal and the stored display spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Key(value) => write!(f, "{:06X}:{}", value.local, value.display_plugin),
        }
    }
}

/// Zero-cost typed identity for later projection steps; it does not prove target existence/type.
#[repr(transparent)]
pub struct Link<T> {
    key: FormKey,
    marker: PhantomData<fn() -> T>,
}
impl<T> fmt::Debug for Link<T> {
    /// Describe the key without imposing Debug on the record marker.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link").field("key", &self.key).finish()
    }
}
impl<T> PartialEq for Link<T> {
    /// Compare keys without imposing equality on the record marker.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<T> Eq for Link<T> {}
impl<T> Hash for Link<T> {
    /// Hash the key without imposing Hash on the record marker.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}
impl<T> Copy for Link<T> {}
impl<T> Clone for Link<T> {
    /// Copy without requiring marker types to implement Clone.
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Link<T> {
    /// Attach a compile-time record kind for later steps, with no runtime allocation.
    pub const fn new(key: FormKey) -> Self {
        Self {
            key,
            marker: PhantomData,
        }
    }
    /// Retrieve the validated identity for contextual resolution.
    pub const fn key(self) -> FormKey {
        self.key
    }
}
/// Static record marker for later projection steps.
pub enum StaticRecord {}
/// Placed-reference marker for later projection steps.
pub enum ReferenceRecord {}
