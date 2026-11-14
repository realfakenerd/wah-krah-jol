# FormKey identity contract (step 1)

Part of [#542](https://github.com/Mudcrab-Team/mudcrab/issues/542).
Step 1 adds shared types beside the existing APIs. **No output, format or schema
change.** Every existing u32 type, field and function stays unchanged. The new
resolver and legacy view are exercised by additive converter tests, without
changing production conversion or runtime behavior. This step does not remove
plugin admission limits.

## Layout and table scope

`FormKey` is an opaque validated eight-byte value backed by `NonZeroU64`:

| Bits | Meaning |
| --- | --- |
| 0–23 | Local record ID, 0 through FFFFFF |
| 24–55 | Nonzero `PluginId(u32)` |
| 56–63 | Reserved, must be zero |

The largest key is `0x00FF_FFFF_FFFF_FFFF` (72,057,594,037,927,935), which fits
SQLite's signed i64. Checked constructors reject invalid values without masking.
There are no implicit conversions to/from u32, and no numeric serde codec.
`Option<FormKey>` is the explicit null representation, with the eight-byte
nonzero niche. Nullable numeric helpers map only all-zero to null. Plugin zero
with a nonzero local is invalid. Owner/local zero is a non-null identity.

**Numeric keys only have meaning inside one defining `PluginTable`.** Equal
integers from different tables do not establish equal identity. A table is not
the load order and gives no winner priority. Portable boundaries use plugin
names plus local IDs. The name and plugin + local-ID design follow Mutagen's
[`FormKey`](https://github.com/Mutagen-Modding/Mutagen); credit to the Mutagen
project for the concept. No Mutagen code is copied here.

Fresh allocation sorts Unicode-lowercase normalized unique names and assigns
consecutive indices starting at one. Normalization uses the existing shared
plugin-selection helper. Duplicate normalized input names are refused before
mutation. Display spelling is separate from identity. Immutable entries retain
master/light/medium flags, original HEDR version, ordered MAST list and an optional
content hash. Hashes serve caches only. ESL extension or light flag classifies
a light owner, matching current selection. Medium flags are retained metadata;
Skyrim medium-slot packing is not introduced.

Malformed MAST strings are retained verbatim rather than refusing the table.
References selecting them report `UnknownOwner` with the original name; other
master positions and usable plugins remain resolvable.

The pure retained-allocation API clones a table, marks missing names not loaded,
preserves old indices/display spelling, updates supplied facts, and appends new
names in sorted order. Case-only re-additions recover the prior index. Read-only
interning can identify a retained missing name; resolution and legacy projection
require a loaded owner. This API has **no persistence** or cross-conversion
lineage implementation. Fresh allocation starts a new scope; numeric retention
is a possible cache optimization, never a cross-pack guarantee.

`Link<T>` is a zero-cost typed wrapper for later projection steps. Static and
placed-reference markers demonstrate the intended use. Its copy, equality,
hashing and debug implementations impose no traits on the marker. A marker does not prove
that a target exists or has the expected runtime record type.

## Text form

`NamedFormKey` parses `012EB7:Skyrim.esm`, `800:My Mod.esp`, and `Null`.
Locals contain 1–6 ASCII hex digits, case-insensitive, with no prefix. Emission
always uses six uppercase hex digits and the stored display spelling of the name.
Parsing preserves the input spelling; equality, hashing and table lookup use the
normalized name. Resolving `012eb7:skyrim.esm` through a table containing
`Skyrim.esm`, then converting back to text, emits `012EB7:Skyrim.esm`.
Interior filename spaces and Unicode are accepted, paths/control characters and non-plugin extensions are
rejected. Surrounding text whitespace is trimmed. The explicit null token is
`Null`, case-insensitive on input; `000000:Owner.esm` is not null.

Digits-only locals are **hexadecimal**, including `800` and `000800`. Rejecting
them as “decimal-looking” would contradict the grammar. Bare decimal/runtime
words, decimal signs/points, `0x` prefixes, empty locals/names,
and overlong locals (even with leading zeroes) are rejected without radix guesses.
The separate legacy text parser accepts exactly eight hexadecimal runtime digits.

## Source resolution

`SourceFormId` contains a raw native u32 plus its containing plugin. It is neither
a `FormKey` nor a runtime ID. The containing plugin's original MAST sequence maps
the source high byte to the owner: less than MAST count selects that master;
equal selects self. It never uses global slots for this interpretation.

| Outcome | Meaning |
| --- | --- |
| `Resolved` | Known loaded owner with valid local/context |
| `Null` | Raw zero under explicit `ZeroPolicy::Null` |
| `SelfProjected` | High byte exceeds MAST count; validated supplier identity used as recovery |
| `UnknownSupplier` | Containing plugin absent/not loaded |
| `UnknownOwner` | Selected MAST name absent/not loaded; original indices retained |
| `InvalidWidth` | Light local exceeds FFF; full source locals inherently fit 24 bits |
| `IncompatibleHeaderVersion` | Includes owner, supplier, local and both optional HEDR versions for a rejected extended light reference |

There is exactly one out-of-range-index policy: `SelfProjected`, matching the
existing permissive runtime header remapper. It is **not proof of original
ownership**. Current strict optional links and provenance paths use other
policies; they remain untouched. There is no special FFFFFFFF “None” sentinel
in this contract: without a future field-specific policy it follows that same
out-of-range rule.

Light-owned locals must fit 12 bits. Below 800, both the owner and referring
supplier must have a finite retained HEDR version at least 1.71. Old/missing/
nonfinite versions cannot authorize extended locals; 800–FFF remains usable.
The owner determines width, so a light supplier overriding a full master's
FFFFFF local keeps the full value. Identity-context raw zero remains distinct
from reference-context null. HEDR is metadata supplied to this contract; existing
readers are not changed to retain it in step 1.

Range evidence: [esplugin's Skyrim SE validator](https://github.com/Ortham/esplugin/blob/f964f3895747362ea6ce4cbbea56f2a746c39319/src/plugin.rs)
accepts 800–FFF before HEDR 1.71 and 000–FFF afterward; its new-record check
excludes overrides. [xEdit's extended-range notes](https://github.com/TES5Edit/TES5Edit/blob/9914c63555d9b058fc1b1c2ae7d5840afc390064/whatsnew.md)
describe referring-module version compatibility. This is a per-reference
validation contract, not a blanket rejection of an entire dependency graph.

Resolution never panics or aborts a conversion. Later consumers must skip or
clear each bad record/field, aggregate a warning and preserve valid neighbors.
This pure module returns named outcomes; it has no logger or conversion-run
error path. Malformed wide light locals are rejected rather than truncated;
differential equivalence is claimed only for the shared valid domain, with
out-of-range projection checked against the permissive header path explicitly.

## Legacy view and limits

`LegacyIdView` takes a table and an explicit current load-order sequence. Full
and light counters advance independently, using today's full `(slot << 24) |
local` and light `FE000000 | (slot << 12) | local` packing. It does not select or
reorder inputs. The existing selection authority supplies installed official
masters in slots 00–04 and installed Creations in `Skyrim.ccc` order. CC runtime
IDs depend on the same installed set and full/light classification.

The view supports both checked directions and eight-digit runtime text parsing.
Missing owners/slots, duplicate order entries, reserved FF slots, over-wide
light locals and unrepresentable owners have distinct errors. Full owners beyond
254 and light owners beyond 4,096 have no representable legacy slot. No masking,
slot reuse or stand-in allocation occurs. Zero runtime ID decodes to null;
full-slot-zero/local-zero yields `NullCollision` in the forward non-null API,
because that encoding is indistinguishable from null.

## Backwards compatibility

Within Skyrim's 254 full / 4,096 light limits, representable non-null IDs round
trip exactly in both directions for any supplied order. Official slots 00–04,
nonalphabetical CCC order, light plugins and nonidentity MAST order have literal
and differential coverage. Missing plugins produce explicit errors, never a
guess based on another order. A key stays owner/local-based when legacy slots
change. Raw zero remains the explicit nullable legacy value.

Existing packs remain readable by the unchanged readers. Later format-changing
steps must pair writers/readers and either retain support or issue an explicit
“reconvert” message. There is no format bump or reconvert requirement in step 1.

Future script integration must make `GetFormID` and `GetFormFromFile` match
Skyrim within the limits. `GetFormID` uses the current legacy slot view;
`GetFormFromFile` uses the supplied plugin name plus local ID, rather than
mistaking that local for a MAST-relative source word. No script functions or
commands change here. Future saves
must carry names plus local IDs (or a table remapped by name), never bare
pack-scoped numeric identities; no save format is implemented here.

**Beyond-limit interoperability defaults to a clear error from the checked
legacy adapter**, as adopted in #542's refinements. Step 1 implements those
representability errors; script interfaces are deferred. An opt-in compatibility
mode with stand-in IDs may be considered later, but is not decided and is not
implemented. Stand-ins cannot promise Skyrim's top-byte owner semantics and
would need collision/lifetime rules. The default does not invent synthetic IDs.

## Non-goals

No file format/schema/cache version bump, writer/reader change, admission-cap
removal, command system, runtime migration, save support, persistent plugin
table, or new emitted pack data. Every existing u32 type, field and function is
unchanged. Tests do not certify all pre-existing Skyrim parity or implement the
later stages of #542.
