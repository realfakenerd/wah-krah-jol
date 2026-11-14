//! The conversion panel in the launcher's window: the folder rows, the bar, the notice pane and the
//! buttons, built as Bevy UI scenes. Everything drawn here comes from [`CurrentConversion`] and
//! [`ConversionStatus`]; nothing here decides anything, it only turns presses and dropped folders
//! into [`Input`]s and the state into pixels.

use super::{
    ConversionStatus, CurrentConversion, GamePathConfig, MANIFEST_FILE, OutputHasManifest,
    OutputReady, PendingInputs, output_is_safe_target,
    state::{self, Controls, Input},
};
use crate::{engine_process::EngineProcess, game_detection, handlers};

/// Why Start, Start over and Resume are refused while the engine started by Play runs: publishing
/// renames the Output folder, which fails on Windows while the game has its files open, and an
/// hours-long run would fail at its last step.
pub const GAME_RUNNING_NOTICE: &str =
    "Close the running game first: publishing replaces files the game has open.";
use bevy::prelude::*;
use bevy::ui_widgets::ScrollArea;
use converter::CheckMode;

/// A button. The panel's controls are one family so one system can read them all, and so the
/// state machine's table is applied in exactly one place.
#[derive(Component, Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum ControlButton {
    #[default]
    Start,
    Stop,
    Resume,
    DeleteStaging,
    Detect,
    /// A quick check of the output folder: every artifact is there at its recorded size.
    Check,
    /// A full check: the quick check, plus every artifact's hash.
    FullCheck,
}

impl ControlButton {
    /// Whether this control is enabled in the state's row of the design's table.
    fn enabled(self, controls: &Controls) -> bool {
        match self {
            ControlButton::Start => controls.start,
            ControlButton::Stop => controls.stop,
            ControlButton::Resume => controls.resume,
            ControlButton::DeleteStaging => controls.delete_staging,
            // Detection only fills the Data row, which the run's folders rule governs.
            ControlButton::Detect => controls.paths,
            ControlButton::Check | ControlButton::FullCheck => controls.check,
        }
    }

    /// Whether a run would have to have its folders chosen first.
    fn needs_folders(self) -> bool {
        matches!(self, ControlButton::Start | ControlButton::Resume)
    }

    /// Whether this control needs a manifest in the output folder, and nothing else chosen.
    fn needs_manifest(self) -> bool {
        matches!(self, ControlButton::Check | ControlButton::FullCheck)
    }

    /// Whether a run would publish into the Output folder, which the running game holds open.
    fn needs_game_closed(self) -> bool {
        matches!(self, ControlButton::Start | ControlButton::Resume)
    }

    /// Whether the button is drawn as available: its row of the table allows it, and the folders
    /// it works on are chosen. A check needs the Output folder to hold a manifest (`has_manifest`,
    /// from [`OutputHasManifest`]): without one there is nothing to check against. Start (and
    /// Start over) and Resume are off while the engine started by Play runs (`game_running`).
    pub fn available(
        self,
        controls: &Controls,
        paths: &GamePathConfig,
        has_manifest: bool,
        game_running: bool,
    ) -> bool {
        self.enabled(controls)
            && (!self.needs_folders() || paths.ready())
            && (!self.needs_manifest() || has_manifest)
            && !(game_running && self.needs_game_closed())
    }

    /// The check a check button asks for.
    fn check_mode(self) -> Option<CheckMode> {
        match self {
            ControlButton::Check => Some(CheckMode::Quick),
            ControlButton::FullCheck => Some(CheckMode::Full),
            _ => None,
        }
    }

    /// What the button says in the current state. Start over an output that already holds a
    /// complete conversion says what it would do: convert again.
    fn label(self, state: &state::ConversionState, ready: OutputReady) -> &'static str {
        match self {
            ControlButton::Start => match state::start_label(state) {
                "Start" if ready.0 => "Convert again",
                label => label,
            },
            ControlButton::Stop => state::stop_label(state),
            ControlButton::Resume => "Resume",
            ControlButton::DeleteStaging => "Delete staging",
            ControlButton::Detect => "Detect",
            ControlButton::Check => "Check",
            ControlButton::FullCheck => "Full check",
        }
    }
}

/// The label inside a button, so the label can change with the state.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ControlLabel(pub ControlButton);

#[derive(Component, Default, Clone)]
pub struct DataPathLabel;

#[derive(Component, Default, Clone)]
pub struct DataPathText;

#[derive(Component, Default, Clone)]
pub struct OutputPathText;

#[derive(Component, Default, Clone)]
pub struct OutputLocationsText;

/// The scroll area that bounds [`OutputLocationsText`].
#[derive(Component, Default, Clone)]
pub struct OutputLocationsPane;

/// Room for the three location lines at 12 px (about 15 px each) with a little to spare; more
/// than that scrolls.
const LOCATIONS_MAX_HEIGHT: f32 = 60.0;

/// The fill inside the progress bar's track.
#[derive(Component, Default, Clone)]
pub struct BarFill;

#[derive(Component, Default, Clone)]
pub struct OverallText;

#[derive(Component, Default, Clone)]
pub struct StageText;

#[derive(Component, Default, Clone)]
pub struct ClockText;

#[derive(Component, Default, Clone)]
pub struct FileText;

/// The scrolling box around the notices, so a long result scrolls instead of spilling out
/// of the panel.
#[derive(Component, Default, Clone)]
pub struct NoticePane;

#[derive(Component, Default, Clone)]
pub struct NoticeText;

pub(crate) const BACKGROUND: Color = Color::srgb(0.08, 0.09, 0.12);
pub(crate) const PANEL: Color = Color::srgb(0.12, 0.14, 0.18);
pub(crate) const BORDER: Color = Color::srgb(0.25, 0.28, 0.35);
pub(crate) const LABEL_COLOR: Color = Color::srgb(0.60, 0.65, 0.75);
pub(crate) const TEXT_COLOR: Color = Color::srgb(0.85, 0.88, 0.95);
pub(crate) const TITLE_COLOR: Color = Color::srgb(0.90, 0.80, 0.45);
pub(crate) const BUTTON_ON: Color = Color::srgb(0.18, 0.55, 0.34);
pub(crate) const BUTTON_ON_HOVER: Color = Color::srgb(0.24, 0.68, 0.42);
pub(crate) const BUTTON_OFF: Color = Color::srgb(0.16, 0.18, 0.22);
pub(crate) const BUTTON_OFF_TEXT: Color = Color::srgb(0.42, 0.45, 0.52);

/// Far past the end of any notice text; [`clamp_notice_scroll`] brings it back to the real end once
/// the layout knows how tall the text is.
const SCROLL_TO_END: f32 = 1.0e6;

/// The whole panel: a title, the two folder rows, the bar and its three lines, the notice pane and
/// the button row. It grows to fill the height the launcher leaves it, and the notice pane takes
/// whatever the fixed rows do not.
pub fn conversion_panel() -> impl Scene {
    bsn! {
        #ConversionPanel
        Node {
            width: Val::Percent(100.0),
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(12.0)),
            row_gap: Val::Px(6.0),
            border: UiRect::all(Val::Px(1.0)),
        }
        BackgroundColor(PANEL)
        BorderColor::all(BORDER)
        Children [
            ui_title(),
            crate::mo2_settings::source_row(),
            ui_data_row(),
            ui_output_row(),
            ui_output_locations(),
            ui_progress_row(),
            ui_stage_line(),
            ui_clock_line(),
            ui_file_line(),
            ui_notice_pane(),
            ui_button_row(),
        ]
    }
}

fn ui_title() -> impl Scene {
    bsn! {
        Text::new("Convert Skyrim assets")
        TextFont { font_size: FontSize::Px(16.0) }
        TextColor(TITLE_COLOR)
    }
}

fn row_label(text: &'static str) -> impl Scene {
    bsn! {
        Text::new(text)
        TextFont { font_size: FontSize::Px(14.0) }
        TextColor(LABEL_COLOR)
        Node { width: Val::Px(100.0) }
    }
}

/// Builds the source-path row with its label, path text, and detection or MO2 settings button.
fn ui_data_row() -> impl Scene {
    bsn! {
        Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(10.0),
        }
        Children [
            (DataPathLabel row_label("Skyrim Data")),
            (
                DataPathText
                Text::new("looking for Skyrim...")
                TextFont { font_size: FontSize::Px(14.0) }
                TextColor(TEXT_COLOR)
                Node { flex_grow: 1.0 }
            ),
            ui_button(ControlButton::Detect, Val::Px(90.0))
        ]
    }
}

fn ui_output_row() -> impl Scene {
    bsn! {
        Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(10.0),
        }
        Children [
            row_label("Mudcrab data"),
            (
                OutputPathText
                Text::new(".")
                TextFont { font_size: FontSize::Px(14.0) }
                TextColor(TEXT_COLOR)
                Node { flex_grow: 1.0 }
            ),
            (
                Text::new("(drop parent)")
                TextFont { font_size: FontSize::Px(13.0) }
                TextColor(LABEL_COLOR)
                Node { width: Val::Px(90.0) }
            )
        ]
    }
}

/// The three location lines in a scroll area no taller than [`LOCATIONS_MAX_HEIGHT`]. A long path
/// wraps onto more lines; without the bound it would push the progress rows and the button row
/// off the bottom of the window.
fn ui_output_locations() -> impl Scene {
    bsn! {
        OutputLocationsPane
        ScrollArea
        Node {
            width: Val::Percent(100.0),
            max_height: Val::Px(LOCATIONS_MAX_HEIGHT),
            flex_shrink: 0.0,
            overflow: Overflow::scroll_y(),
        }
        Children [
            (
                OutputLocationsText
                Text::new("")
                TextFont { font_size: FontSize::Px(12.0) }
                TextColor(LABEL_COLOR)
            )
        ]
    }
}

fn ui_progress_row() -> impl Scene {
    bsn! {
        Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(12.0),
        }
        Children [
            (
                #ProgressBarTrack
                Node {
                    flex_grow: 1.0,
                    height: Val::Px(16.0),
                    border: UiRect::all(Val::Px(1.0)),
                    overflow: Overflow::clip(),
                }
                BackgroundColor(BACKGROUND)
                BorderColor::all(BORDER)
                Children [
                    (
                        BarFill
                        Node { width: Val::Percent(0.0), height: Val::Percent(100.0) }
                        BackgroundColor(BUTTON_ON)
                    )
                ]
            ),
            (
                OverallText
                Text::new("0%")
                TextFont { font_size: FontSize::Px(15.0) }
                TextColor(TITLE_COLOR)
                Node { width: Val::Px(60.0) }
            )
        ]
    }
}

fn ui_stage_line() -> impl Scene {
    bsn! {
        StageText
        Text::new("waiting for the first asset")
        TextFont { font_size: FontSize::Px(14.0) }
        TextColor(TEXT_COLOR)
    }
}

fn ui_clock_line() -> impl Scene {
    bsn! {
        ClockText
        Text::new("00:00:00 elapsed")
        TextFont { font_size: FontSize::Px(14.0) }
        TextColor(TEXT_COLOR)
    }
}

fn ui_file_line() -> impl Scene {
    bsn! {
        FileText
        Text::new("")
        TextFont { font_size: FontSize::Px(13.0) }
        TextColor(LABEL_COLOR)
    }
}

fn ui_notice_pane() -> impl Scene {
    bsn! {
        NoticePane
        ScrollArea
        Node {
            width: Val::Percent(100.0),
            min_height: Val::Px(60.0),
            flex_grow: 1.0,
            flex_basis: Val::Px(0.0),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(Val::Px(1.0)),
            padding: UiRect::all(Val::Px(6.0)),
            overflow: Overflow::scroll_y(),
        }
        BackgroundColor(BACKGROUND)
        BorderColor::all(BORDER)
        Children [
            (
                NoticeText
                Text::new("")
                TextFont { font_size: FontSize::Px(13.0) }
                TextColor(TEXT_COLOR)
            )
        ]
    }
}

fn ui_button_row() -> impl Scene {
    bsn! {
        Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            column_gap: Val::Px(12.0),
        }
        Children [
            ui_button(ControlButton::Start, Val::Px(150.0)),
            ui_button(ControlButton::Stop, Val::Px(110.0)),
            ui_button(ControlButton::Resume, Val::Px(110.0)),
            ui_button(ControlButton::DeleteStaging, Val::Px(150.0)),
            ui_button(ControlButton::Check, Val::Px(100.0)),
            ui_button(ControlButton::FullCheck, Val::Px(120.0))
        ]
    }
}

/// One button: a filled box with a label that changes with the state, disabled by its colour
/// rather than by taking the button component away, so a press on a disabled control still reaches
/// the state machine and is refused there.
fn ui_button(control: ControlButton, width: Val) -> impl Scene {
    bsn! {
        template_value(control)
        Button
        Node {
            width: { width },
            height: Val::Px(30.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
        }
        BackgroundColor(BUTTON_OFF)
        Children [
            (
                ControlLabel({ control })
                Text::new("")
                TextFont { font_size: FontSize::Px(14.0) }
                TextColor(BUTTON_OFF_TEXT)
            )
        ]
    }
}

/// Fills the Data row before the first frame, so a player whose Skyrim is installed where Steam
/// says it is only has to press Start. A Data folder that is already set is kept.
pub fn detect_skyrim_at_start(
    mut paths: ResMut<GamePathConfig>,
    mut status: ResMut<ConversionStatus>,
) {
    if paths.skyrim_data_path.is_some() {
        return;
    }
    match game_detection::find_skyrim_data_dir() {
        Some(data) => paths.skyrim_data_path = Some(data),
        None => status.push_notice(
            "Skyrim Special Edition was not found in the Steam libraries or in local folders: drop its Data folder onto the launcher.",
        ),
    }
}

/// Turns button presses into inputs. A press the table forbids is dropped here and refused by the
/// state machine as well, so an unexpected event can never start a second run.
#[allow(clippy::too_many_arguments)] // Bevy injects each system parameter.
pub fn click_controls(
    buttons: Query<(&Interaction, &ControlButton), Changed<Interaction>>,
    state: Res<CurrentConversion>,
    has_manifest: Res<OutputHasManifest>,
    engine: Option<Res<EngineProcess>>,
    mut source: Option<ResMut<crate::mo2_settings::SourceSettings>>,
    mut paths: ResMut<GamePathConfig>,
    mut inputs: ResMut<PendingInputs>,
    mut status: ResMut<ConversionStatus>,
) {
    for (interaction, control) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let controls = state::controls(&state.0);
        if *control == ControlButton::Detect
            && let Some(source) = source.as_mut().filter(|source| source.use_mo2)
        {
            if crate::mo2_settings::editable(&state.0) {
                source.open = true;
            }
            continue;
        }
        if !control.enabled(&controls) {
            continue;
        }
        if matches!(control, ControlButton::Start | ControlButton::Resume)
            && let Some(source) = &source
        {
            let mut config = converter::PipelineConfig::new("", "");
            if let Err(reason) = source.apply(&mut config) {
                status.push_notice(&reason);
                continue;
            }
        }
        match control {
            ControlButton::Detect => match game_detection::find_skyrim_data_dir() {
                Some(data) => {
                    paths.skyrim_data_path = Some(data);
                    status.push_notice("Found Skyrim's Data folder.");
                }
                None => status.push_notice(
                    "Skyrim Special Edition was not found: drop its Data folder onto the launcher instead.",
                ),
            },
            ControlButton::Start | ControlButton::Resume if engine.is_some() => {
                status.push_notice(GAME_RUNNING_NOTICE);
            }
            ControlButton::Start | ControlButton::Resume => match paths.pair() {
                // Looked at on the press, not only when the folder was dropped: files may have
                // appeared in it since.
                Some((data, output)) => match output_is_safe_target(&output) {
                    Ok(()) => inputs.push(if *control == ControlButton::Start {
                        Input::Start { data, output }
                    } else {
                        Input::Resume { data, output }
                    }),
                    Err(reason) => status.push_notice(&reason),
                },
                None => status.push_notice(
                    "Choose a Skyrim Data folder (one holding Skyrim.esm) and a Mudcrab data parent folder first.",
                ),
            },
            ControlButton::Stop => inputs.push(Input::Stop),
            ControlButton::DeleteStaging => inputs.push(Input::DeleteStaging),
            ControlButton::Check | ControlButton::FullCheck => {
                if paths.converted_assets_path.as_os_str().is_empty() {
                    status.push_notice("Choose the output folder to check first.");
                } else if !has_manifest.0 {
                    status.push_notice(&format!(
                        "{} has no {MANIFEST_FILE} to check against: convert into it first.",
                        paths.converted_assets_path.display()
                    ));
                } else if let Some(mode) = control.check_mode() {
                    inputs.push(Input::Check {
                        output: paths.converted_assets_path.clone(),
                        mode,
                    });
                }
            }
        }
    }
}

/// A dropped folder fills a path row: a Skyrim `Data` folder (or an installation root) becomes the
/// Data folder, and any other folder becomes the data parent (or an existing package, recognized
/// by its manifest or resumable staging beside it). Dropping is the only way to choose a
/// folder, so nothing new has to be learned from a file dialog. Dropped files belong to the mod
/// manager ([`handlers::handle_mod_drag_and_drop`]); a file that is not a mod is refused here.
pub fn accept_dropped_folder(
    mut dropped: MessageReader<FileDragAndDrop>,
    state: Res<CurrentConversion>,
    mut source: Option<ResMut<crate::mo2_settings::SourceSettings>>,
    mut paths: ResMut<GamePathConfig>,
    mut status: ResMut<ConversionStatus>,
) {
    for event in dropped.read() {
        let FileDragAndDrop::DroppedFile { path_buf, .. } = event else {
            continue;
        };
        if let Some(source) = &mut source
            && source.open
        {
            if crate::mo2_settings::editable(&state.0) {
                source.set_instance(path_buf);
            }
            continue;
        }
        if !path_buf.is_dir() {
            if !handlers::is_mod_file(path_buf) {
                status.push_notice(&format!(
                    "{} is neither a folder nor a mod file (.zip, .7z, .esp, .esm, .esl).",
                    path_buf.display()
                ));
            }
            continue;
        }
        if !state::controls(&state.0).paths {
            status.push_notice(
                "The folders are fixed while a conversion or a check runs, and while a stopped run's staging folder is waiting.",
            );
            continue;
        }
        if let Some(data) = game_detection::data_dir_from_drop(path_buf) {
            status.push_notice(&format!("Data folder: {}", data.display()));
            paths.skyrim_data_path = Some(data);
        } else {
            // Publishing replaces the Output folder, so a folder of other things is never taken.
            match paths.choose_data_folder(path_buf) {
                Ok(()) => {
                    status.push_notice(&format!(
                        "Mudcrab data folder: {}",
                        paths.data_folder_label()
                    ));
                }
                Err(reason) => status.push_notice(&reason),
            }
        }
    }
}

/// The two path rows' texts; see `draw_paths`.
type PathTexts = (
    Query<'static, 'static, &'static mut Text, With<DataPathText>>,
    Query<'static, 'static, &'static mut Text, With<OutputPathText>>,
    Query<'static, 'static, &'static mut Text, With<DataPathLabel>>,
    Query<'static, 'static, &'static mut Text, With<OutputLocationsText>>,
);

/// The status lines' texts; see `draw_labels`.
type LabelTexts = (
    Query<'static, 'static, &'static mut Text, With<StageText>>,
    Query<'static, 'static, &'static mut Text, With<ClockText>>,
    Query<'static, 'static, &'static mut Text, With<FileText>>,
    Query<'static, 'static, &'static mut Text, With<NoticeText>>,
);

/// Updates source and output path labels for the selected Skyrim Data or MO2 source.
pub fn draw_paths(
    paths: Res<GamePathConfig>,
    source: Option<Res<crate::mo2_settings::SourceSettings>>,
    // Bevy cannot prove these Text queries are disjoint from their With filters,
    // so they share one set and are borrowed in turn.
    mut texts: ParamSet<PathTexts>,
) {
    let mo2 = source.as_ref().filter(|source| source.use_mo2);
    for mut text in &mut texts.p0() {
        text.0 = if let Some(source) = mo2 {
            source
                .selection
                .as_ref()
                .map(|selection| crate::mo2_settings::display_path(&selection.instance_path))
                .unwrap_or_else(|| "not configured - open MO2 settings".into())
        } else {
            paths.data_label()
        };
    }
    for mut text in &mut texts.p1() {
        text.0 = paths.data_folder_label();
    }
    for mut text in &mut texts.p2() {
        text.0 = if mo2.is_some() {
            "MO2 instance"
        } else {
            "Skyrim Data"
        }
        .into();
    }
    for mut text in &mut texts.p3() {
        text.0 = paths.locations_label();
    }
}

pub fn draw_bar(
    status: Res<ConversionStatus>,
    mut fill: Query<&mut Node, With<BarFill>>,
    mut overall: Query<&mut Text, With<OverallText>>,
) {
    let (percent, label) = status.progress_bar();
    for mut node in &mut fill {
        node.width = Val::Percent(percent);
    }
    for mut text in &mut overall {
        text.0 = label.clone();
    }
}

/// Draws the three status lines and the notices. When the notices change, the pane scrolls to
/// where the news is: the top for a completed result, which reads from its first line, and the end
/// for anything else, whose newest line is last.
pub fn draw_labels(
    status: Res<ConversionStatus>,
    mut texts: ParamSet<LabelTexts>,
    mut pane: Query<&mut ScrollPosition, With<NoticePane>>,
) {
    for mut text in &mut texts.p0() {
        text.0 = status.stage_line();
    }
    for mut text in &mut texts.p1() {
        text.0 = status.clock_line();
    }
    for mut text in &mut texts.p2() {
        text.0 = status.file_line();
    }
    let notices = status.notice_text();
    let mut changed = false;
    for mut text in &mut texts.p3() {
        if text.0 != notices {
            text.0 = notices.clone();
            changed = true;
        }
    }
    if changed {
        let y = if status.check.is_some() || status.run_finished {
            0.0
        } else {
            SCROLL_TO_END
        };
        for mut scroll in &mut pane {
            scroll.0.y = y;
        }
    }
}

/// Updates button appearance and labels from conversion state, source readiness, and engine status.
#[allow(clippy::too_many_arguments)] // Bevy injects each system parameter.
pub fn draw_controls(
    state: Res<CurrentConversion>,
    paths: Res<GamePathConfig>,
    ready: Res<OutputReady>,
    has_manifest: Res<OutputHasManifest>,
    engine: Option<Res<EngineProcess>>,
    source: Option<Res<crate::mo2_settings::SourceSettings>>,
    mut buttons: Query<(
        &ControlButton,
        &Interaction,
        &mut BackgroundColor,
        Option<&mut Node>,
    )>,
    mut labels: Query<(&ControlLabel, &mut Text, &mut TextColor)>,
) {
    let controls = state::controls(&state.0);
    let game_running = engine.is_some();
    let source_ready = source
        .as_ref()
        .is_none_or(|source| !source.use_mo2 || source.selection.is_some());
    let mo2 = source.as_ref().is_some_and(|source| source.use_mo2);
    for (control, interaction, mut background, node) in &mut buttons {
        let enabled = if mo2 && *control == ControlButton::Detect {
            crate::mo2_settings::editable(&state.0)
        } else {
            control.available(&controls, &paths, has_manifest.0, game_running)
                && (!control.needs_folders() || source_ready)
        };
        if *control == ControlButton::Detect
            && let Some(mut node) = node
        {
            node.width = Val::Px(if mo2 { 120.0 } else { 90.0 });
        }
        background.0 = match (enabled, *interaction) {
            (false, _) => BUTTON_OFF,
            (true, Interaction::Hovered) => BUTTON_ON_HOVER,
            (true, _) => BUTTON_ON,
        };
    }
    for (label, mut text, mut color) in &mut labels {
        let wanted = if mo2 && label.0 == ControlButton::Detect {
            "MO2 settings"
        } else {
            label.0.label(&state.0, *ready)
        };
        if text.0 != wanted {
            text.0 = wanted.to_owned();
        }
        let enabled = if mo2 && label.0 == ControlButton::Detect {
            crate::mo2_settings::editable(&state.0)
        } else {
            label
                .0
                .available(&controls, &paths, has_manifest.0, game_running)
                && (!label.0.needs_folders() || source_ready)
        };
        let wanted_color = if enabled { TEXT_COLOR } else { BUTTON_OFF_TEXT };
        if color.0 != wanted_color {
            color.0 = wanted_color;
        }
    }
}

/// Keeps the notice pane's scroll position inside its content, after the layout has measured it.
///
/// [`ScrollArea`] moves the position by each mouse-wheel step and clamps the result, but the
/// layout only clamps what it draws, not the stored position. Left at [`SCROLL_TO_END`], the first
/// step up would be spent coming back from far past the end and the pane would not move; clamped
/// here, every step moves it.
pub fn clamp_notice_scroll(
    mut pane: Query<(&mut ScrollPosition, &ComputedNode), With<NoticePane>>,
) {
    for (mut scroll, computed) in &mut pane {
        let max = ((computed.content_size().y - computed.size().y)
            * computed.inverse_scale_factor())
        .max(0.0);
        if scroll.0.y > max {
            scroll.0.y = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::tests::temp_dir;
    use std::path::{Path, PathBuf};

    fn count<T: Component>(world: &mut World) -> usize {
        let mut query = world.query::<&T>();
        query.iter(world).count()
    }

    /// A Data folder that holds a `Skyrim.esm`, under the system temporary directory.
    fn data_folder(name: &str) -> PathBuf {
        let data = temp_dir(name);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("Skyrim.esm"), []).unwrap();
        data
    }

    /// Bevy refuses two queries in one system that could alias (B0001) only when the schedule is
    /// built, which no scene test reaches: run every panel system once in a headless app. The
    /// launcher's own test does the same for the whole window.
    #[test]
    fn the_panel_systems_run_without_conflicting_queries() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<FileDragAndDrop>()
            .init_resource::<GamePathConfig>()
            .init_resource::<ConversionStatus>()
            .init_resource::<CurrentConversion>()
            .init_resource::<PendingInputs>()
            .init_resource::<OutputReady>()
            .init_resource::<OutputHasManifest>()
            .add_systems(
                Update,
                (
                    click_controls,
                    accept_dropped_folder,
                    draw_paths,
                    draw_bar,
                    draw_labels,
                    draw_controls,
                ),
            )
            .add_systems(PostUpdate, clamp_notice_scroll);
        app.update();
    }

    /// The panel in a headless app that runs `draw_controls` each update, with a Data folder that
    /// holds a `Skyrim.esm`, so every button the table allows is drawn as available.
    fn panel_app(data: &Path) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::scene::ScenePlugin,
        ))
        .insert_resource(GamePathConfig {
            skyrim_data_path: Some(data.to_path_buf()),
            converted_assets_path: PathBuf::from("modern_assets"),
        })
        .init_resource::<CurrentConversion>()
        .init_resource::<OutputReady>()
        .insert_resource(OutputHasManifest(true))
        .add_systems(Update, draw_controls);
        app.world_mut()
            .spawn_scene(conversion_panel())
            .expect("the panel scene spawns");
        app
    }

    /// A long location path wraps onto more lines; the pane that holds the text must stay no
    /// taller than its bound and scroll instead, so the button row stays on screen.
    #[test]
    fn location_lines_sit_in_a_bounded_scroll_area() {
        let data = data_folder("locations-bounded");
        let mut app = panel_app(&data);
        let world = app.world_mut();
        let mut pane = world.query_filtered::<(&Node, &Children), With<OutputLocationsPane>>();
        let (node, children) = pane.single(world).expect("one locations pane");
        assert_eq!(node.max_height, Val::Px(LOCATIONS_MAX_HEIGHT));
        assert_eq!(node.overflow, Overflow::scroll_y());
        assert_eq!(node.flex_shrink, 0.0);
        let child = children[0];
        assert!(world.entity(child).contains::<OutputLocationsText>());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn completed_run_scrolls_to_summary_and_keeps_lod_warnings() {
        let data = data_folder("lod-result-scroll");
        let mut app = panel_app(&data);
        let mut status = ConversionStatus::default();
        let lines: Vec<_> = std::iter::once("Conversion complete".to_owned())
            .chain((0..12).map(|i| format!("LOD warning {i}")))
            .collect();
        status.finish_run(&lines);
        app.insert_resource(status).add_systems(Update, draw_labels);
        app.update();
        let world = app.world_mut();
        let mut pane = world.query_filtered::<&ScrollPosition, With<NoticePane>>();
        assert_eq!(pane.single(world).unwrap().0.y, 0.0);
        let mut text = world.query_filtered::<&Text, With<NoticeText>>();
        let result = &text.single(world).unwrap().0;
        assert!(result.starts_with("Conversion complete"));
        assert!(result.contains("LOD warning 0"));
        assert!(result.contains("LOD warning 11"));
    }

    /// Which buttons are drawn as available, read from their colours after a frame.
    fn drawn_available(app: &mut App) -> Vec<ControlButton> {
        app.update();
        let world = app.world_mut();
        let mut query = world.query::<(&ControlButton, &BackgroundColor)>();
        let mut available: Vec<ControlButton> = query
            .iter(world)
            .filter(|(_, background)| background.0 != BUTTON_OFF)
            .map(|(control, _)| *control)
            .collect();
        available.sort_by_key(|control| *control as u8);
        available
    }

    #[test]
    fn only_stop_is_on_while_a_check_runs() {
        let data = data_folder("buttons-check");
        let mut app = panel_app(&data);

        assert_eq!(
            drawn_available(&mut app),
            vec![
                ControlButton::Start,
                ControlButton::Detect,
                ControlButton::Check,
                ControlButton::FullCheck,
            ],
            "idle, with both folders chosen"
        );

        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Checking {
            mode: CheckMode::Quick,
            previous: Box::new(state::ConversionState::Stopped {
                staging: Some(PathBuf::from("modern_assets.staging-1")),
                cancelled: true,
            }),
        };
        assert_eq!(
            drawn_available(&mut app),
            vec![ControlButton::Stop],
            "a check leaves only Stop to press"
        );

        // The check over, the stopped run's Resume and Delete staging are back, and nothing more.
        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Stopped {
            staging: Some(PathBuf::from("modern_assets.staging-1")),
            cancelled: true,
        };
        assert_eq!(
            drawn_available(&mut app),
            vec![
                ControlButton::Start,
                ControlButton::Resume,
                ControlButton::DeleteStaging,
                ControlButton::Check,
                ControlButton::FullCheck,
            ]
        );

        // While the staging folder is deleted, nothing at all can be pressed.
        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Deleting {
            staging: PathBuf::from("modern_assets.staging-1"),
            cancelled: true,
        };
        assert_eq!(drawn_available(&mut app), Vec::<ControlButton>::new());

        // No manifest in the output folder: nothing to check against.
        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Idle;
        app.world_mut().insert_resource(OutputHasManifest(false));
        assert_eq!(
            drawn_available(&mut app),
            vec![ControlButton::Start, ControlButton::Detect]
        );

        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn start_says_convert_again_over_a_complete_output() {
        use state::ConversionState::{Idle, Running};
        assert_eq!(
            ControlButton::Start.label(&Idle, OutputReady(false)),
            "Start"
        );
        assert_eq!(
            ControlButton::Start.label(&Idle, OutputReady(true)),
            "Convert again"
        );
        // With a staging folder waiting, Start leaves it behind: it starts over.
        let waiting = state::ConversionState::Stopped {
            staging: Some(PathBuf::from("modern_assets.staging-1")),
            cancelled: true,
        };
        for ready in [OutputReady(false), OutputReady(true)] {
            assert_eq!(ControlButton::Start.label(&waiting, ready), "Start over");
        }
        // Once the staging folder is deleted (or none was kept) there is nothing to start over
        // from: Start, or Convert again over a complete output.
        let nothing_waiting = state::ConversionState::Stopped {
            staging: None,
            cancelled: true,
        };
        assert_eq!(
            ControlButton::Start.label(&nothing_waiting, OutputReady(false)),
            "Start"
        );
        assert_eq!(
            ControlButton::Start.label(&nothing_waiting, OutputReady(true)),
            "Convert again"
        );
        assert_eq!(
            ControlButton::Stop.label(&Running, OutputReady(true)),
            "Stop"
        );
    }

    #[test]
    fn the_check_buttons_ask_for_their_own_mode() {
        assert_eq!(ControlButton::Check.check_mode(), Some(CheckMode::Quick));
        assert_eq!(ControlButton::FullCheck.check_mode(), Some(CheckMode::Full));
        assert_eq!(ControlButton::Start.check_mode(), None);
    }

    #[test]
    fn the_panel_scene_spawns_every_widget() {
        // Spawning a scene needs the asset server's scene plumbing and nothing else: no window, no
        // renderer, no font server.
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::scene::ScenePlugin,
        ));
        let world = app.world_mut();
        world
            .spawn_scene(conversion_panel())
            .expect("the panel scene spawns");

        let mut query = world.query::<(&ControlButton, &Interaction, &Children)>();
        let mut controls: Vec<ControlButton> = Vec::new();
        for (control, _, children) in query.iter(world) {
            assert!(!children.is_empty(), "{control:?} has no label");
            controls.push(*control);
        }
        assert_eq!(controls.len(), 7, "buttons found: {controls:?}");
        for control in [
            ControlButton::Start,
            ControlButton::Stop,
            ControlButton::Resume,
            ControlButton::DeleteStaging,
            ControlButton::Detect,
            ControlButton::Check,
            ControlButton::FullCheck,
        ] {
            assert!(controls.contains(&control), "{control:?} is missing");
        }

        let mut labels = world.query::<(&ControlLabel, &Text)>();
        assert_eq!(labels.iter(world).count(), 7, "one label per button");

        // Every line and row the drawing systems update is on exactly one entity.
        assert_eq!(count::<DataPathText>(world), 1);
        assert_eq!(count::<OutputPathText>(world), 1);
        assert_eq!(count::<OutputLocationsText>(world), 1);
        assert_eq!(count::<BarFill>(world), 1);
        assert_eq!(count::<OverallText>(world), 1);
        assert_eq!(count::<StageText>(world), 1);
        assert_eq!(count::<ClockText>(world), 1);
        assert_eq!(count::<FileText>(world), 1);
        assert_eq!(count::<NoticeText>(world), 1);
        // The notices scroll rather than spill out of the panel.
        let mut pane = world.query_filtered::<&ScrollPosition, With<NoticePane>>();
        assert_eq!(pane.iter(world).count(), 1);
    }

    /// Only folders reach the path rows, and only while the folders may change.
    #[test]
    fn dropped_folders_fill_the_rows_and_files_do_not() {
        let data = data_folder("drop-data");
        let output = temp_dir("drop-output");
        std::fs::create_dir_all(&output).unwrap();
        let mods = temp_dir("drop-mods");
        std::fs::create_dir_all(&mods).unwrap();
        let plugin = mods.join("SomeMod.esp");
        std::fs::write(&plugin, []).unwrap();

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<FileDragAndDrop>()
            .init_resource::<GamePathConfig>()
            .init_resource::<ConversionStatus>()
            .init_resource::<CurrentConversion>()
            .add_systems(Update, accept_dropped_folder);
        let drop_path = |app: &mut App, path: &Path| {
            app.world_mut().write_message(FileDragAndDrop::DroppedFile {
                window: Entity::PLACEHOLDER,
                path_buf: path.to_path_buf(),
            });
            app.update();
        };

        drop_path(&mut app, &data);
        drop_path(&mut app, &output);
        drop_path(&mut app, &plugin);
        let paths = app.world().resource::<GamePathConfig>().clone();
        assert_eq!(paths.skyrim_data_path, Some(data.clone()));
        assert_eq!(
            paths.converted_assets_path,
            output.join("assets"),
            "a mod file is not an output"
        );

        // While a run is going the folders stay what they are.
        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Running;
        let other = temp_dir("drop-other");
        std::fs::create_dir_all(&other).unwrap();
        drop_path(&mut app, &other);
        assert_eq!(
            app.world()
                .resource::<GamePathConfig>()
                .converted_assets_path,
            output.join("assets")
        );
        assert!(
            app.world()
                .resource::<ConversionStatus>()
                .notice_text()
                .contains("The folders are fixed"),
            "{:?}",
            app.world().resource::<ConversionStatus>().notice_text()
        );

        for dir in [data, output, mods, other] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// An app with the panel's input systems and the state machine, so a press can be followed to
    /// the effect it asks for.
    fn input_app(paths: GamePathConfig) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, super::super::ConversionLogicPlugin))
            .add_message::<FileDragAndDrop>()
            .insert_resource(paths)
            .init_resource::<OutputHasManifest>()
            .add_systems(
                Update,
                (click_controls, accept_dropped_folder).before(super::super::process_inputs),
            );
        app
    }

    /// Presses `control` once and returns the effects the press asked for.
    fn press(app: &mut App, control: ControlButton) -> Vec<state::Effect> {
        app.world_mut().spawn((control, Interaction::Pressed));
        app.update();
        app.world_mut()
            .resource_mut::<super::super::PendingEffects>()
            .drain()
            .collect()
    }

    fn notices(app: &App) -> String {
        app.world().resource::<ConversionStatus>().notice_text()
    }

    /// A parent whose package child holds other things is refused: publishing would replace them.
    #[test]
    fn a_dropped_parent_with_an_unsafe_package_child_is_refused() {
        let data = data_folder("unsafe-drop-data");
        let games = temp_dir("unsafe-drop-games");
        let child = games.join("assets");
        std::fs::create_dir_all(child.join("SomeGame")).unwrap();
        std::fs::write(child.join("save.dat"), b"precious").unwrap();
        let before = PathBuf::from("modern_assets");
        let mut app = input_app(GamePathConfig {
            skyrim_data_path: Some(data.clone()),
            converted_assets_path: before.clone(),
        });

        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: games.clone(),
        });
        app.update();

        assert_eq!(
            app.world()
                .resource::<GamePathConfig>()
                .converted_assets_path,
            before,
            "the Output row keeps its old value"
        );
        assert!(
            notices(&app).contains(&format!(
                "{} is not empty and is not a Mudcrab conversion; choose an empty or new folder.",
                child.display()
            )),
            "{}",
            notices(&app)
        );
        assert!(child.join("save.dat").is_file(), "nothing was touched");

        for dir in [data, games] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn parent_selection_reaches_start_and_resume_without_replacing_the_parent() {
        for resume in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let parent = root.path().join("Mudcrab data");
            let output = parent.join("assets");
            let staging = parent.join("assets.staging-1-1");
            let cache = parent.join("assets.assets-cache");
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(parent.join("keep.txt"), b"parent contents").unwrap();
            if resume {
                std::fs::create_dir(&staging).unwrap();
            }
            let data = data_folder("parent-start-resume");
            let mut app = input_app(GamePathConfig {
                skyrim_data_path: Some(data.clone()),
                ..default()
            });
            app.init_resource::<OutputReady>().add_systems(
                Update,
                super::super::refresh_output_ready.after(super::super::process_inputs),
            );
            app.world_mut().write_message(FileDragAndDrop::DroppedFile {
                window: Entity::PLACEHOLDER,
                path_buf: parent.clone(),
            });
            app.update();
            if resume {
                assert!(matches!(
                    &app.world().resource::<CurrentConversion>().0,
                    state::ConversionState::Stopped { staging: Some(found), .. } if found == &staging
                ));
            } else {
                assert_eq!(
                    app.world().resource::<CurrentConversion>().0,
                    state::ConversionState::Idle,
                    "the cache alone is not resumable staging"
                );
            }
            let effects = press(
                &mut app,
                if resume {
                    ControlButton::Resume
                } else {
                    ControlButton::Start
                },
            );
            let [state::Effect::Begin(config)] = effects.as_slice() else {
                panic!("unexpected effects: {effects:?}");
            };
            assert_eq!(config.output_dir, output);
            assert_eq!(config.ingestion_cache_dir(), cache);
            assert_eq!(config.resume_staging, resume.then_some(staging));
            assert_eq!(
                std::fs::read(parent.join("keep.txt")).unwrap(),
                b"parent contents"
            );
            assert!(
                !output.exists(),
                "selection and input handling do not convert"
            );
            std::fs::remove_dir_all(data).unwrap();
        }
    }

    #[test]
    fn dropping_an_existing_package_preserves_its_exact_path() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("Skyrim.Converted");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join(MANIFEST_FILE), b"{}").unwrap();
        let mut app = input_app(GamePathConfig::default());
        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: package.clone(),
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<GamePathConfig>()
                .converted_assets_path,
            package
        );
        assert!(!package.join("assets").exists());
    }

    #[test]
    fn dropping_an_unfinished_legacy_output_finds_its_original_staging() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("Skyrim.Converted");
        let staging = root.path().join("Skyrim.Converted.staging-1-1");
        std::fs::create_dir(&output).unwrap();
        std::fs::create_dir(&staging).unwrap();
        let mut app = input_app(GamePathConfig::default());
        app.init_resource::<OutputReady>().add_systems(
            Update,
            super::super::refresh_output_ready.after(super::super::process_inputs),
        );
        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: output.clone(),
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<GamePathConfig>()
                .converted_assets_path,
            output
        );
        assert!(matches!(
            &app.world().resource::<CurrentConversion>().0,
            state::ConversionState::Stopped { staging: Some(found), .. } if found == &staging
        ));
    }

    /// The states Start, Start over and Resume are pressed in by the tests below.
    fn publishing_presses(staging: &Path) -> [(state::ConversionState, ControlButton); 3] {
        let stopped = || state::ConversionState::Stopped {
            staging: Some(staging.to_path_buf()),
            cancelled: true,
        };
        [
            (state::ConversionState::Idle, ControlButton::Start),
            // Start over: Start with a stopped run's staging folder waiting.
            (stopped(), ControlButton::Start),
            (stopped(), ControlButton::Resume),
        ]
    }

    /// The Output folder is looked at again on the press: one that was empty when it was chosen
    /// and has filled up since is refused, by Start, Start over and Resume alike.
    #[test]
    fn start_and_resume_refuse_an_output_that_holds_other_things() {
        let data = data_folder("unsafe-start-data");
        let output = temp_dir("unsafe-start-output");
        std::fs::create_dir_all(&output).unwrap();
        let mut app = input_app(GamePathConfig {
            skyrim_data_path: Some(data.clone()),
            converted_assets_path: output.clone(),
        });

        // Empty: Start converts.
        let effects = press(&mut app, ControlButton::Start);
        assert!(
            matches!(effects.as_slice(), [state::Effect::Begin(_)]),
            "{effects:?}"
        );

        // Files appeared after the folder was chosen: no press asks for a run.
        std::fs::write(output.join("notes.txt"), b"mine").unwrap();
        for (state, control) in publishing_presses(&output.with_extension("staging-1")) {
            app.world_mut().resource_mut::<CurrentConversion>().0 = state;
            let effects = press(&mut app, control);
            assert!(effects.is_empty(), "{control:?} asked for {effects:?}");
        }
        assert!(
            notices(&app).contains("is not empty and is not a Mudcrab conversion"),
            "{}",
            notices(&app)
        );
        assert!(output.join("notes.txt").is_file());

        // An earlier conversion is converted over.
        std::fs::write(output.join(MANIFEST_FILE), b"{}").unwrap();
        app.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Idle;
        let effects = press(&mut app, ControlButton::Start);
        assert!(
            matches!(effects.as_slice(), [state::Effect::Begin(_)]),
            "{effects:?}"
        );

        for dir in [data, output] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// A real engine process, already exited: the launcher keeps its resource until the watcher
    /// sees the exit, which these tests never run, so it stands for a running game.
    fn engine_process(name: &str) -> (EngineProcess, PathBuf) {
        let log = temp_dir(name).with_extension("log");
        #[cfg(windows)]
        let command = {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };
        let mut engine = EngineProcess::spawn(command, &log).expect("spawn the shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while engine.poll().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (engine, log)
    }

    /// While the game started by Play runs, a run could not publish over the files it has open:
    /// Start, Start over and Resume are drawn off and refused; the checks and Delete staging stay.
    #[test]
    fn start_and_resume_are_off_while_the_game_runs() {
        let data = data_folder("engine-data");
        let output = temp_dir("engine-output");
        let staging = output.with_extension("staging-1");
        let (engine, log) = engine_process("engine-log");
        let mut app = input_app(GamePathConfig {
            skyrim_data_path: Some(data.clone()),
            converted_assets_path: output.clone(),
        });
        app.insert_resource(engine);

        for (state, control) in publishing_presses(&staging) {
            app.world_mut().resource_mut::<CurrentConversion>().0 = state;
            let effects = press(&mut app, control);
            assert!(effects.is_empty(), "{control:?} asked for {effects:?}");
        }
        assert!(
            notices(&app).contains(GAME_RUNNING_NOTICE),
            "{}",
            notices(&app)
        );

        // Drawn off, while what does not publish stays on.
        let mut panel = panel_app(&data);
        let (engine, second_log) = engine_process("engine-log-2");
        panel.insert_resource(engine);
        assert_eq!(
            drawn_available(&mut panel),
            vec![
                ControlButton::Detect,
                ControlButton::Check,
                ControlButton::FullCheck,
            ],
            "idle with the game running"
        );
        panel.world_mut().resource_mut::<CurrentConversion>().0 = state::ConversionState::Stopped {
            staging: Some(staging),
            cancelled: true,
        };
        assert_eq!(
            drawn_available(&mut panel),
            vec![
                ControlButton::DeleteStaging,
                ControlButton::Check,
                ControlButton::FullCheck,
            ],
            "a stopped run with the game running"
        );
        // The game closed: Start is back.
        panel.world_mut().remove_resource::<EngineProcess>();
        assert!(drawn_available(&mut panel).contains(&ControlButton::Start));

        std::fs::remove_dir_all(&data).unwrap();
        for file in [log, second_log] {
            let _ = std::fs::remove_file(file);
        }
    }
}
