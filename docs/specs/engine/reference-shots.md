# Reference shots (`--shots`)

`--shots <file>` renders a list of exact camera poses, one PNG each, then exits. It exists for
visual review: take a screenshot in Skyrim, write down the camera pose, and render the same view
here, so the two images can be compared side by side and a regression shows up as a changed image
rather than a vague impression.

```
engine --assets <converted assets> --shots review/riverwood.json [--shots-out review/out]
```

Add `--hidden-window` to render and capture without displaying a game window.
The primary window starts with visibility disabled and keeps rendering in the
background; screenshots still read the same render target. This option also
works with benchmark `--acceptance-screenshot` captures. It keeps an actual
rendering window, whereas `--headless` normally removes that window. When both
are supplied, `--hidden-window` retains the hidden render target. Hidden runs
do not capture interactive mouse/controller input.

## The shots file

```json
{
  "width": 1400,
  "height": 1050,
  "shots": [
    {
      "name": "riverwood-bridge",
      "worldspace_id": 60,
      "interior_cell_id": null,
      "position": [21340.0, -44890.0, 520.0],
      "yaw": 152.7,
      "pitch": 7.9,
      "hfov": 75.0,
      "reference": "review/riverwood-bridge.jpg",
      "note": "the bridge from the south bank"
    }
  ]
}
```

| Field | Meaning |
| :-- | :-- |
| `width`, `height` | The frame size in pixels, at most 8192 each. The production camera renders into a dedicated image target at this size, independent of window size or display scaling. Use the size the reference screenshots were taken at. |
| `name` | Output file stem: the image is `<out>/<name>.png`. A plain file name, unique in the file, that Windows can create (see below). |
| `worldspace_id` | The worldspace of an exterior shot (60 is Tamriel). `null` or absent leaves it to `--worldspace`. |
| `interior_cell_id` | An interior cell. Such a shot is skipped and logged: the streamer holds exterior cells only. |
| `position` | The camera's eye in Creation units, absolute (not relative to a cell or the render origin). |
| `yaw` | Skyrim heading in degrees: 0 looks north (`+Y`), 90 east (`+X`), clockwise seen from above. |
| `pitch` | Skyrim's X angle in degrees: positive looks down. |
| `hfov` | Horizontal field of view in degrees, between 0 and 180; the vertical one follows from the frame's aspect. |
| `probes` | Optional named world positions to measure. The run writes `<out>/<name>.stages.json` with each probe's pre-tonemap HDR color and output bytes; the format, the probe fields and the V1 probe set are in [visual-profiles.md](visual-profiles.md#writing-stage-receipts-from-a-shots-run). |
| `reference`, `note` | Free text for people and comparison tools; the engine ignores them, and any other field. |

`name` has to be a file this engine can create on Windows, since it is written there: no `/`, `\`,
`:`, `<`, `>`, `"`, `|`, `?` or `*`, no control character, no trailing dot or space, and not one of the
reserved device names `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9` or `LPT1`-`LPT9` - with or without
an extension, and ignoring case, so `con.png` and `Nul.x` are refused too.

A file that cannot be read, is not JSON of this shape, has no shots, a zero frame, a side over 8192
pixels, a missing or non-numeric pose field, a field of view out of range, or a name that is empty,
repeated or not a name Windows can create stops the run before a window opens, with a message that
names the file and the problem.

## What a run does

Streaming starts at the first exterior shot: its worldspace (when it names one) and the grid square
its camera stands over. The run streams that one worldspace; a shot in another worldspace is
skipped and logged, like an interior shot, so a file that mixes worldspaces is rendered one
worldspace per run.

For each shot in order, the run places the camera at the pose (the streamer then loads the cells
around it) and waits until the view has settled: no cell loading, no database request in flight,
no model or surface waiting for its assets, no model waiting to be armed, no out-of-range cell
still waiting to be unloaded, no new failed cells, asset-load failures or material, terrain,
water, transform-bounds or renderer validation failures since the shot was posed, and the
renderer's final path running, for `SETTLE_QUIET_FRAMES`
(10) frames in a row, and not before `WARM_UP_SECONDS` (2 s) after start-up, while the first
pipelines compile. The frame count is exact: a view that has been quiet for ten frames running is
photographed on the tenth, and the `frames=` of its log line is then 10 - the count of quiet
frames is advanced before it is read, so it is not one more than the constant. The run then saves
the camera's dedicated image target to `<out>/<name>.png`. A shot that has not settled after
`--shots-settle-timeout` seconds (default 30 s; positive, finite, at most 600 s) is still captured, and the log says the timeout took it and what
was still pending or which failures prevented settling. A new failure prevents that shot from
settling even after pending work drains; its timeout capture fails the run.

`--shots-out` defaults to a `<file stem>-shots/` folder beside the shots file. `shots.log` there
has one line per shot: its name, the frames it waited, whether it settled or timed out, the image's
path and exact render-target dimensions. Skipped shots have a line
saying why. Each line is appended as its shot finishes, and the file is created with the first
line, so a run that is killed keeps the shots it had already taken.

The run exits with success once every shot is written, and exits non-zero - the process's exit code
is not zero, and `main` reports the failure - when an image or the log could not be written, when a
shot was given up on, when the engine exited with an error for any other reason, and when an
acceptance run's gates did not pass. `scripts/phase2-acceptance.ps1` records that exit code per
command and marks the command failed on anything but 0, so a failed shots or acceptance run is now
visible in a campaign report instead of being read as a pass.

`--headless` is ignored during a shots run, because the shots runner requires the render loop. `--shots`
cannot be combined with a benchmark, `--acceptance-screenshot`, `--auto-fly-speed` or a fixture.
`--run-label` names the run in the window title (`Mudcrab - shots: <label>`).

## Game clock and sun direction

`--game-hour <hour>` starts the game clock at a finite hour in `[0,24)` (default 12).
Ordinary play advances at `--timescale <scale>` game seconds per real second (default 20;
0 freezes). Shots, acceptance screenshots, benchmarks and matched routes freeze at the launch
hour regardless of timescale, so streaming duration cannot move shadows between captures.

The directional light follows the active worldspace's climate TNAM sunrise/sunset midpoints
(including PNAM-requested parent climate inheritance),
expanded by half `fSunAlphaTransTime`. A parameter sweeps from +1 to -1 across this day window
and returns overnight. The normalized Creation-space toward-sun vector is
`(parameter * fSunDirXExtreme, fSunYExtreme * fSunDirXExtreme / fSunXExtreme, -fSunZExtreme)`;
engine axes map it to `(x,z,-y)`. This geometry follows spud (MIT, chasmlol/spud, e5f2de4);
it is distinct from the visible sun-disc path.

Winning canonical GMST records supply those five values. Optional `[Sky]` keys of the same names
in `--ini` files override them individually. Missing inputs use fallback values
400 / 25 / -100 / 400 / 2, and a symmetric 06:00/18:00 day without usable climate timing.
Invalid record/INI fields warn and retain valid neighbors or fallbacks. TNAM bytes are ten-minute
units; ordered same-day transitions are supported. A fade that would consume a full day uses
the unexpanded climate midpoints. `--sun-position` remains a fixed explicit bearing/elevation
override. Capture-frame receipts include the actual clock and engine-space toward-sun vector.

This step changes direction only. Existing constant clear-day light intensity and palette remain
at night while the X parameter returns; no moon light, astronomical orbit, weather selection,
seasonal path or night palette is introduced.
