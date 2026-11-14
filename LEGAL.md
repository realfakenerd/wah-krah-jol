# Legal Notices and Trademark Disclaimers

## 1. Independence and Non-Affiliation

**Mudcrab** is an independent, community-driven, open-source game engine reimplementation.

This project is **not** affiliated with, endorsed by, sponsored by, or in any way associated with:
- **Bethesda Softworks LLC**
- **ZeniMax Media Inc.**
- **Microsoft Corporation**
- Any of their respective subsidiaries, parent companies, or affiliates.

---

## 2. Trademarks & Nominative Fair Use

- *"The Elder Scrolls"*, *"Skyrim"*, *"Bethesda"*, *"Bethesda Game Studios"*, *"ZeniMax"*, *"Creation Engine"*, and related names, logos, emblems, and visual identities are registered trademarks or service marks of **Bethesda Softworks LLC** and/or **ZeniMax Media Inc.** in the United States and other countries.
- Any use of these trademarks within this repository, its source code, build scripts, tests, issue trackers, or documentation is strictly for **nominative fair use**—namely, to accurately describe technical compatibility, file format support, and data interoperability with legally purchased editions of *The Elder Scrolls V: Skyrim*.
- This project does **not** claim any ownership, right, or title to any trademarks, trade dress, or intellectual property owned by Bethesda Softworks LLC, ZeniMax Media Inc., or Microsoft Corporation.

---

## 3. Strict No-Asset Distribution Policy

- **No Proprietary Game Assets:** Mudcrab does **not** include, ship, host, or distribute any proprietary or copyrighted assets from Bethesda Softworks or ZeniMax Media. This includes, without limitation:
  - 3D models (`.nif`)
  - Textures and bitmaps (`.dds`)
  - Sound effects, music, and voice acting (`.xwm`, `.wav`, `.fuz`)
  - Master records, plugin databases, and dialogues (`.esm`, `.esp`, `.esl`)
  - Archive containers (`.bsa`, `.ba2`)
  - Interface movies and scripts (`.swf`, `.gfx`, `.pex`)
- **Procedural Fixtures Only:** Any asset files present in this repository (such as fixtures in `crates/dummy-content`) are procedurally generated synthetic placeholders created strictly for unit tests, benchmarking, and continuous integration. They contain zero proprietary content from commercial game releases.

---

## 4. Legally Acquired Game Files Required

- To run the game using Mudcrab with official content, users must supply their own legally purchased copy of *The Elder Scrolls V: Skyrim Special Edition* (via Steam, GOG, or another authorized vendor).
- The Mudcrab conversion tools inspect and process local files on the user's personal machine for personal gameplay and interoperability.

---

## 5. Software Interoperability

Mudcrab is an original, clean-room software engine reimplementation engineered in Rust. Research, specifications, and format parsers are developed for the express purpose of achieving software and file format interoperability under applicable laws (including 17 U.S.C. § 1201(f) in the United States and Directive 2009/24/EC on the legal protection of computer programs in the European Union).

---

## 6. Open-Source Licensing

Original code in this repository is licensed under:
- **GNU General Public License v3 or later** ([LICENSE](LICENSE))

Some files are derived from other GPL-compatible projects and keep those projects' terms; they are listed in [THIRD_PARTY.md](THIRD_PARTY.md). In particular, shaders derived from Community Shaders are **GPL-3.0-only**, so a build containing them is distributed under GPL-3.0 only.

This license applies **exclusively** to source code in this repository. It does not grant any rights to third-party assets, trademarks, or proprietary formats.
