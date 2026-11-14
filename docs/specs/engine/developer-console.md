# Developer console

Tracking: [#88](https://github.com/Mudcrab-Team/mudcrab/issues/88).

Backtick opens or closes the console; Escape closes it and leaves the cursor released.
Opening pauses virtual time and releases the cursor. Closing preserves a pre-existing pause;
backtick restores prior cursor capture after the closing frame. Gameplay input remains blocked
while open and on the closing frame. Tab completes command names through the existing registry.

## Reference selection

With the console open, left-click an object in the world above the overlay. The selection row
shows its placed reference FormID as eight uppercase hexadecimal digits, including leading zeros.
This is the reference's ID, not its base record ID or a Bevy entity number. Clicking a scene child
resolves the nearest ancestor carrying both `FormId` and `CellRef`; shared mesh assets and entity
names do not determine identity.

A yellow wire box encloses the selected reference's visible mesh descendants. Bounds follow
current global transforms, including moving clutter and render-origin rebasing. Highlighting uses
its own gizmo group on the placed-object layer, without changing materials or reflection views.
The box marks aggregate bounds, not a silhouette or a shader outline.

An empty world click clears selection. Clicking the console overlay preserves it. Opening-frame,
closing-frame, unfocused-window, settings-menu, and bug-report capture clicks do not pick objects.
Selection clears on close, reference despawn, or identity removal/change. Capture suppression
hides the highlight without releasing console input ownership or changing the selection.

Picking uses Bevy's on-demand CPU triangle ray cast against resident visible meshes in the main
camera's layers. Non-reference meshes can occlude references; synthetic objects without a source
reference do not receive an invented ID. The backend is not installed as a continuous mesh-picking
plugin. Objects whose CPU meshes are unavailable cannot be selected. Alpha-tested texture holes,
GPU skinning, morph deformation, and material-specific visibility are not evaluated by the CPU
ray cast; animated or translucent surfaces can differ from the rendered image. Native visual
qualification remains required.

## Issue #88 recommendation inventory

| Request | Status and boundary |
| --- | --- |
| Backtick, pause, cursor ownership, help, errors, extensible registry | Merged in [#123](https://github.com/Mudcrab-Team/mudcrab/pull/123); retained. |
| Taylor's command autofill recommendation | Merged in #123; retained, including short names and first-token completion. |
| Taylor's click, highlight, and reference-ID recommendation | Implemented locally here; CPU execution and native visual qualification still pending. |
| Screenshot and local diagnostic bundle, with path sanitation | Merged in [#243](https://github.com/Mudcrab-Team/mudcrab/pull/243); no duplicate reporting implementation. |
| Existing debug actions exposed as commands | `ToggleCollision` (`tcl`) and `ToggleCollisionGeometry` (`tcg`) exist. Debug tankard spawn/pickup/drop remain key-only (`T`/`E`), an unfulfilled part of the original issue. |
| Cell travel | `CenterOnExterior` (`coe`) and `CenterOnCell` (`coc`) merged in [#124](https://github.com/Mudcrab-Team/mudcrab/pull/124). Active [#454](https://github.com/Mudcrab-Team/mudcrab/pull/454) owns travel guards; this slice changes neither travel nor landing behavior. COC markers and safe walk-mode arrival remain separate qualification/work. |
| Maintained Skyrim command inventory and eventual parity | Still incomplete. This table tracks #88's requests, not a complete Skyrim command catalog. Inventory, quests, scripts, arbitrary spawning, and remaining commands need their own scoped work. |
| Command history and quoted arguments | Listed as deferred in #123; current parser still splits whitespace and has no history navigation. These are follow-ups, not additional explicit Taylor recommendations in #88's discussion. |
| Idle console change detection | Active [#455](https://github.com/Mudcrab-Team/mudcrab/pull/455) owns it. Existing `ConsoleState` mutation behavior is unchanged here; selection uses a separate resource. |

The live discussion was inspected on 2026-10-31. Taylor owns this console follow-up; the old issue
assignment does not change that ownership. This slice does not close #88 or establish command parity.

## Validation scope

`console_selection::tests` supplies CPU meshes, camera data, visibility, and overlay layout without
starting a renderer or game. It exercises reference ancestry, nearest triangle hits, occlusion,
selection bounds, selection cleanup, overlay/HiDPI handling, pause/cursor transitions, capture/menu
suppression, and preservation of Tab completion. The existing physics closing-frame regression
also delivers a left click alongside the closing key to exercise the real gameplay recapture gate.
These tests need a managed isolated Cargo run. Native picking, actual UI layout, rendering, and
visual/performance acceptance are separate checks.
