//! Literal identity/legacy contracts. No fixtures depend on current load-order integer identity.
use shared::{
    form_key::{FormKey, IdentityError, Link, NamedFormKey, PluginId, StaticRecord},
    legacy_id_view::{LegacyError, LegacyIdView},
    plugin_table::{PluginFacts, PluginFlags, PluginTable, TableError},
    source_form_id::{Resolution, SourceFormId, ZeroPolicy, resolve},
};

/// Short explicit nonzero-id constructor for fixtures, not an expected-value oracle.
fn id(n: u32) -> PluginId {
    PluginId::new(n).unwrap()
}
/// Checked fixture key; assertions below use independent packed literals.
fn key(owner: u32, local: u32) -> FormKey {
    FormKey::new(id(owner), local).unwrap()
}
/// MAST order B, Light, A, then self Patch: no selector equals its table id.
fn table() -> PluginTable {
    PluginTable::fresh(vec![
        (
            "Patch.esp".into(),
            PluginFacts {
                masters: vec!["B.esm".into(), "Light.esl".into(), "A.esm".into()],
                header_version: Some(1.71),
                ..Default::default()
            },
        ),
        ("B.esm".into(), PluginFacts::default()),
        (
            "Light.esl".into(),
            PluginFacts {
                flags: PluginFlags {
                    light: true,
                    ..Default::default()
                },
                header_version: Some(1.71),
                ..Default::default()
            },
        ),
        ("A.esm".into(), PluginFacts::default()),
    ])
    .unwrap()
}
/// Resolve a raw Patch word without confusing its source selector with plugin-table ids.
fn source(t: &PluginTable, raw: u32, zero: ZeroPolicy) -> Resolution {
    resolve(
        t,
        SourceFormId {
            raw,
            containing_plugin: id(4),
        },
        zero,
    )
}

#[test]
/// Encoding fits signed SQLite, rejects reserved bits, and never aliases plugin/local-zero.
fn packed_boundaries_and_null() {
    assert_eq!(key(1, 0x012eb7).to_packed(), 0x0000_0000_0101_2eb7);
    assert_eq!(key(7, 0).to_packed(), 0x0000_0000_0700_0000);
    assert_eq!(key(u32::MAX, 0xffffff).to_sql_i64(), 72_057_594_037_927_935);
    assert_eq!(FormKey::from_packed(0), Err(IdentityError::ZeroPlugin));
    assert_eq!(FormKey::from_packed(1), Err(IdentityError::ZeroPlugin));
    assert_eq!(
        FormKey::from_packed(0x0100_0000_0000_0000),
        Err(IdentityError::ReservedBits)
    );
    assert_eq!(
        FormKey::new(id(1), 0x01000000),
        Err(IdentityError::LocalTooWide(0x01000000))
    );
    assert_eq!(PluginId::new(0), Err(IdentityError::ZeroPlugin));
    assert_eq!(FormKey::from_sql_i64(-1), Err(IdentityError::NegativeSql));
    assert_eq!(
        FormKey::from_sql_i64(72_057_594_037_927_936),
        Err(IdentityError::ReservedBits)
    );
    assert_eq!(
        FormKey::from_sql_i64(72_057_594_037_927_935),
        Ok(key(u32::MAX, 0xffffff))
    );
    assert_eq!(FormKey::from_nullable_packed(0), Ok(None));
    assert_eq!(
        FormKey::from_nullable_packed(1),
        Err(IdentityError::ZeroPlugin)
    );
    assert_eq!(FormKey::to_nullable_packed(Some(key(7, 0))), 0x0700_0000);
    assert_eq!(FormKey::to_nullable_packed(None), 0);
    assert_eq!(std::mem::size_of::<FormKey>(), 8);
    assert_eq!(std::mem::size_of::<Option<FormKey>>(), 8);
}

#[test]
/// Typed links carry no runtime tag and require no traits on their marker.
fn typed_links_are_zero_cost() {
    enum BareMarker {}
    let link = Link::<BareMarker>::new(key(7, 0x800));
    assert_eq!(link.key().to_packed(), 0x0700_0800);
    let copied = link;
    assert_eq!(link.key(), copied.key());
    assert_eq!(std::mem::size_of_val(&link), 8);
    assert_eq!(std::mem::size_of::<Option<Link<StaticRecord>>>(), 8);
    assert_eq!(link, copied);
    assert!(format!("{link:?}").contains("Link"));
    let mut links = std::collections::HashSet::new();
    assert!(links.insert(link));
    assert!(!links.insert(copied));
}

#[test]
/// Short/case-insensitive hex and spaced filenames round trip canonically.
fn portable_text_round_trips() {
    for (input, expected) in [
        ("012EB7:Skyrim.esm", "012EB7:Skyrim.esm"),
        ("800:My Mod.ESP", "000800:My Mod.ESP"),
        ("aBc:Café.esm", "000ABC:Café.esm"),
        ("000000:A.esm", "000000:A.esm"),
        ("0d800:A.esm", "00D800:A.esm"),
        ("0D8000:A.esm", "0D8000:A.esm"),
        ("null", "Null"),
    ] {
        let parsed = input.parse::<NamedFormKey>();
        assert!(parsed.is_ok(), "{input}: {parsed:?}");
        let parsed = parsed.unwrap();
        assert_eq!(parsed.to_string(), expected);
        assert_eq!(expected.parse::<NamedFormKey>(), Ok(parsed));
    }
    assert_eq!(
        "012eb7:SKYRIM.ESM".parse::<NamedFormKey>(),
        "012EB7:Skyrim.esm".parse()
    );
}

#[test]
/// Case-insensitive parsing resolves to the table's authored display spelling.
fn portable_text_uses_table_display_spelling() {
    let t = PluginTable::fresh(vec![("Skyrim.esm".into(), PluginFacts::default())]).unwrap();
    let named = "012eb7:skyrim.esm".parse().unwrap();
    let resolved = t.intern(&named).unwrap().unwrap();
    assert_eq!(resolved.to_packed(), 0x0101_2eb7);
    let emitted = t.named(resolved).unwrap();
    assert_eq!(emitted.to_string(), "012EB7:Skyrim.esm");
    assert_eq!(emitted, named);
    let mut identities = std::collections::HashSet::new();
    assert!(identities.insert(named));
    assert!(!identities.insert(emitted.clone()));
    assert_eq!(
        t.intern(&emitted.to_string().parse().unwrap()),
        Ok(Some(resolved))
    );
}

#[test]
/// A malformed master is local to its reference; fresh and retained tables keep usable entries.
fn bad_master_keeps_neighboring_plugins_resolvable() {
    let plugins = vec![
        ("A.esm".into(), PluginFacts::default()),
        (
            "Broken.esp".into(),
            PluginFacts {
                masters: vec!["Foo.esp ".into(), "A.esm".into()],
                ..Default::default()
            },
        ),
        (
            "Good.esp".into(),
            PluginFacts {
                masters: vec!["A.esm".into()],
                ..Default::default()
            },
        ),
    ];
    let prior = PluginTable::fresh(vec![("A.esm".into(), PluginFacts::default())]).unwrap();
    for result in [PluginTable::fresh(plugins.clone()), prior.retained(plugins)] {
        assert!(
            result.is_ok(),
            "one bad master must not reject the table: {result:?}"
        );
        let t = result.unwrap();
        assert_eq!(t.entries().len(), 3);
        let word = |supplier, raw| SourceFormId {
            containing_plugin: id(supplier),
            raw,
        };
        assert_eq!(
            resolve(&t, word(2, 0x800), ZeroPolicy::Null),
            Resolution::UnknownOwner("Foo.esp ".into())
        );
        assert_eq!(
            resolve(&t, word(2, 0x0100_0800), ZeroPolicy::Null),
            Resolution::Resolved(key(1, 0x800))
        );
        assert_eq!(
            resolve(&t, word(3, 0x800), ZeroPolicy::Null),
            Resolution::Resolved(key(1, 0x800))
        );
        assert_eq!(
            resolve(&t, word(3, 0x0100_0800), ZeroPolicy::Null),
            Resolution::Resolved(key(3, 0x800))
        );
        assert_eq!(
            resolve(&t, word(1, 0x800), ZeroPolicy::Null),
            Resolution::Resolved(key(1, 0x800))
        );
    }
}

#[test]
/// Decimal syntax/bare runtime words, overlong words, prefixes and paths cannot be guessed.
fn portable_text_rejects_ambiguous_syntax() {
    for input in [
        "1234567:A.esm",
        "0000800:A.esm",
        "123",
        "0002F4E3",
        "0x800:A.esm",
        "8.0:A.esm",
        "+800:A.esm",
        "800:",
        ":A.esm",
        "800:dir/A.esm",
        "800:dir\\A.esm",
        "800:A.esm:extra",
        "800:A.txt",
    ] {
        assert!(input.parse::<NamedFormKey>().is_err(), "accepted {input}");
    }
    assert_eq!(
        NamedFormKey::new("A.esm", 0x01000000),
        Err(IdentityError::LocalTooWide(0x01000000))
    );
    assert_eq!(
        NamedFormKey::new("dir/A.esm", 0x800),
        Err(IdentityError::InvalidPluginName)
    );
    assert_eq!(
        NamedFormKey::new("A.ESM", 0x800).unwrap().parts(),
        Some(("a.esm", 0x800))
    );
}

#[test]
/// Fresh allocation sorts names, retains metadata/display and refuses normalized duplicates.
fn fresh_table_is_deterministic_and_refuses_duplicates() {
    let t = table();
    assert_eq!(t.id("A.ESM"), Some(id(1)));
    assert_eq!(t.id("B.esm"), Some(id(2)));
    assert_eq!(t.id("Light.esl"), Some(id(3)));
    assert_eq!(t.id("Patch.esp"), Some(id(4)));
    assert_eq!(t.entry(id(4)).unwrap().facts.header_version, Some(1.71));
    assert_eq!(
        t.entry(id(4)).unwrap().facts.masters,
        ["B.esm", "Light.esl", "A.esm"]
    );
    assert_eq!(t.entry(id(3)).unwrap().display_name, "Light.esl");
    assert!(
        matches!(PluginTable::fresh(vec![("A.esm".into(), PluginFacts::default()), ("a.ESM".into(), PluginFacts::default())]), Err(TableError::DuplicateName(name)) if name == "a.esm")
    );
    assert!(matches!(
        PluginTable::fresh(vec![
            ("Café.esp".into(), PluginFacts::default()),
            ("CAFÉ.ESP".into(), PluginFacts::default())
        ]),
        Err(TableError::DuplicateName(_))
    ));
}

#[test]
/// Retained updates tombstone names, append sorted additions and re-add case-only names.
fn retained_allocation_is_an_api_only_capability() {
    let t = PluginTable::fresh(vec![
        ("B.esp".into(), PluginFacts::default()),
        ("A.esp".into(), PluginFacts::default()),
    ])
    .unwrap();
    let updated = t
        .retained(vec![
            (
                "B.esp".into(),
                PluginFacts {
                    content_hash: Some([9; 32]),
                    ..Default::default()
                },
            ),
            ("C.esp".into(), PluginFacts::default()),
        ])
        .unwrap();
    assert!(!updated.entry(id(1)).unwrap().loaded);
    assert_eq!(updated.id("B.esp"), Some(id(2)));
    assert_eq!(updated.id("C.esp"), Some(id(3)));
    assert_eq!(
        updated.entry(id(2)).unwrap().facts.content_hash,
        Some([9; 32])
    );
    let readded = updated
        .retained(vec![("a.ESP".into(), PluginFacts::default())])
        .unwrap();
    assert_eq!(readded.id("a.ESP"), Some(id(1)));
    assert!(readded.entry(id(1)).unwrap().loaded);
    assert_eq!(readded.entry(id(1)).unwrap().display_name, "A.esp");
    assert_eq!(
        readded.named(key(1, 0x800)).unwrap().to_string(),
        "000800:A.esp"
    );
    assert!(t.entry(id(1)).unwrap().loaded);
    let fresh = PluginTable::fresh(vec![("B.esp".into(), PluginFacts::default())]).unwrap();
    assert_eq!(fresh.id("B.esp"), Some(id(1))); // Numeric equality across tables means nothing.
    let named = "800:B.esp".parse().unwrap();
    assert_eq!(t.intern(&named), Ok(Some(key(2, 0x800))));
    assert_eq!(fresh.intern(&named), Ok(Some(key(1, 0x800))));
    assert_eq!(t.named(key(2, 0x800)), Ok(named));
    assert_eq!(t.intern(&NamedFormKey::Null), Ok(None));
    assert!(matches!(
        t.intern(&"800:Missing.esp".parse().unwrap()),
        Err(TableError::UnknownPlugin(_))
    ));
}

#[test]
/// MAST selectors are neither table indices nor runtime slots; equal locals retain owners.
fn resolve_nonidentity_masters_and_overrides() {
    let t = table();
    assert_eq!(
        source(&t, 0x0000_0800, ZeroPolicy::Null)
            .key()
            .map(FormKey::to_packed),
        Some(0x0200_0800)
    );
    assert_eq!(
        source(&t, 0x0200_0800, ZeroPolicy::Null)
            .key()
            .map(FormKey::to_packed),
        Some(0x0100_0800)
    );
    assert_eq!(
        source(&t, 0x0100_0fff, ZeroPolicy::Null),
        Resolution::Resolved(key(3, 0xfff))
    );
    assert_eq!(
        source(&t, 0x0300_0800, ZeroPolicy::Null)
            .key()
            .map(FormKey::to_packed),
        Some(0x0400_0800)
    );
    assert_eq!(
        source(&t, 0x02ff_ffff, ZeroPolicy::Null)
            .key()
            .map(FormKey::to_packed),
        Some(0x01ff_ffff)
    );
}

#[test]
/// Raw null has context; zero local identities and self projection remain distinguishable.
fn resolve_null_zero_and_out_of_range() {
    let t = table();
    assert_eq!(
        source(&t, 0, ZeroPolicy::Identity)
            .key()
            .map(FormKey::to_packed),
        Some(0x0200_0000)
    );
    assert_eq!(source(&t, 0, ZeroPolicy::Null), Resolution::Null);
    assert_eq!(
        source(&t, 0x0500_0123, ZeroPolicy::Null),
        Resolution::SelfProjected {
            key: key(4, 0x123),
            index: 5
        }
    );
    assert_eq!(
        source(&t, 0xffff_ffff, ZeroPolicy::Null),
        Resolution::SelfProjected {
            key: key(4, 0xffffff),
            index: 255
        }
    );
}

#[test]
/// Missing suppliers/masters are named outcomes, preserving the original MAST positions.
fn resolve_unknown_owners_without_panicking() {
    let t = table();
    assert_eq!(
        resolve(
            &t,
            SourceFormId {
                containing_plugin: id(99),
                raw: 0x800
            },
            ZeroPolicy::Null
        ),
        Resolution::UnknownSupplier(id(99))
    );
    let t = PluginTable::fresh(vec![(
        "Patch.esp".into(),
        PluginFacts {
            masters: vec!["Missing.esm".into()],
            ..Default::default()
        },
    )])
    .unwrap();
    assert_eq!(
        resolve(
            &t,
            SourceFormId {
                containing_plugin: id(1),
                raw: 0x800
            },
            ZeroPolicy::Null
        ),
        Resolution::UnknownOwner("Missing.esm".into())
    );
}

#[test]
/// Light validation uses the owner's width/version and the supplier's extended-range support.
fn light_boundaries_and_retained_header_versions() {
    let t = table();
    assert_eq!(
        source(&t, 0x0100_1000, ZeroPolicy::Null),
        Resolution::InvalidWidth {
            owner: id(3),
            local: 0x1000,
            maximum: 0xfff
        }
    );
    for version in [Some(1.70), Some(1.71), None, Some(f32::NAN)] {
        let t = PluginTable::fresh(vec![(
            "Light.esl".into(),
            PluginFacts {
                header_version: version,
                ..Default::default()
            },
        )])
        .unwrap();
        for local in [0, 1, 0x7ff, 0x800, 0xfff, 0x1000] {
            let actual = resolve(
                &t,
                SourceFormId {
                    containing_plugin: id(1),
                    raw: local,
                },
                ZeroPolicy::Identity,
            );
            let expected = if local > 0xfff {
                Resolution::InvalidWidth {
                    owner: id(1),
                    local,
                    maximum: 0xfff,
                }
            } else if local < 0x800 && version != Some(1.71) {
                Resolution::IncompatibleHeaderVersion {
                    owner: id(1),
                    supplier: id(1),
                    local,
                    owner_version: version,
                    supplier_version: version,
                }
            } else {
                Resolution::Resolved(key(1, local))
            };
            if local < 0x800 && version.is_some_and(f32::is_nan) {
                let Resolution::IncompatibleHeaderVersion {
                    owner,
                    supplier,
                    local: rejected,
                    owner_version,
                    supplier_version,
                } = actual
                else {
                    panic!("expected version diagnostic, got {actual:?}")
                };
                assert_eq!((owner, supplier, rejected), (id(1), id(1), local));
                assert_eq!(owner_version.map(f32::to_bits), version.map(f32::to_bits));
                assert_eq!(
                    supplier_version.map(f32::to_bits),
                    version.map(f32::to_bits)
                );
            } else {
                assert_eq!(actual, expected, "version={version:?}, local={local:X}");
            }
        }
    }
    let t = PluginTable::fresh(vec![
        (
            "Light.esl".into(),
            PluginFacts {
                header_version: Some(1.71),
                ..Default::default()
            },
        ),
        (
            "Patch.esp".into(),
            PluginFacts {
                masters: vec!["Light.esl".into()],
                header_version: Some(1.70),
                ..Default::default()
            },
        ),
    ])
    .unwrap();
    assert_eq!(
        resolve(
            &t,
            SourceFormId {
                containing_plugin: id(2),
                raw: 0x7ff
            },
            ZeroPolicy::Null
        ),
        Resolution::IncompatibleHeaderVersion {
            owner: id(1),
            supplier: id(2),
            local: 0x7ff,
            owner_version: Some(1.71),
            supplier_version: Some(1.70),
        }
    );
    // A light supplier overriding a full master keeps all 24 local bits.
    let t = PluginTable::fresh(vec![
        ("A.esm".into(), PluginFacts::default()),
        (
            "Light.esl".into(),
            PluginFacts {
                masters: vec!["A.esm".into()],
                ..Default::default()
            },
        ),
    ])
    .unwrap();
    assert_eq!(
        resolve(
            &t,
            SourceFormId {
                containing_plugin: id(2),
                raw: 0x00ff_ffff
            },
            ZeroPolicy::Null
        ),
        Resolution::Resolved(key(1, 0xffffff))
    );
}

#[test]
/// Both legacy directions are exact, while absent order entries never acquire guessed slots.
fn legacy_two_way_and_missing_slots() {
    let t = table();
    let view = LegacyIdView::from_load_order(&t, &[id(2), id(4), id(3), id(1)]).unwrap();
    for (packed, runtime) in [
        (0x0202_f4e3, 0x0002_f4e3),
        (0x0100_0800, 0x0200_0800),
        (0x0300_0800, 0xfe00_0800),
        (0x0400_0800, 0x0100_0800),
        (0x02ff_ffff, 0x00ff_ffff),
    ] {
        let k = FormKey::from_packed(packed).unwrap();
        assert_eq!(view.to_legacy_u32(k), Ok(runtime));
        assert_eq!(view.from_legacy_u32(runtime), Ok(Some(k)));
        assert_eq!(
            view.from_legacy_u32(view.to_legacy_u32(k).unwrap()),
            Ok(Some(k))
        );
        assert_eq!(
            view.to_legacy_u32(view.from_legacy_u32(runtime).unwrap().unwrap()),
            Ok(runtime)
        );
    }
    assert_eq!(view.parse_legacy("0002F4E3"), Ok(Some(key(2, 0x2f4e3))));
    assert_eq!(view.from_legacy_u32(0), Ok(None));
    assert_eq!(
        view.to_nullable_legacy_u32(view.from_legacy_u32(0).unwrap()),
        Ok(0)
    );
    assert_eq!(
        view.to_legacy_u32(key(2, 0)),
        Err(LegacyError::NullCollision)
    );
    assert_eq!(
        view.to_legacy_u32(key(3, 0x1000)),
        Err(LegacyError::LightLocalTooWide(0x1000))
    );
    let missing = LegacyIdView::from_load_order(&t, &[id(2)]).unwrap();
    assert_eq!(
        missing.to_legacy_u32(key(4, 0x800)),
        Err(LegacyError::MissingPlugin(id(4)))
    );
    assert_eq!(
        missing.from_legacy_u32(0x0100_0800),
        Err(LegacyError::UnknownFullSlot(1))
    );
    assert_eq!(
        missing.from_legacy_u32(0xfe01_2800),
        Err(LegacyError::UnknownLightSlot(0x12))
    );
    assert_eq!(
        view.from_legacy_u32(0xff00_0800),
        Err(LegacyError::ReservedSlot)
    );
    assert_eq!(view.parse_legacy("800"), Err(LegacyError::InvalidText));
    assert!(matches!(
        LegacyIdView::from_load_order(&t, &[id(1), id(1)]),
        Err(LegacyError::DuplicatePlugin(_))
    ));
}

#[test]
/// Reordering changes only the view, while literal owner/local keys stay fixed.
fn reorder_changes_legacy_only() {
    let t = table();
    let a = LegacyIdView::from_load_order(&t, &[id(1), id(2), id(3), id(4)]).unwrap();
    let b = LegacyIdView::from_load_order(&t, &[id(2), id(1), id(4), id(3)]).unwrap();
    assert_eq!(a.to_legacy_u32(key(1, 0x800)), Ok(0x0000_0800));
    assert_eq!(b.to_legacy_u32(key(1, 0x800)), Ok(0x0100_0800));
    assert_eq!(
        b.from_legacy_u32(0x0100_0800)
            .unwrap()
            .map(FormKey::to_packed),
        Some(0x0100_0800)
    );
}

#[test]
/// Aggregate tables exceed Skyrim limits without inventing legacy slots; both endpoints work.
fn capacity_and_fe012800_round_trip() {
    let mut entries = Vec::new();
    for n in 0..256 {
        entries.push((format!("Full{n:04}.esp"), PluginFacts::default()));
    }
    for n in 0..4097 {
        entries.push((format!("Light{n:04}.esl"), PluginFacts::default()));
    }
    let t = PluginTable::fresh(entries).unwrap();
    let order: Vec<_> = t.entries().iter().map(|e| e.id).collect();
    let view = LegacyIdView::from_load_order(&t, &order).unwrap();
    assert_eq!(key(256, 0x800).to_packed(), 0x0000_0001_0000_0800);
    assert_eq!(key(4097, 0x800).to_packed(), 0x0000_0010_0100_0800);
    assert_eq!(view.to_legacy_u32(key(254, 0xffffff)), Ok(0xfdff_ffff));
    assert_eq!(
        view.to_legacy_u32(key(255, 0x800)),
        Err(LegacyError::NoFullSlot(id(255)))
    );
    assert_eq!(view.to_legacy_u32(key(4352, 0xfff)), Ok(0xfeff_ffff));
    assert_eq!(
        view.to_legacy_u32(key(4353, 0x800)),
        Err(LegacyError::NoLightSlot(id(4353)))
    );
    assert_eq!(view.parse_legacy("FE012800"), Ok(Some(key(275, 0x800))));
    assert_eq!(view.to_legacy_u32(key(275, 0x800)), Ok(0xfe01_2800));
    // Every admitted slot, with independent arithmetic expectations and width endpoints.
    for slot in 0..254u32 {
        for local in [0, 1, 0x800, 0xffffff] {
            let raw = slot << 24 | local;
            if raw == 0 {
                continue;
            }
            assert_eq!(view.from_legacy_u32(raw), Ok(Some(key(slot + 1, local))));
            assert_eq!(view.to_legacy_u32(key(slot + 1, local)), Ok(raw));
        }
    }
    for slot in 0..4096u32 {
        for local in [0, 1, 0x800, 0xfff] {
            let raw = 0xfe00_0000 | slot << 12 | local;
            assert_eq!(view.from_legacy_u32(raw), Ok(Some(key(slot + 257, local))));
            assert_eq!(view.to_legacy_u32(key(slot + 257, local)), Ok(raw));
        }
    }
}
