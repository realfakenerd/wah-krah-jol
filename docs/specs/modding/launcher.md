# Mudcrab Launcher & Setup Workflow

This document details the user journey, automatic game directory detection, asset transformation pipeline trigger, and game launch process.

---

## 1. User Experience & Workflow Diagram

```
                 ┌───────────────────────────────┐
                 │    Gamer Runs Mudcrab         │
                 │          Launcher             │
                 └───────────────┬───────────────┘
                                 │
                                 ▼
                 ┌───────────────────────────────┐
                 │  Auto-Detect Skyrim Folder    │
                 │ (Steam, GOG, Custom Registry) │
                 └───────────────┬───────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │ Found game directory?   │
                    └─────┬─────────────┬─────┘
                     YES  │             │  NO
                          │             ▼
                          │   ┌───────────────────┐
                          │   │ File Picker Dialog│
                          │   │ "Where is Skyrim?"│
                          │   └─────────┬─────────┘
                          │             │
                          └───────┬─────┘
                                  │
                                  ▼
                 ┌───────────────────────────────┐
                 │  Check if Assets Modernized?  │
                 └───────────────┬───────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │ Transformed files exist?│
                    └─────┬─────────────┬─────┘
                     YES  │             │  NO
                          │             ▼
                          │   ┌───────────────────┐
                          │   │ Run Converter     │
                          │   │ Pipeline (.bsa,   │
                          │   │ .nif, .dds, .pex) │
                          │   └─────────┬─────────┘
                          │             │
                          └───────┬─────┘
                                  │
                                  ▼
                 ┌───────────────────────────────┐
                 │   Enable "PLAY" Button in UI  │
                 │  Launch Mudcrab Bevy Engine   │
                 └───────────────────────────────┘
```

---

## 2. Step-by-Step Launcher Specifications

### Step 1: Automated Game Detection

When the launcher opens, it checks common installation paths on Windows:

- **GOG Registry / Install Paths:** `C:\GOG Games\The Elder Scrolls V Skyrim Special Edition`
- **Steam Common Paths:** `C:\Program Files (x86)\Steam\steamapps\common\Skyrim Special Edition`
- **Local Workspace Fallback:** `./skyrim_game/` or `./game_data/`

If non-existent or invalid, it opens a Native File Dialog:

> _"Skyrim game directory not automatically found. Please select your Skyrim Special Edition installation folder."_

---

### Step 2: Validation & Modernization Check

The launcher validates the selected folder for signature files:

- Required: `Data/Skyrim.esm`, `Data/Skyrim - Textures0.bsa`, `Data/Skyrim - Meshes0.bsa`

It checks if the target `modern_assets/` output directory already contains valid converted data:

- `modern_assets/skyrim_world.db` (SQLite database)
- `modern_assets/meshes/` (glTF 2.0 `.glb` models)
- `modern_assets/textures/` (KTX2 compressed textures)
- `modern_assets/scripts/` (Transpiled Luau scripts)

---

### Step 3: Transformation Progress Bar (Library Call)

The launcher imports `converter` directly as a Rust crate dependency. It invokes the conversion functions in a background Rust thread while updating the GUI progress bar (what is built today is described in [section 3, Conversion screen](#3-conversion-screen)):

```rust
// Inside launcher
use converter::{NifConverter, TextureConverter, EsmConverter};

pub fn run_conversion_job(game_dir: PathBuf, progress_tx: Sender<ProgressUpdate>) {
    std::thread::spawn(move || {
        // Stage 1: Convert Textures
        TextureConverter::convert_all(&game_dir, &progress_tx);
        // Stage 2: Convert 3D Meshes
        NifConverter::convert_all(&game_dir, &progress_tx);
        // Stage 3: Parse ESM to SQLite
        EsmConverter::convert_all(&game_dir, &progress_tx);
    });
}
```

---

### Step 4: Ready to Play & Spawning the Engine

When conversion finishes (or on subsequent launches):

1. The launcher saves the path configuration to `config.json`.
2. Clicking **"PLAY MUDCRAB"** spawns the game engine binary (`engine`), passing the
   converted assets directory the configuration holds:
   ```rust
   use std::process::Command;

   pub fn launch_game_engine(assets: &std::path::Path) {
       Command::new("./engine")
           .arg("--assets")
           .arg(assets)
           .spawn()
           .expect("Failed to launch Mudcrab engine binary!");

       // Optionally close the launcher
       std::process::exit(0);
   }
   ```

---

## 3. Conversion screen

What `crates/launcher` builds today. The launcher is one window (900x680, not resizable): the
header, the conversion panel, the mod manager's drop zone (a stub that only logs what is dropped),
and the Play row. `cargo run -p launcher` starts it.

**Nothing converts on its own.** At start-up the launcher detects Skyrim (Steam libraries, the
registry, `skyrim_game/` and `game_data/`) into the Data row and looks at the Output folder
(`modern_assets` by default). If the output already holds a complete conversion, Play is enabled
at once and Start reads "Convert again". Otherwise the player presses **Start**: a full conversion writes tens of gigabytes and
takes a while, so it only ever starts on purpose. An output that is not complete but has a staging
folder beside it, left by a run whose process ended before it could publish, opens with that folder
offered for **Resume** (see below).

### What the panel shows

Conversion includes terrain LOD before final validation and publication. The
completion result reports generated chunk count and worldspace warnings; a
complete conversion does not imply LOD coverage for every worldspace.

| | |
| :--- | :--- |
| **Skyrim Data** | Where the game's assets are. Filled at start-up by game detection, or set by dropping a folder. A folder with a `Skyrim.esm` in it, case-insensitively, is a `Data` folder; dropping an installation root uses its `Data` subfolder. **Detect** looks again. |
| **Mudcrab data** | Drop a parent folder to use `<chosen>/assets` as the package. Package, staging and ingestion-cache paths appear below the row. Dropping an existing package with `conversion-manifest.json` or resumable staging beside it uses that package directly. The existing `modern_assets` default and explicit package paths stay unchanged. |
| **Bar** | Whole-run completion, from the same `converter::ProgressEstimate` the command line's status line prints, so it never moves backwards even when a stage finishes short of its total. |
| **Stage line** | The stage, its own completion, and the item and byte rates once the run is moving fast enough to measure them. Terrain LOD shows processed/total worldspaces, including skipped worlds, without asset-rate estimates. |
| **Clock line** | Elapsed time, and the estimated time left once three samples and five seconds have passed. Terrain LOD shows elapsed time only; its timing is not calibrated. |
| **Asset line** | The asset in flight. |
| **Notice pane** | Active notices keep the last five lines. Completed conversion and check results retain all lines and scroll to the summary at the top; conversion results include terrain chunk count and full LOD warnings. Mouse-wheel scrolling reads the rest. A later live notice returns to the five-line view and scrolls to its newest line. |
| **Play row** | Why Play is or is not available, and the Play button. |

Clicking a disabled button does nothing: the missing `Skyrim.esm` or the empty output is reported in
the notice pane instead.

For a newly selected parent, all conversion work stays under that folder:

```text
<chosen>/assets                 package used by Play and Check
<chosen>/assets.staging-*       unfinished conversion, kept for Resume
<chosen>/assets.assets-cache    reusable ingestion cache
```

The parent may contain other files; only its package child must pass the converter's output
safety check. Selection creates nothing. Publication still swaps sibling directories on the same
filesystem, preserving the previous package until its replacement is ready. Resume discovery
still searches beside the exact package path. Staging never moves inside the package.

### What the buttons do

| State | Start | Stop | Resume | Delete staging | Check, Full check | Path rows |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| Idle | on | off | off | off | on | on |
| Running | off | on | off | off | off | off |
| Stopping | off | on ("Quit now") | off | off | off | off |
| Finished | on ("Convert again") | off | off | off | on | on |
| Stopped with a staging folder | on ("Start over") | off | on | on | on | off |
| Stopped without one | on | off | off | off | on | on |
| Checking | off | on (stops the check) | off | off | off | off |
| Deleting | off | off | off | off | off | off |

Check and Full check are drawn as available only when the Output folder also holds a
`conversion-manifest.json`; without one there is nothing to check against.

While the engine started by Play is running, Start (and "Start over") and Resume are drawn off as
well, and a press says `Close the running game first: publishing replaces files the game has
open.`: publishing renames the Output folder, which fails on Windows while the game has
`skyrim_world.db` open, so a long run would fail at its last step. Stop, Delete staging (which
touches only the staging folder), Check and Full check stay as the table says.

**The Output folder is replaced.** A run that succeeds publishes by renaming the old Output folder
aside and deleting it, so the launcher only converts into a folder that is safe to lose: one that
does not exist yet, an empty one, or one that holds a `conversion-manifest.json` (an earlier
conversion). Anything else (a file, a folder it cannot read, a folder of other things such as a
games or documents folder) is refused with `<path> is not empty and is not a Mudcrab conversion;
choose an empty or new folder.` (or why it is not a folder). The rule lives in one function,
`output_is_safe_target`, which the Output drop and every Start, Start over and Resume press ask; the
press asks again, so a folder that has filled up since it was chosen, or the default
`modern_assets`, is refused at the press, not only at the drop.

- **Start** always converts from scratch into a fresh staging folder. In `Finished` or `Stopped` it
  means "convert again", and any staging folder from the last run stays until it is deleted.
- **Stop** asks the run to stop. The pipeline finishes the asset in flight, keeps its staging folder
  and reports it, which is what makes Resume work. The launcher is never killed.
- **Stop** pressed a second time, while the run is stopping, ends the launcher (exit code 130), as
  the command line's second Ctrl+C does. A run that publishes, or fails, before the stop reaches it
  ends the stop all the same: the panel shows `Finished` (or `Stopped`, with whatever staging folder
  the failure kept) rather than staying in `Stopping`.
- **Resume** continues from the stopped run's staging folder, with the same `Data` and output
  folders: it continues where it stopped; finished files are checked again, not redone. That is why the path rows are fixed while a staging folder is waiting: a resume must
  reuse the folders the stopped run used. Changing them means **Delete staging** first.
- **A staging folder left by an earlier session** (the window was closed, the process crashed, the
  power went) is looked for at start-up and whenever the Output folder changes, when the output is
  not already complete: the converter's `find_resumable_staging` picks the newest folder beside the
  output named `<output name>.staging-...`, by the stamp in its name, and counts the older ones. It
  puts the panel in "Stopped with a staging folder", exactly as a Stop in this session would, and
  the pane says `An unfinished conversion was found in <name>. Resume continues where it stopped;
  finished files are checked again, not redone. Delete staging removes it.`, with how many older
  ones were found. Older ones are
  never deleted automatically. Resume then runs with that folder as the pipeline's
  `resume_staging`.
- **Delete staging** removes the kept folder, which frees the disk space at the cost of starting
  over. The output folder is never touched. A full install's staging folder is tens of gigabytes, so
  the delete runs on its own thread: the panel is `Deleting`, the stage line reads `Deleting
  <folder>...`, and every button is off until it reports. When the folder is gone (or was already
  gone) the panel is "Stopped without one" and Start reads "Start" again ("Convert again" over a
  complete output). A delete that fails offers what is left of the folder for Resume and Delete
  staging again, and the pane says why.
- **Check** reads the output folder's `conversion-manifest.json` and looks at every artifact it
  lists: there, and at its recorded size. **Full check** also re-hashes every artifact, which reads
  the whole output. Both need only an Output folder with a manifest; they convert nothing and write
  nothing. The check
  runs on its own thread, the bar shows how many manifest entries it has looked at, and the panel
  returns to the state it was in before (`Checking` remembers it), so a staging folder waiting for
  Resume is still waiting afterwards. The result replaces the notice pane: `All good: N files, X,
  <mode>, T s`, or the problem count, the first eight problems, `and N more`, and one line of advice,
  in `converter check`'s wording. A manifest that cannot be read reports `Check failed: ...`.
  **Stop** during a check stops it (`converter::check_output_with_cancel`): the check finishes the
  artifacts already being read, the panel returns to the state it was in, the bar empties and the
  pane says `Check stopped.`; nothing of the part checked is shown as a result.
- **Play** is enabled when the Output folder holds a complete conversion and no run is going (a run
  publishes by renaming its staging folder over the output, which fails while the engine has files
  in it open). It starts `engine` from the launcher's own folder with `--assets <output>`.

"A complete conversion" is the check the launcher makes at start-up, when the Output folder changes
and when a run ends: `conversion-manifest.json` says `complete` at a converter schema the engine
loads (`shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION` through this converter's), `skyrim_world.db`
and `cell_cache.rkyv` are there, and `integration-report.json` passed at a world-database schema the
engine reads (`shared::supports_runtime_world_database_schema`). The manifest is read as written, so
an older output the engine starts on counts as complete; Check still compares it with this
converter's schema. It does not look at every artifact; Check and Full check do.

### Dropping things onto the launcher

- A **folder** goes to the conversion panel: a Skyrim `Data` folder or installation root fills the
  Data row. Any other folder selects a Mudcrab data parent, except an existing package with a
  manifest or resumable staging beside it, which keeps its exact output path. The resulting package must be safe to convert into
  (new, empty, or an earlier conversion; see "The Output folder is replaced"). An unsafe package keeps the Output
  row as it was, and the pane says why. Folders are taken only while the path rows are on (see
  the table): not while a conversion or a check runs, and not while a staging folder is waiting.
- A **file** with a mod's extension (`.zip`, `.7z`, `.esp`, `.esm`, `.esl`) goes to the mod
  manager, which only logs it for now. Any other file is refused with a notice.

### Trying it without the game

```sh
cargo run -p dummy-content -- gen Data
cargo run -p launcher
```

Drop the generated `Data` folder onto the window (it holds a `Skyrim.esm`, so it fills the Data
row), then press Start; the default output folder is already set. The stages finish in seconds and
Play is enabled at the end. Start and Stop mid-run to see a staging folder kept: it must exist on
disk while the panel offers Resume, and be gone after Delete staging. After a finished run, Check and
Full check read `All good`; delete one converted file and Check lists it as missing.

### How it is built

| File | What it holds |
| :--- | :--- |
| `src/conversion/state.rs` | The state machine: `ConversionState`, `Input`, `Effect`, `apply`, and the `controls` table above, plus `CheckSummary`, the lines a check's result shows. No Bevy types, so every transition is a unit test. |
| `src/conversion/runner.rs` | Runs a conversion on its own thread with its own tokio runtime and reports `RunMessage::{Progress, Finished, Failed}` on a crossbeam channel. Returns the `Cancellation` the Stop button holds. `spawn_check` runs `converter::check_output_with_cancel` on its own thread with a shared stop flag and reports `RunMessage::{CheckProgress, CheckFinished, CheckFailed, CheckCancelled}`, its per-entry progress thinned to 200 steps. `spawn_delete` removes a staging folder on its own thread and reports `RunMessage::StagingDeleted`. |
| `src/conversion/status.rs` | `ConversionStatus`: the bar, the three lines and the notices, from the converter's `ProgressEstimate` and formatters. |
| `src/conversion/panel.rs` | The panel's Bevy UI scene, the button and drag-and-drop systems, and the systems that draw the state. |
| `src/conversion/mod.rs` | `GamePathConfig` (the two folders), `ConversionLogicPlugin` (state machine, message drain, effects queue; no UI), `ConversionPanelPlugin` (the panel on top), the output check behind Play and the manifest flag behind Check, the offer of a leftover staging folder, and the systems that carry effects out. |
| `src/handlers.rs` | Play and the engine launch, the mod drop zone, and `LauncherState`, which follows the conversion. |
| `src/ui.rs` | The window's scene: header, conversion panel, mod manager, Play row. |
| `src/game_detection.rs` | Steam library and registry detection, and what a dropped folder means. |

Work happens in one order each frame (`LauncherSet`): presses and dropped items become inputs, the
logic folds them and the run's messages into the state and the status, the effects are carried out
(start a run, cancel it, delete a staging folder, start or stop a check), and the widgets are drawn
from the result. After the UI layout, the notice pane's scroll position is clamped to its content,
so every mouse-wheel step moves it even after a new notice scrolled it to the end.
