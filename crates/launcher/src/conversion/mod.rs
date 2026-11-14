//! The launcher's conversion screen: choose the Skyrim `Data` folder and the output folder,
//! Start / Stop / Resume, and watch a progress bar with the stage, the rate and the time left.
//! Check and Full check read a converted output against its manifest without converting anything.
//!
//! Nothing converts until Start (or Resume) is pressed: a full conversion writes tens of gigabytes
//! and takes a while, so it only ever starts on purpose. When the Output folder already holds a
//! complete conversion, [`OutputReady`] says so and the launcher's Play button is enabled.
//!
//! The module is two plugins. [`ConversionLogicPlugin`] is the state machine, the messages a run
//! sends back and the stop button, with no UI in it; [`ConversionPanelPlugin`] adds the panel's
//! controls, its drawing and the output check on top. The launcher spawns the panel's widgets as
//! part of its own window.

pub mod panel;
pub mod runner;
pub mod state;
pub mod status;

use crate::{LauncherSet, game_detection};
use bevy::prelude::*;
use converter::Cancellation;
use crossbeam_channel::Receiver;
use runner::RunMessage;
use state::{ConversionState, Effect, Input};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use status::ConversionStatus;

/// What the process exits with when a second Stop gives up on a run that is stopping, the same code
/// the command line exits with after a second Ctrl+C.
pub const QUIT_CODE: i32 = 130;

/// The file a converted output is checked against; Check and Full check need it.
pub const MANIFEST_FILE: &str = "conversion-manifest.json";

/// The two folders the launcher works with: where the Skyrim assets are, and where the converted
/// output goes. The engine is started on the output folder.
#[derive(Resource, Debug, Clone)]
pub struct GamePathConfig {
    pub skyrim_data_path: Option<PathBuf>,
    pub converted_assets_path: PathBuf,
}

impl Default for GamePathConfig {
    fn default() -> Self {
        Self {
            skyrim_data_path: None,
            converted_assets_path: PathBuf::from("modern_assets"),
        }
    }
}

impl GamePathConfig {
    /// Selects a parent for new conversions, while still accepting an existing package directly.
    /// Keep the resolved package in the existing field so restored settings and explicit callers
    /// retain their exact output path; no separate persistence or migration is needed.
    pub fn choose_data_folder(&mut self, folder: &Path) -> Result<(), String> {
        let folder = std::path::absolute(folder)
            .map_err(|error| format!("Cannot locate {}: {error}", folder.display()))?;
        let output = if folder.join(MANIFEST_FILE).is_file()
            || converter::find_resumable_staging(&folder).is_some()
        {
            folder
        } else {
            folder.join("assets")
        };
        output_is_safe_target(&output)?;
        self.converted_assets_path = output;
        Ok(())
    }

    /// The parent containing the package and its sibling conversion workspaces.
    pub fn data_folder_label(&self) -> String {
        if self.converted_assets_path.as_os_str().is_empty() {
            return "not chosen - drop a parent folder here".into();
        }
        self.converted_assets_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .display()
            .to_string()
    }

    /// Actual package/cache paths and the converter's sibling staging-name pattern.
    pub fn locations_label(&self) -> String {
        let output = &self.converted_assets_path;
        if output.as_os_str().is_empty() {
            return "Package, staging and ingestion cache: choose a Mudcrab data folder first."
                .into();
        }
        let mut staging_name = output.file_name().unwrap_or_default().to_os_string();
        staging_name.push(".staging-*");
        let staging = output.with_file_name(staging_name);
        let cache = converter::PipelineConfig::new("", output).ingestion_cache_dir();
        format!(
            "Package: {}\nStaging (kept for Resume): {}\nIngestion cache: {}",
            self.output_label(),
            staging.display(),
            cache.display()
        )
    }

    /// The folders a run would use, or `None` when the Data folder is missing or does not hold a
    /// `Skyrim.esm` (which is what makes it a Skyrim `Data` folder) or the output is empty.
    pub fn pair(&self) -> Option<(PathBuf, PathBuf)> {
        let data = self.skyrim_data_path.as_ref()?;
        if !game_detection::is_skyrim_data_dir(data) {
            return None;
        }
        (!self.converted_assets_path.as_os_str().is_empty())
            .then(|| (data.clone(), self.converted_assets_path.clone()))
    }

    /// Whether a run could start with these folders.
    pub fn ready(&self) -> bool {
        self.pair().is_some()
    }

    /// Whether the Data row holds a Skyrim `Data` folder.
    pub fn has_data(&self) -> bool {
        self.skyrim_data_path
            .as_deref()
            .is_some_and(game_detection::is_skyrim_data_dir)
    }

    /// What the Data row shows.
    pub fn data_label(&self) -> String {
        match &self.skyrim_data_path {
            Some(path) if game_detection::is_skyrim_data_dir(path) => path.display().to_string(),
            Some(path) => format!("{} (no Skyrim.esm here)", path.display()),
            None => "not found - drop your Skyrim Data folder here".to_owned(),
        }
    }

    /// What the Output row shows.
    pub fn output_label(&self) -> String {
        if self.converted_assets_path.as_os_str().is_empty() {
            "not chosen - drop a folder here".to_owned()
        } else {
            self.converted_assets_path.display().to_string()
        }
    }
}

/// The state machine, as a resource: the machine itself is in [`state`], which knows nothing of
/// Bevy.
#[derive(Resource, Debug, Default)]
pub struct CurrentConversion(pub ConversionState);

/// Whether the Output folder holds a complete conversion the engine can start on (see
/// [`output_is_complete`]). Play is enabled only while this is true and no run is going.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutputReady(pub bool);

/// Whether the Output folder holds a [`MANIFEST_FILE`], which is what Check and Full check read.
/// Looked at when [`OutputReady`] is, so the buttons do not touch the disk every frame.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutputHasManifest(pub bool);

/// The inputs the panel's controls produced this frame, waiting for the state machine.
#[derive(Resource, Debug, Default)]
pub struct PendingInputs(Vec<Input>);

impl PendingInputs {
    pub fn push(&mut self, input: Input) {
        self.0.push(input);
    }
}

/// The effects the state machine asked for, waiting to be carried out. The logic plugin leaves
/// them here; [`ConversionPanelPlugin`] drains them, so a headless test can read what a press asked
/// for without starting anything.
#[derive(Resource, Debug, Default)]
pub struct PendingEffects(Vec<Effect>);

impl PendingEffects {
    pub fn push(&mut self, effect: Effect) {
        self.0.push(effect);
    }

    /// What has been asked for and not yet carried out.
    #[cfg(test)]
    pub fn pending(&self) -> &[Effect] {
        &self.0
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Effect> + '_ {
        self.0.drain(..)
    }
}

/// The messages a running conversion sends back, best read once a frame.
#[derive(Resource, Debug)]
pub struct RunChannel {
    pub receiver: Receiver<RunMessage>,
}

/// The stop buttons of the running conversion and of the running check, kept so the Stop button
/// can reach them.
#[derive(Resource, Default)]
pub struct RunHandle {
    pub cancellation: Option<Cancellation>,
    pub check_cancel: Option<Arc<AtomicBool>>,
}

/// The state machine, the running conversion's messages and the status they feed, with no UI in
/// sight. [`ConversionPanelPlugin`] adds the panel itself.
pub struct ConversionLogicPlugin;

impl Plugin for ConversionLogicPlugin {
    /// Registers conversion and source-selection resources and orders the conversion logic systems.
    fn build(&self, app: &mut App) {
        app.init_resource::<ConversionStatus>()
            .init_resource::<GamePathConfig>()
            .init_resource::<crate::mo2_settings::SourceSettings>()
            .init_resource::<CurrentConversion>()
            .init_resource::<PendingInputs>()
            .init_resource::<PendingEffects>()
            .init_resource::<RunHandle>()
            .add_systems(
                Update,
                (process_inputs, drain_run_messages, tick_status)
                    .chain()
                    .in_set(LauncherSet::Logic),
            );
    }
}

/// The conversion panel: the logic above, the check of the output folder that gates Play, and the
/// buttons, path rows and drawing systems of the panel. The widgets themselves are spawned with the
/// launcher's scene ([`panel::conversion_panel`]).
pub struct ConversionPanelPlugin;

impl Plugin for ConversionPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ConversionLogicPlugin)
            .init_resource::<OutputReady>()
            .init_resource::<OutputHasManifest>()
            .add_systems(Startup, panel::detect_skyrim_at_start)
            .add_systems(
                Update,
                refresh_output_ready
                    .in_set(LauncherSet::Logic)
                    .after(tick_status),
            )
            .add_systems(
                Update,
                (panel::click_controls, panel::accept_dropped_folder).in_set(LauncherSet::Input),
            )
            .add_systems(Update, actuate_effects.in_set(LauncherSet::Actuate))
            .add_systems(
                Update,
                (
                    panel::draw_paths,
                    panel::draw_bar,
                    panel::draw_labels,
                    panel::draw_controls,
                )
                    .in_set(LauncherSet::Draw),
            )
            .add_systems(
                PostUpdate,
                panel::clamp_notice_scroll.after(bevy::ui::UiSystems::Layout),
            );
    }
}

/// Folds this frame's inputs into the state, queueing whatever the state machine asks the runner
/// to do.
pub fn process_inputs(
    mut inputs: ResMut<PendingInputs>,
    mut state: ResMut<CurrentConversion>,
    mut effects: ResMut<PendingEffects>,
) {
    for input in std::mem::take(&mut inputs.0) {
        transition(&mut state, &mut effects, input);
    }
}

/// Folds a running conversion's messages into the status and the state.
pub fn drain_run_messages(
    channel: Option<Res<RunChannel>>,
    mut status: ResMut<ConversionStatus>,
    mut state: ResMut<CurrentConversion>,
    mut effects: ResMut<PendingEffects>,
) {
    let Some(channel) = channel else {
        return;
    };
    while let Ok(message) = channel.receiver.try_recv() {
        match message {
            // Progress moves the numbers; the table keeps a running run running.
            RunMessage::Progress(event) => {
                status.observe(&event);
                transition(&mut state, &mut effects, Input::Progress);
            }
            RunMessage::Finished(report) => {
                status.finish_run(&report.lines());
                transition(&mut state, &mut effects, Input::Finished(report));
            }
            RunMessage::Failed(failure) => {
                status.stop_clock();
                status.push_notice(&failure.headline());
                status.push_notice(&failure.staging_line());
                transition(&mut state, &mut effects, Input::Failed(failure));
            }
            RunMessage::CheckProgress { done, total } => status.observe_check(done, total),
            RunMessage::CheckFinished(summary) => {
                status.finish_check(&summary.lines(), true);
                transition(&mut state, &mut effects, Input::CheckFinished(summary));
            }
            RunMessage::CheckFailed(message) => {
                status.finish_check(&[state::check_failure_line(&message)], false);
                transition(&mut state, &mut effects, Input::CheckFailed(message));
            }
            RunMessage::CheckCancelled => {
                status.cancel_check();
                transition(&mut state, &mut effects, Input::CheckCancelled);
            }
            RunMessage::StagingDeleted { staging, result } => {
                status.finish_delete(&staging, &result);
                let input = match result {
                    Ok(()) => Input::StagingDeleted,
                    Err(error) => Input::DeleteFailed(error),
                };
                transition(&mut state, &mut effects, input);
            }
        }
    }
}

/// Keeps the elapsed clock and the time-left estimate moving between progress events.
pub fn tick_status(mut status: ResMut<ConversionStatus>) {
    status.tick();
}

/// Runs one input through the state machine and queues the effect it asked for.
fn transition(state: &mut CurrentConversion, effects: &mut PendingEffects, input: Input) {
    let (next, effect) = state::apply(state.0.clone(), input);
    state.0 = next;
    if !matches!(effect, Effect::None) {
        effects.push(effect);
    }
}

/// Carries out what the state machine asked for: start a conversion, stop it, delete a staging
/// folder, give up on a run that is stopping, or start or stop a check of the output folder.
pub fn actuate_effects(
    mut commands: Commands,
    mut effects: ResMut<PendingEffects>,
    mut status: ResMut<ConversionStatus>,
    mut handle: ResMut<RunHandle>,
    source: Res<crate::mo2_settings::SourceSettings>,
) {
    for effect in effects.drain() {
        match effect {
            Effect::None => {}
            Effect::Begin(mut config) => {
                let (tx, rx) = crossbeam_channel::unbounded();
                match source.apply(&mut config) {
                    Ok(()) => handle.cancellation = Some(runner::spawn(*config, tx)),
                    Err(reason) => {
                        let _ = tx.send(RunMessage::Failed(state::FailureReport::before_start(
                            reason,
                        )));
                    }
                }
                commands.insert_resource(RunChannel { receiver: rx });
                status.begin_run();
            }
            Effect::Cancel => match &handle.cancellation {
                Some(cancellation) => cancellation.cancel(),
                None => status.push_notice("There is no conversion to stop."),
            },
            Effect::Quit => std::process::exit(QUIT_CODE),
            // A staging folder is tens of gigabytes: deleting it on this thread would freeze the
            // window until it was gone.
            Effect::DeleteStaging { staging } => {
                let (tx, rx) = crossbeam_channel::unbounded();
                status.begin_delete(&staging);
                runner::spawn_delete(staging, tx);
                commands.insert_resource(RunChannel { receiver: rx });
            }
            Effect::BeginCheck { output, mode } => {
                let (tx, rx) = crossbeam_channel::unbounded();
                let cancel = Arc::new(AtomicBool::new(false));
                handle.check_cancel = Some(Arc::clone(&cancel));
                status.begin_check(mode, &output);
                runner::spawn_check(output, mode, cancel, tx);
                commands.insert_resource(RunChannel { receiver: rx });
            }
            Effect::CancelCheck => match &handle.check_cancel {
                Some(cancel) => cancel.store(true, Ordering::Relaxed),
                None => status.push_notice("There is no check to stop."),
            },
        }
    }
}

/// Whether a conversion may publish into `output`, or why not, as a line for the notice pane.
///
/// Publishing replaces the whole Output folder: the old folder is renamed aside and then deleted.
/// So the launcher only ever converts into a folder that is safe to lose: one that does not exist
/// yet, an empty one, or one that holds a [`MANIFEST_FILE`] (an earlier conversion). Anything else,
/// such as a games or documents folder dropped by mistake, is refused, as is a file or a path that
/// cannot be read. The drop into the Output row and every Start, Start over and Resume press ask
/// this, so a folder that filled up after it was chosen is refused at the press.
///
/// The rule is the converter's own ([`converter::check_output_dir`], which the pipeline also
/// applies before it starts and before it publishes); this only words its answer for the pane.
pub fn output_is_safe_target(output: &Path) -> Result<(), String> {
    use converter::OutputDirError;
    converter::check_output_dir(output).map_err(|error| match error {
        OutputDirError::NotConverterOutput(path) => format!(
            "{} is not empty and is not a Mudcrab conversion; choose an empty or new folder.",
            path.display()
        ),
        OutputDirError::NotADirectory(path) => format!(
            "{} is not a folder; choose an empty or new folder.",
            path.display()
        ),
        OutputDirError::Unreadable { path, source } => format!(
            "{} cannot be read ({source}); choose an empty or new folder.",
            path.display()
        ),
    })
}

/// Whether `output` holds a complete conversion the engine can start on: a manifest that says it is
/// complete at a converter schema the engine loads (from [`shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION`]
/// through this converter's), the world database and cell cache beside it, and an integration
/// report that passed at a world database schema the engine reads
/// ([`shared::supports_runtime_world_database_schema`]).
///
/// The manifest is read as written: [`converter::cache::ConversionManifest::load`] marks an older
/// schema's manifest incomplete, because the next conversion rebuilds from it, but the engine still
/// starts on that output.
///
/// Database schema and advertised LOD identity are checked by the runtime's shared read-only
/// validator, and the database schema must match the integration report. Check and Full check
/// additionally verify individual artifact bytes.
pub fn output_is_complete(output: &Path) -> bool {
    std::fs::read(output.join(MANIFEST_FILE))
        .ok()
        .and_then(|bytes| {
            serde_json::from_slice::<converter::cache::ConversionManifest>(&bytes).ok()
        })
        .is_some_and(|manifest| {
            manifest.complete
                && manifest.failures.is_empty()
                && (shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION
                    ..=converter::cache::CONVERTER_SCHEMA_VERSION)
                    .contains(&manifest.schema_version)
                && output.join("cell_cache.rkyv").is_file()
                && std::fs::read(output.join("integration-report.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .is_some_and(|report| {
                        report.get("passed").and_then(serde_json::Value::as_bool) == Some(true)
                            && report
                                .get("schema_version")
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|version| u32::try_from(version).ok())
                                .is_some_and(|schema| {
                                    shared::world_assets::validate_lod_build_contract(
                                        output,
                                        shared::LOD_CONVERTER_SCHEMA_VERSION,
                                    )
                                    .is_ok_and(|actual| actual == schema)
                                })
                    })
        })
}

/// Keeps [`OutputReady`] and [`OutputHasManifest`] up to date. The check reads the output's
/// manifest, which is large for a full install, so it runs only when there is something new to look
/// at: on the first frame, when the Output folder changes, and when a run ends.
///
/// On the first frame and when the Output folder changes, an output that is not complete is also
/// looked beside for a staging folder an earlier session left, and the newest one is offered for
/// Resume (see [`offer_leftover_staging`]).
pub fn refresh_output_ready(
    config: Res<GamePathConfig>,
    mut state: ResMut<CurrentConversion>,
    mut ready: ResMut<OutputReady>,
    mut has_manifest: ResMut<OutputHasManifest>,
    mut status: ResMut<ConversionStatus>,
    mut checked: Local<Option<PathBuf>>,
    mut was_running: Local<bool>,
) {
    let running = matches!(
        state.0,
        ConversionState::Running | ConversionState::Stopping
    );
    let run_ended = *was_running && !running;
    *was_running = running;
    let output = &config.converted_assets_path;
    if !run_ended && checked.as_deref() == Some(output.as_path()) {
        return;
    }
    *checked = Some(output.clone());

    let chosen = !output.as_os_str().is_empty();
    let manifest = chosen && output.join(MANIFEST_FILE).is_file();
    if has_manifest.0 != manifest {
        has_manifest.0 = manifest;
    }
    let complete = chosen && output_is_complete(output);
    if ready.0 != complete {
        ready.0 = complete;
    }
    let finished_complete =
        matches!(&state.0, ConversionState::Finished(report) if report.complete);
    if run_ended && !finished_complete {
        // A run that stopped short left the output as it was, and the pane has said why.
        return;
    }
    if complete {
        status.push_notice(&format!(
            "{} holds a complete conversion: Play is ready.",
            output.display()
        ));
    } else if run_ended {
        status.push_notice(
            "The run finished, but the output does not pass the launcher's check (manifest, world database, cell cache and a passed integration report), so Play stays off.",
        );
    } else if chosen && !offer_leftover_staging(&mut state.0, &mut status, output) {
        status.push_notice(&format!(
            "No complete conversion in {} yet: Start converts one.",
            output.display()
        ));
    }
}

/// Looks beside `output` for a staging folder an earlier session left behind (a run whose process
/// ended before it could publish) and, when the state machine takes it, offers the newest one for
/// Resume and says so in the pane. Older ones are counted and left where they are; nothing is
/// deleted. Returns whether a folder was offered.
fn offer_leftover_staging(
    state: &mut ConversionState,
    status: &mut ConversionStatus,
    output: &Path,
) -> bool {
    let Some((staging, older)) = converter::find_resumable_staging(output) else {
        return false;
    };
    let (next, effect) = state::apply(
        state.clone(),
        Input::FoundStaging {
            staging: staging.clone(),
        },
    );
    debug_assert!(matches!(effect, Effect::None), "{effect:?}");
    if next == *state {
        return false;
    }
    *state = next;
    let name = staging.file_name().map_or_else(
        || staging.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut line = format!(
        "An unfinished conversion was found in {name}. Resume continues where it stopped; finished files are checked again, not redone. Delete staging removes it."
    );
    if older > 0 {
        let _ = write!(
            line,
            " {older} older unfinished conversion(s) beside it are left untouched."
        );
    }
    status.push_notice(&line);
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bevy::MinimalPlugins;
    use converter::{CheckMode, ProgressEvent, ProgressStage};
    use crossbeam_channel::unbounded;
    use state::{FailureReport, RunReport};
    use std::time::{Duration, Instant};

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(ConversionLogicPlugin);
        app
    }

    /// An invalid MO2 source reports a before-start failure through the boxed Begin effect.
    #[test]
    fn invalid_mo2_source_fails_before_spawning_conversion() {
        let mut app = test_app();
        app.add_systems(Update, actuate_effects.after(LauncherSet::Logic));
        app.world_mut()
            .resource_mut::<crate::mo2_settings::SourceSettings>()
            .use_mo2 = true;
        let (data, output) = folders();
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Start { data, output });
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Stopped {
                staging: None,
                cancelled: false,
            }
        );
        assert!(app.world().resource::<RunHandle>().cancellation.is_none());
        assert!(
            app.world()
                .resource::<ConversionStatus>()
                .notice_text()
                .contains("Choose a valid MO2 instance and profile")
        );
    }

    /// The folders the tests run with, and a Start press that puts the window in `Running`.
    fn folders() -> (PathBuf, PathBuf) {
        (PathBuf::from("C:/Skyrim/Data"), PathBuf::from("C:/out"))
    }

    fn start(app: &mut App) {
        let (data, output) = folders();
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Start { data, output });
        app.update();
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Running
        );
        // Carry the effect out, as the window does every frame, so the next test reads only its
        // own frame's effects.
        let _ = take_effects(app);
    }

    /// The effects this frame queued, drained the way the window's own system drains them.
    fn take_effects(app: &mut App) -> Vec<Effect> {
        app.world_mut()
            .resource_mut::<PendingEffects>()
            .drain()
            .collect()
    }

    fn push_message(
        app: &mut App,
        tx: &crossbeam_channel::Sender<RunMessage>,
        message: RunMessage,
    ) {
        tx.send(message).expect("the window is reading");
        app.update();
    }

    /// A progress event whose whole-run fraction is `overall`, whatever its counts say.
    fn progress(overall: f32) -> ProgressEvent {
        let mut event = ProgressEvent::new(ProgressStage::Textures, 1, 10, None, "converting");
        event.overall = overall;
        event
    }

    /// The design's first headless test: the bar holds its position when a stage reports itself
    /// short, as the command line's status line does.
    #[test]
    fn the_bar_never_moves_backwards() {
        let mut app = test_app();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });

        let mut seen = 0.0f32;
        for overall in [0.2, 0.5, 0.3, 0.5, 0.44] {
            push_message(&mut app, &tx, RunMessage::Progress(progress(overall)));
            let status = app.world().resource::<ConversionStatus>();
            assert!(
                status.overall_percent() >= seen,
                "the bar moved back from {seen} to {}",
                status.overall_percent()
            );
            seen = status.overall_percent();
        }
        assert_eq!(seen, 50.0, "the bar stopped at the highest fraction seen");
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Running,
            "progress does not end a run"
        );
    }

    /// The design's second headless test: a failure that kept a staging folder is resumable.
    #[test]
    fn a_failed_run_with_a_kept_staging_folder_offers_resume() {
        let mut app = test_app();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });

        let staging = PathBuf::from("C:/out.staging-20260928");
        push_message(
            &mut app,
            &tx,
            RunMessage::Failed(FailureReport {
                message: "the disk went away".into(),
                staging: Some(staging.clone()),
                cancelled: false,
            }),
        );

        let state = app.world().resource::<CurrentConversion>().0.clone();
        assert_eq!(
            state,
            ConversionState::Stopped {
                staging: Some(staging.clone()),
                cancelled: false,
            }
        );
        let controls = state::controls(&state);
        assert!(controls.resume, "a kept staging folder must offer Resume");
        assert!(controls.delete_staging);
        let status = app.world().resource::<ConversionStatus>();
        assert!(
            status.notice_text().contains("Conversion failed"),
            "the failure is not readable in the pane: {:?}",
            status.notice_text()
        );
        assert!(
            status.notice_text().contains("the disk went away"),
            "{:?}",
            status.notice_text()
        );
    }

    /// The design's third headless test: Resume asks for exactly the resumable configuration,
    /// and the test never starts a run (nothing in the logic plugin does).
    #[test]
    fn resume_asks_for_a_run_that_carries_on_from_the_staging_folder() {
        let mut app = test_app();
        let (data, output) = folders();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        let staging = PathBuf::from("C:/out.staging-20260928");
        push_message(
            &mut app,
            &tx,
            RunMessage::Failed(FailureReport {
                message: "stopped".into(),
                staging: Some(staging.clone()),
                cancelled: true,
            }),
        );

        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Resume {
                data: data.clone(),
                output: output.clone(),
            });
        app.update();

        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Running
        );
        let effects = take_effects(&mut app);
        let [Effect::Begin(config)] = effects.as_slice() else {
            panic!("Resume asked for {effects:?}");
        };
        assert_eq!(config.resume_staging, Some(staging));
        assert_eq!(config.data_dir, data);
        assert_eq!(config.output_dir, output);
    }

    /// Nothing in the logic plugin starts a conversion: a Start press only queues the effect, which
    /// is what makes the other tests safe to run beside a real pipeline.
    #[test]
    fn a_start_press_only_queues_the_configuration() {
        let mut app = test_app();
        let (data, output) = folders();
        // Start by hand rather than through the helper, so this test sees the frame's effect.
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Start {
                data: data.clone(),
                output: output.clone(),
            });
        app.update();

        assert!(
            app.world().get_resource::<RunChannel>().is_none(),
            "the logic plugin started a run by itself"
        );
        let effects = take_effects(&mut app);
        let [Effect::Begin(config)] = effects.as_slice() else {
            panic!("Start asked for {effects:?}");
        };
        assert_eq!(config.resume_staging, None);
        assert_eq!(config.data_dir, data);
        assert_eq!(config.output_dir, output);
        assert_eq!(
            config.record_reader,
            converter::config::RecordReader::Inhouse
        );
        assert_eq!(
            app.world().resource::<ConversionStatus>().overall_percent(),
            0.0,
            "a run that has not reported anything shows an empty bar"
        );
    }

    /// The controls the table disables do nothing, however they are pressed.
    #[test]
    fn a_press_the_table_forbids_changes_nothing() {
        let mut app = test_app();
        // Idle: Stop, Resume and Delete staging are all off.
        for input in [
            Input::Stop,
            Input::Resume {
                data: PathBuf::from("C:/Skyrim/Data"),
                output: PathBuf::from("C:/out"),
            },
            Input::DeleteStaging,
        ] {
            app.world_mut().resource_mut::<PendingInputs>().push(input);
        }
        app.update();
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        assert!(
            app.world()
                .resource::<PendingEffects>()
                .pending()
                .is_empty(),
            "a forbidden press asked for {:?}",
            app.world().resource::<PendingEffects>().pending()
        );
    }

    /// A notice is for the pane, not the bar, and the pane keeps only the last few lines.
    #[test]
    fn notices_fill_the_pane_without_moving_the_bar() {
        let mut app = test_app();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        push_message(&mut app, &tx, RunMessage::Progress(progress(0.4)));
        let bar = app.world().resource::<ConversionStatus>().overall_percent();

        for index in 0..ConversionStatus::NOTICE_LINES + 3 {
            push_message(
                &mut app,
                &tx,
                RunMessage::Progress(ProgressEvent::notice(
                    ProgressStage::Textures,
                    None,
                    &format!("warning {index}"),
                )),
            );
        }
        let status = app.world().resource::<ConversionStatus>();
        assert_eq!(status.overall_percent(), bar, "a notice moved the bar");
        assert_eq!(status.notices.len(), ConversionStatus::NOTICE_LINES);
        assert!(
            status.notice_text().contains("warning 7"),
            "{:?}",
            status.notice_text()
        );
        assert!(
            !status.notice_text().contains("warning 0"),
            "{:?}",
            status.notice_text()
        );
    }

    /// A finished run shows its summary and the state says so.
    #[test]
    fn a_finished_run_shows_its_summary() {
        let mut app = test_app();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        let report = RunReport {
            complete: true,
            converted: 12,
            cache_hits: 3,
            skipped: 0,
            warnings: Vec::new(),
            notices: Vec::new(),
            lod_chunks: 1693,
            lod_warnings: (0..12).map(|i| format!("Skipped world {i}")).collect(),
            artifacts: 15,
            elapsed: Duration::from_secs(754),
        };
        push_message(&mut app, &tx, RunMessage::Finished(report.clone()));

        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Finished(report)
        );
        let status = app.world().resource::<ConversionStatus>();
        assert!(
            status
                .notice_text()
                .contains("Conversion complete in 0:12:34.0"),
            "{:?}",
            status.notice_text()
        );
        assert!(
            status
                .notice_text()
                .contains("Terrain LOD: 1693 chunks; 12 worldspace warning(s).")
        );
        assert!(status.notice_text().contains("LOD: Skipped world 0"));
        assert!(status.notice_text().contains("LOD: Skipped world 11"));
        assert_eq!(status.notices.len(), 15);
        assert!(status.run_finished);
    }

    /// The folders a run needs: a Data folder with a `Skyrim.esm` in it, and an output that is set.
    #[test]
    fn the_folders_a_run_needs_are_a_data_folder_and_an_output() {
        let mut paths = GamePathConfig::default();
        assert!(!paths.ready(), "an unset Data folder cannot start a run");
        assert!(paths.data_label().contains("not found"));

        let dir = temp_dir("ready");
        std::fs::create_dir_all(&dir).unwrap();
        paths.skyrim_data_path = Some(dir.clone());
        assert!(
            !paths.ready(),
            "a folder without Skyrim.esm is not a Data folder"
        );
        assert!(paths.data_label().contains("no Skyrim.esm"));

        std::fs::write(dir.join("Skyrim.esm"), []).unwrap();
        assert!(paths.ready());
        assert_eq!(
            paths.pair(),
            Some((dir.clone(), PathBuf::from("modern_assets")))
        );

        paths.converted_assets_path = PathBuf::new();
        assert!(!paths.ready(), "an empty output cannot start a run");
        assert!(paths.output_label().contains("not chosen"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn location_labels_follow_parent_selection_and_explicit_package_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = GamePathConfig::default();
        paths.choose_data_folder(root.path()).unwrap();
        let output = root.path().join("assets");
        assert_eq!(paths.converted_assets_path, output);
        assert_eq!(paths.data_folder_label(), root.path().display().to_string());
        assert_eq!(
            paths.locations_label(),
            format!(
                "Package: {}\nStaging (kept for Resume): {}\nIngestion cache: {}",
                output.display(),
                root.path().join("assets.staging-*").display(),
                root.path().join("assets.assets-cache").display()
            )
        );
        // An explicit output (including a restored choice) is already a package, not a parent.
        paths.converted_assets_path = root.path().join("Skyrim.Converted");
        assert!(
            paths
                .locations_label()
                .contains("Skyrim.Converted.staging-*")
        );
        assert!(
            paths
                .locations_label()
                .contains("Skyrim.Converted.assets-cache")
        );
        assert!(!paths.locations_label().contains("Skyrim.Converted/assets"));
        paths.converted_assets_path = PathBuf::from("modern_assets");
        assert_eq!(paths.data_folder_label(), ".");
        assert!(
            paths
                .locations_label()
                .starts_with("Package: modern_assets\n")
        );
        paths.converted_assets_path.clear();
        assert!(
            paths
                .locations_label()
                .contains("choose a Mudcrab data folder first")
        );
        assert!(paths.data_folder_label().contains("not chosen"));
    }

    /// Publishing deletes what the Output folder held, so only a missing folder, an empty one or an
    /// earlier conversion (a folder with a manifest) may be converted into.
    #[test]
    fn only_a_new_empty_or_converted_folder_is_a_safe_output() {
        let root = temp_dir("safe-output");
        std::fs::create_dir_all(&root).unwrap();

        let missing = root.join("not-there-yet");
        assert_eq!(output_is_safe_target(&missing), Ok(()), "a new folder");

        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(output_is_safe_target(&empty), Ok(()), "an empty folder");

        let converted = root.join("converted");
        std::fs::create_dir_all(converted.join("meshes")).unwrap();
        std::fs::write(converted.join("meshes").join("a.glb"), b"glTF").unwrap();
        std::fs::write(converted.join(MANIFEST_FILE), b"{}").unwrap();
        assert_eq!(
            output_is_safe_target(&converted),
            Ok(()),
            "an earlier conversion"
        );

        let games = root.join("Games");
        std::fs::create_dir_all(games.join("SomeGame")).unwrap();
        std::fs::write(games.join("save.dat"), b"precious").unwrap();
        let refused = output_is_safe_target(&games).expect_err("a folder of other things");
        assert_eq!(
            refused,
            format!(
                "{} is not empty and is not a Mudcrab conversion; choose an empty or new folder.",
                games.display()
            )
        );

        // A manifest-named folder is not a manifest.
        let odd = root.join("odd");
        std::fs::create_dir_all(odd.join(MANIFEST_FILE)).unwrap();
        assert!(output_is_safe_target(&odd).is_err());

        let file = root.join("a-file.txt");
        std::fs::write(&file, b"not a folder").unwrap();
        let refused = output_is_safe_target(&file).expect_err("a file");
        assert!(refused.contains("is not a folder"), "{refused}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A unique folder under the system temporary directory, as the launcher's tests do.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mudcrab-launcher-{name}-{unique}"))
    }

    /// A converted output in miniature: one artifact, a world database and a manifest written by
    /// hand, plus, when `missing` is set, a manifest entry whose artifact is not there.
    pub(crate) fn tiny_output(name: &str, missing: bool) -> PathBuf {
        let output = temp_dir(name);
        std::fs::create_dir_all(output.join("meshes")).unwrap();
        std::fs::write(output.join("meshes").join("a.glb"), b"glTF!").unwrap();
        std::fs::write(output.join("skyrim_world.db"), b"").unwrap();
        let hash = converter::cache::hash_file(&output.join("meshes").join("a.glb")).unwrap();
        let mut entries = format!(
            r#""meshes/a.nif": {{"source_hash": "s", "output": "meshes/a.glb", "output_size": 5, "output_hash": "{hash}"}}"#
        );
        if missing {
            entries.push_str(
                r#", "meshes/b.nif": {"source_hash": "s", "output": "meshes/b.glb", "output_size": 7, "output_hash": "h"}"#,
            );
        }
        let manifest = format!(
            r#"{{"schema_version": {}, "complete": true, "entries": {{{entries}}}}}"#,
            converter::cache::CONVERTER_SCHEMA_VERSION
        );
        std::fs::write(output.join("conversion-manifest.json"), manifest).unwrap();
        output
    }

    /// A converted output the launcher can start the engine on: [`tiny_output`] plus the cell
    /// cache and an integration report that passed.
    pub(crate) fn complete_output(name: &str) -> PathBuf {
        let output = tiny_output(name, false);
        write_world_database(&output, shared::WORLD_DATABASE_SCHEMA_VERSION);
        std::fs::write(output.join("cell_cache.rkyv"), b"").unwrap();
        write_integration_report(&output, true);
        output
    }

    fn write_world_database(output: &Path, version: u32) {
        let path = output.join("skyrim_world.db");
        let _ = std::fs::remove_file(&path);
        let database = rusqlite::Connection::open(path).unwrap();
        database.execute_batch(&format!(
            "CREATE TABLE schema_info(version INTEGER NOT NULL); INSERT INTO schema_info VALUES({version});"
        )).unwrap();
        if version >= shared::WORLD_DATABASE_LOD_SCHEMA_VERSION {
            database.execute_batch("CREATE TABLE lod_chunks(id INTEGER PRIMARY KEY); CREATE TABLE lod_build(id INTEGER PRIMARY KEY,build_identity TEXT NOT NULL);").unwrap();
        }
    }

    fn write_integration_report(output: &Path, passed: bool) {
        let report = format!(
            r#"{{"passed": {passed}, "schema_version": {}}}"#,
            shared::WORLD_DATABASE_SCHEMA_VERSION
        );
        std::fs::write(output.join("integration-report.json"), report).unwrap();
    }

    /// Rewrites `output`'s manifest and passed integration report to say they were written at
    /// these converter and world database schemas.
    fn set_schemas(output: &Path, converter_schema: u32, database_schema: u32) {
        write_world_database(output, database_schema);
        let manifest = format!(
            r#"{{"schema_version": {converter_schema}, "complete": true, "entries": {{}}}}"#
        );
        std::fs::write(output.join(MANIFEST_FILE), manifest).unwrap();
        let report = format!(r#"{{"passed": true, "schema_version": {database_schema}}}"#);
        std::fs::write(output.join("integration-report.json"), report).unwrap();
    }

    /// Play readiness binds the actual database to its report and exact current LOD contract.
    #[test]
    fn readiness_validates_database_report_and_current_lod_identity() {
        let output = complete_output("db-lod-contract");
        for version in [3, 4, 5] {
            set_schemas(&output, converter::cache::CONVERTER_SCHEMA_VERSION, version);
            assert!(
                output_is_complete(&output),
                "legacy {version} needs no LOD tables"
            );
        }
        set_schemas(&output, converter::cache::CONVERTER_SCHEMA_VERSION, 5);
        write_integration_report(&output, true); // Reports the current schema; actual DB is 5.
        assert!(
            !output_is_complete(&output),
            "integration report must match actual schema"
        );
        std::fs::write(output.join("skyrim_world.db"), b"corrupt SQLite bytes").unwrap();
        assert!(
            !output_is_complete(&output),
            "corrupt database cannot enable Play"
        );
        set_schemas(
            &output,
            converter::cache::CONVERTER_SCHEMA_VERSION,
            shared::WORLD_DATABASE_SCHEMA_VERSION,
        );
        let identity = "a".repeat(64);
        let database = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
        database
            .execute(
                "INSERT INTO lod_build(id,build_identity) VALUES(1,?1)",
                [&identity],
            )
            .unwrap();
        let mut lod = serde_json::json!({
            "converter_schema": shared::LOD_CONVERTER_SCHEMA_VERSION,
            "world_database_schema": shared::WORLD_DATABASE_SCHEMA_VERSION,
            "land_texture_repeats_per_cell": shared::LAND_TEXTURE_REPEATS_PER_CELL,
            "build_identity": identity, "chunks": 0
        });
        let path = output.join("lod-manifest.json");
        std::fs::write(&path, serde_json::to_vec(&lod).unwrap()).unwrap();
        assert!(output_is_complete(&output), "matching current LOD contract");
        lod["build_identity"] = serde_json::json!("b".repeat(64));
        std::fs::write(&path, serde_json::to_vec(&lod).unwrap()).unwrap();
        assert!(
            !output_is_complete(&output),
            "mixed build identity cannot enable Play"
        );
        lod["build_identity"] = serde_json::json!(identity);
        for stale_producer in [25, 24, 23] {
            lod["converter_schema"] = serde_json::json!(stale_producer);
            std::fs::write(&path, serde_json::to_vec(&lod).unwrap()).unwrap();
            assert!(
                !output_is_complete(&output),
                "stale LOD producer {stale_producer} cannot enable Play"
            );
        }
        std::fs::remove_file(&path).unwrap();
        database
            .execute("INSERT INTO lod_chunks(id) VALUES(0)", [])
            .unwrap();
        assert!(
            !output_is_complete(&output),
            "populated LOD needs its manifest"
        );
        drop(database);
        std::fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn readiness_rejects_empty_world_database() {
        let output = complete_output("empty-db-regression");
        std::fs::write(output.join("skyrim_world.db"), []).unwrap();
        assert!(
            !output_is_complete(&output),
            "empty database cannot enable Play"
        );
        std::fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn an_output_is_complete_at_every_schema_the_engine_loads() {
        let output = complete_output("schema-range");

        let oldest_converter = shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        let oldest_database = shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION;

        // The oldest output the engine still starts on (converter 15 + database 3 when this was
        // written, from before the merge that brought converter 16 and database 4).
        set_schemas(&output, oldest_converter, oldest_database);
        assert!(output_is_complete(&output), "the oldest schemas");

        set_schemas(&output, oldest_converter - 1, oldest_database);
        assert!(
            !output_is_complete(&output),
            "a converter older than the oldest"
        );

        set_schemas(&output, oldest_converter, oldest_database - 1);
        assert!(
            !output_is_complete(&output),
            "a database older than the oldest"
        );

        set_schemas(
            &output,
            converter::cache::CONVERTER_SCHEMA_VERSION,
            shared::WORLD_DATABASE_SCHEMA_VERSION,
        );
        assert!(output_is_complete(&output), "the current schemas");

        set_schemas(
            &output,
            converter::cache::CONVERTER_SCHEMA_VERSION + 1,
            shared::WORLD_DATABASE_SCHEMA_VERSION,
        );
        assert!(!output_is_complete(&output), "a newer converter");

        set_schemas(
            &output,
            converter::cache::CONVERTER_SCHEMA_VERSION,
            shared::WORLD_DATABASE_SCHEMA_VERSION + 1,
        );
        assert!(!output_is_complete(&output), "a newer database");

        // A report schema that does not fit in a u32 is not ready, and does not panic.
        std::fs::write(
            output.join("integration-report.json"),
            r#"{"passed": true, "schema_version": 4294967299}"#,
        )
        .unwrap();
        assert!(!output_is_complete(&output), "a report schema past u32");

        std::fs::remove_dir_all(&output).unwrap();
    }

    #[test]
    fn an_output_is_complete_only_with_everything_the_engine_needs() {
        let output = complete_output("complete");
        assert!(output_is_complete(&output));

        write_integration_report(&output, false);
        assert!(
            !output_is_complete(&output),
            "a failed integration report is not complete"
        );
        write_integration_report(&output, true);

        std::fs::remove_file(output.join("cell_cache.rkyv")).unwrap();
        assert!(!output_is_complete(&output), "no cell cache");
        std::fs::write(output.join("cell_cache.rkyv"), b"").unwrap();

        std::fs::remove_file(output.join("conversion-manifest.json")).unwrap();
        assert!(!output_is_complete(&output), "no manifest");

        let tiny = tiny_output("no-report", false);
        assert!(
            !output_is_complete(&tiny),
            "no integration report and no cell cache"
        );
        assert!(!output_is_complete(&temp_dir("not-there")));

        std::fs::remove_dir_all(&output).unwrap();
        std::fs::remove_dir_all(&tiny).unwrap();
    }

    #[test]
    fn readiness_accepts_the_same_schema_ranges_as_the_runtime() {
        let output = complete_output("lod-schema-range");
        for converter_schema in 14..=converter::cache::CONVERTER_SCHEMA_VERSION + 1 {
            for world_schema in 2..=shared::WORLD_DATABASE_SCHEMA_VERSION + 1 {
                set_schemas(&output, converter_schema, world_schema);
                assert_eq!(
                    output_is_complete(&output),
                    (shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION
                        ..=converter::cache::CONVERTER_SCHEMA_VERSION)
                        .contains(&converter_schema)
                        && shared::supports_runtime_world_database_schema(world_schema),
                    "converter {converter_schema}, world {world_schema}"
                );
            }
        }
        std::fs::remove_dir_all(output).unwrap();
    }

    /// The output is looked at on the first frame and again when the Output folder changes, never
    /// by starting anything.
    #[test]
    fn output_ready_follows_the_output_folder() {
        let complete = complete_output("ready-follows");
        let elsewhere = temp_dir("ready-elsewhere");
        let mut app = refresh_app(&complete);
        app.update();
        assert_eq!(*app.world().resource::<OutputReady>(), OutputReady(true));
        assert!(
            app.world()
                .resource::<ConversionStatus>()
                .notice_text()
                .contains("Play is ready"),
            "{:?}",
            app.world().resource::<ConversionStatus>().notice_text()
        );

        app.world_mut()
            .resource_mut::<GamePathConfig>()
            .converted_assets_path = elsewhere;
        app.update();
        assert_eq!(*app.world().resource::<OutputReady>(), OutputReady(false));
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        assert!(
            app.world()
                .resource::<PendingEffects>()
                .pending()
                .is_empty()
        );

        std::fs::remove_dir_all(&complete).unwrap();
    }

    fn summary_with(problems: usize) -> state::CheckSummary {
        state::CheckSummary {
            mode: CheckMode::Quick,
            files_checked: 100,
            bytes_checked: 3_100_000,
            elapsed: Duration::from_millis(200),
            problem_count: problems,
            first_problems: (0..problems.min(state::CHECK_PROBLEM_LINES))
                .map(|index| format!("missing: meshes/{index:02}.glb"))
                .collect(),
            advice: Some("Run the converter again on the same Data folder.".to_owned()),
        }
    }

    /// Presses Check and carries its effect out the way the window does, with a scripted channel
    /// in place of the checking thread.
    fn start_scripted_check(app: &mut App) -> crossbeam_channel::Sender<RunMessage> {
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Check {
                output: PathBuf::from("C:/out"),
                mode: CheckMode::Quick,
            });
        app.update();
        let effects = take_effects(app);
        let [Effect::BeginCheck { output, mode }] = effects.as_slice() else {
            panic!("Check asked for {effects:?}");
        };
        assert_eq!(output, &PathBuf::from("C:/out"));
        app.world_mut()
            .resource_mut::<ConversionStatus>()
            .begin_check(*mode, output);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        tx
    }

    /// A check's progress moves the bar, never backwards, and its result fills the pane and puts
    /// the window back where it was.
    #[test]
    fn a_check_moves_the_bar_and_ends_with_its_result() {
        let mut app = test_app();
        let tx = start_scripted_check(&mut app);
        assert!(
            matches!(
                app.world().resource::<CurrentConversion>().0,
                ConversionState::Checking { .. }
            ),
            "{:?}",
            app.world().resource::<CurrentConversion>().0
        );
        assert!(
            app.world()
                .resource::<ConversionStatus>()
                .stage_line()
                .contains("reading the manifest")
        );

        for (done, bar) in [(0, 0.0), (25, 25.0), (60, 60.0), (40, 60.0), (100, 100.0)] {
            push_message(
                &mut app,
                &tx,
                RunMessage::CheckProgress { done, total: 100 },
            );
            assert_eq!(
                app.world().resource::<ConversionStatus>().overall_percent(),
                bar,
                "after {done} of 100"
            );
        }
        let stage = app.world().resource::<ConversionStatus>().stage_line();
        assert!(stage.contains("Quick check   100/100 files"), "{stage:?}");

        let summary = summary_with(12);
        push_message(&mut app, &tx, RunMessage::CheckFinished(summary.clone()));
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle,
            "the check returns to where it started"
        );
        let status = app.world().resource::<ConversionStatus>();
        let pane = status.notice_text();
        assert_eq!(pane, summary.lines().join("\n"));
        assert!(pane.starts_with("12 problem(s) in 100 files"), "{pane:?}");
        assert!(pane.contains("  missing: meshes/00.glb"), "{pane:?}");
        assert!(pane.contains("  and 4 more"), "{pane:?}");
        assert!(
            pane.ends_with("Run the converter again on the same Data folder."),
            "{pane:?}"
        );
        assert_eq!(status.overall_percent(), 100.0);
        assert!(
            status.stage_line().contains("done"),
            "{:?}",
            status.stage_line()
        );
    }

    #[test]
    fn a_clean_check_reads_all_good_in_the_pane() {
        let mut app = test_app();
        let tx = start_scripted_check(&mut app);
        push_message(
            &mut app,
            &tx,
            RunMessage::CheckProgress {
                done: 0,
                total: 100,
            },
        );
        push_message(&mut app, &tx, RunMessage::CheckFinished(summary_with(0)));
        let status = app.world().resource::<ConversionStatus>();
        assert_eq!(
            status.notice_text(),
            "All good: 100 files, 3.1 MB, quick check: existence and size, 0.2 s"
        );
        assert_eq!(
            status.overall_percent(),
            100.0,
            "a finished check fills the bar"
        );
    }

    #[test]
    fn a_check_that_cannot_run_says_why_and_changes_nothing_else() {
        let mut app = test_app();
        start(&mut app);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        let staging = PathBuf::from("C:/out.staging-20260928");
        push_message(
            &mut app,
            &tx,
            RunMessage::Failed(FailureReport {
                message: "stopped".into(),
                staging: Some(staging.clone()),
                cancelled: true,
            }),
        );
        let before = app.world().resource::<CurrentConversion>().0.clone();

        let tx = start_scripted_check(&mut app);
        push_message(
            &mut app,
            &tx,
            RunMessage::CheckFailed("C:/out has no conversion-manifest.json".into()),
        );
        let state = app.world().resource::<CurrentConversion>().0.clone();
        assert_eq!(
            state, before,
            "a failed check keeps the staging folder's offer"
        );
        assert!(state::controls(&state).resume);
        let status = app.world().resource::<ConversionStatus>();
        assert_eq!(
            status.notice_text(),
            "Check failed: C:/out has no conversion-manifest.json"
        );
        assert_eq!(status.overall_percent(), 0.0);
    }

    /// Start, Resume and a second check are refused while a check runs, whatever is pressed.
    #[test]
    fn nothing_else_starts_while_a_check_runs() {
        let mut app = test_app();
        let _tx = start_scripted_check(&mut app);
        let (data, output) = folders();
        for input in [
            Input::Start {
                data: data.clone(),
                output: output.clone(),
            },
            Input::Resume { data, output },
            Input::Check {
                output: PathBuf::from("C:/out"),
                mode: CheckMode::Full,
            },
            Input::DeleteStaging,
        ] {
            app.world_mut().resource_mut::<PendingInputs>().push(input);
        }
        app.update();
        assert!(
            matches!(
                app.world().resource::<CurrentConversion>().0,
                ConversionState::Checking { .. }
            ),
            "{:?}",
            app.world().resource::<CurrentConversion>().0
        );
        assert!(
            take_effects(&mut app).is_empty(),
            "a press during a check asked for something"
        );
    }

    /// Stop during a check asks the checking thread to stop; when it reports that it did, the
    /// window is back where it was, the pane says "Check stopped." and nothing of the part checked
    /// so far is shown as a result.
    #[test]
    fn stop_during_a_check_stops_it_without_a_partial_result() {
        let mut app = test_app();
        let tx = start_scripted_check(&mut app);
        push_message(
            &mut app,
            &tx,
            RunMessage::CheckProgress {
                done: 40,
                total: 100,
            },
        );

        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Stop);
        app.update();
        let effects = take_effects(&mut app);
        assert!(
            matches!(effects.as_slice(), [Effect::CancelCheck]),
            "Stop during a check asked for {effects:?}"
        );
        assert!(
            matches!(
                app.world().resource::<CurrentConversion>().0,
                ConversionState::Checking { .. }
            ),
            "the check is still winding down"
        );

        push_message(&mut app, &tx, RunMessage::CheckCancelled);
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        let status = app.world().resource::<ConversionStatus>();
        assert_eq!(status.notice_text(), "Check stopped.");
        assert_eq!(status.overall_percent(), 0.0, "no partial bar is left");
        assert_eq!(status.stage_line(), "Quick check stopped");
    }

    /// The Stop effect reaches the flag the checking thread reads.
    #[test]
    fn the_cancel_check_effect_sets_the_checks_flag() {
        let mut app = test_app();
        app.add_systems(Update, actuate_effects.after(LauncherSet::Logic));
        let cancel = Arc::new(AtomicBool::new(false));
        app.world_mut().resource_mut::<RunHandle>().check_cancel = Some(Arc::clone(&cancel));
        app.world_mut()
            .resource_mut::<PendingEffects>()
            .push(Effect::CancelCheck);
        app.update();
        assert!(cancel.load(Ordering::Relaxed));
    }

    /// Puts the window in `Stopped` with `staging` waiting, then presses Delete staging.
    fn press_delete_staging(app: &mut App, staging: &Path) {
        app.world_mut().resource_mut::<CurrentConversion>().0 = ConversionState::Stopped {
            staging: Some(staging.to_path_buf()),
            cancelled: true,
        };
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::DeleteStaging);
    }

    /// Delete staging runs on its own thread: the window shows "Deleting ..." and keeps drawing
    /// frames while it goes, and the folder is gone when the thread reports.
    #[test]
    fn delete_staging_deletes_on_its_own_thread_and_reports_in_the_pane() {
        let root = temp_dir("window-delete");
        let staging = root.join("modern_assets.staging-1");
        std::fs::create_dir_all(staging.join("meshes")).unwrap();
        std::fs::write(staging.join("meshes").join("a.glb"), b"glTF!").unwrap();
        let mut app = test_app();
        app.add_systems(Update, actuate_effects.after(LauncherSet::Logic));
        press_delete_staging(&mut app, &staging);
        app.update();
        let deleting = ConversionState::Deleting {
            staging: staging.clone(),
            cancelled: true,
        };
        let started = Instant::now();
        loop {
            let state = app.world().resource::<CurrentConversion>().0.clone();
            if state != deleting {
                break;
            }
            let status = app.world().resource::<ConversionStatus>();
            assert!(
                status.stage_line().starts_with("Deleting "),
                "{:?}",
                status.stage_line()
            );
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the delete never reported"
            );
            std::thread::sleep(Duration::from_millis(5));
            app.update();
        }
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Stopped {
                staging: None,
                cancelled: true,
            }
        );
        assert!(!staging.exists(), "the staging folder is still there");
        let status = app.world().resource::<ConversionStatus>();
        assert!(status.deleting.is_none());
        assert!(
            status.notice_text().contains("Deleted the staging folder"),
            "{:?}",
            status.notice_text()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// While the delete runs every control is off, and the status says what is being deleted.
    #[test]
    fn nothing_can_be_pressed_while_a_staging_folder_is_deleted() {
        let staging = PathBuf::from("C:/out.staging-1");
        let mut app = test_app();
        press_delete_staging(&mut app, &staging);
        app.update();
        let effects = take_effects(&mut app);
        assert!(
            matches!(effects.as_slice(), [Effect::DeleteStaging { staging: asked }] if *asked == staging),
            "{effects:?}"
        );
        // Carry the effect out by hand, with a scripted channel in place of the thread.
        app.world_mut()
            .resource_mut::<ConversionStatus>()
            .begin_delete(&staging);
        let (tx, rx) = unbounded();
        app.world_mut().insert_resource(RunChannel { receiver: rx });
        let state = app.world().resource::<CurrentConversion>().0.clone();
        let controls = state::controls(&state);
        assert!(
            !(controls.start
                || controls.stop
                || controls.resume
                || controls.delete_staging
                || controls.check
                || controls.paths),
            "{controls:?}"
        );
        assert_eq!(
            app.world().resource::<ConversionStatus>().stage_line(),
            "Deleting C:/out.staging-1..."
        );

        // A failed delete offers the folder again and says why.
        push_message(
            &mut app,
            &tx,
            RunMessage::StagingDeleted {
                staging: staging.clone(),
                result: Err("Access is denied. (os error 5)".into()),
            },
        );
        let state = app.world().resource::<CurrentConversion>().0.clone();
        assert_eq!(
            state,
            ConversionState::Stopped {
                staging: Some(staging),
                cancelled: true,
            }
        );
        assert!(state::controls(&state).resume && state::controls(&state).delete_staging);
        let status = app.world().resource::<ConversionStatus>();
        assert!(status.deleting.is_none());
        assert!(
            status
                .notice_text()
                .contains("Could not delete C:/out.staging-1: Access is denied."),
            "{:?}",
            status.notice_text()
        );
    }

    /// A folder beside `output` that looks like a staging folder a run left: named after the
    /// output.
    fn leftover_staging(output: &Path, suffix: &str) -> PathBuf {
        let name = output.file_name().unwrap().to_str().unwrap();
        let staging = output.with_file_name(format!("{name}.staging-{suffix}"));
        std::fs::create_dir_all(&staging).unwrap();
        staging
    }

    fn refresh_app(output: &Path) -> App {
        let mut app = test_app();
        app.init_resource::<OutputReady>()
            .init_resource::<OutputHasManifest>()
            .add_systems(Update, refresh_output_ready.after(tick_status));
        app.world_mut()
            .resource_mut::<GamePathConfig>()
            .converted_assets_path = output.to_path_buf();
        app
    }

    /// A staging folder an earlier session left is found on the first frame and offered for Resume,
    /// and Resume asks for a run that carries on from it.
    #[test]
    fn a_leftover_staging_folder_found_at_start_up_enables_resume() {
        let root = temp_dir("leftover");
        let output = root.join("modern_assets");
        std::fs::create_dir_all(&root).unwrap();
        let staging = leftover_staging(&output, "1-1");
        let mut app = refresh_app(&output);
        app.update();

        let state = app.world().resource::<CurrentConversion>().0.clone();
        assert_eq!(
            state,
            ConversionState::Stopped {
                staging: Some(staging.clone()),
                cancelled: false,
            }
        );
        let controls = state::controls(&state);
        assert!(controls.resume && controls.delete_staging);
        let pane = app.world().resource::<ConversionStatus>().notice_text();
        assert_eq!(
            pane,
            "An unfinished conversion was found in modern_assets.staging-1-1. Resume continues where it stopped; finished files are checked again, not redone. Delete staging removes it."
        );

        let (data, _) = folders();
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Resume {
                data,
                output: output.clone(),
            });
        app.update();
        let effects = take_effects(&mut app);
        let [Effect::Begin(config)] = effects.as_slice() else {
            panic!("Resume asked for {effects:?}");
        };
        assert_eq!(config.resume_staging, Some(staging));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Several leftovers: the newest is offered, the others are counted, and none is deleted.
    #[test]
    fn older_leftover_staging_folders_are_counted_and_kept() {
        let root = temp_dir("leftovers");
        let output = root.join("modern_assets");
        std::fs::create_dir_all(&root).unwrap();
        let older = leftover_staging(&output, "1-1");
        let newer = leftover_staging(&output, "2-2");
        let mut app = refresh_app(&output);
        app.update();

        let state = app.world().resource::<CurrentConversion>().0.clone();
        assert!(
            matches!(&state, ConversionState::Stopped { staging: Some(path), .. } if *path == newer),
            "{state:?}"
        );
        let pane = app.world().resource::<ConversionStatus>().notice_text();
        assert!(
            pane.ends_with("1 older unfinished conversion(s) beside it are left untouched."),
            "{pane:?}"
        );
        assert!(older.is_dir() && newer.is_dir(), "a leftover was deleted");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A complete output is ready to play; a staging folder beside it is not offered.
    #[test]
    fn a_leftover_staging_folder_beside_a_complete_output_is_not_offered() {
        let output = complete_output("leftover-complete");
        let staging = leftover_staging(&output, "1-1");
        let mut app = refresh_app(&output);
        app.update();
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        assert_eq!(*app.world().resource::<OutputReady>(), OutputReady(true));
        std::fs::remove_dir_all(&output).unwrap();
        std::fs::remove_dir_all(&staging).unwrap();
    }

    /// Check needs a manifest to read: the flag follows the Output folder.
    #[test]
    fn the_manifest_flag_follows_the_output_folder() {
        let tiny = tiny_output("manifest-flag", false);
        let empty = temp_dir("manifest-flag-empty");
        std::fs::create_dir_all(&empty).unwrap();
        let mut app = refresh_app(&empty);
        app.update();
        assert_eq!(
            *app.world().resource::<OutputHasManifest>(),
            OutputHasManifest(false)
        );
        app.world_mut()
            .resource_mut::<GamePathConfig>()
            .converted_assets_path = tiny.clone();
        app.update();
        assert_eq!(
            *app.world().resource::<OutputHasManifest>(),
            OutputHasManifest(true)
        );
        std::fs::remove_dir_all(&tiny).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
    }

    /// The whole path with a real `check_output`: the press, the checking thread, the bar and the
    /// result, on a hand-written output of two entries (one of them missing).
    #[test]
    fn a_real_check_runs_on_its_own_thread_and_reports_in_the_pane() {
        let output = tiny_output("window-check", true);
        let mut app = test_app();
        app.add_systems(Update, actuate_effects.after(LauncherSet::Logic));
        app.world_mut()
            .resource_mut::<PendingInputs>()
            .push(Input::Check {
                output: output.clone(),
                mode: CheckMode::Full,
            });
        let started = Instant::now();
        loop {
            app.update();
            if !matches!(
                app.world().resource::<CurrentConversion>().0,
                ConversionState::Checking { .. }
            ) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the check never reported"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        let status = app.world().resource::<ConversionStatus>();
        let pane = status.notice_text();
        assert!(
            pane.starts_with("1 problem(s) in 2 files, 5 B, full check: size and hash"),
            "{pane:?}"
        );
        assert!(pane.contains("  missing: meshes/b.glb"), "{pane:?}");
        assert!(pane.contains("Run the converter again"), "{pane:?}");
        assert_eq!(status.overall_percent(), 100.0);
        std::fs::remove_dir_all(&output).unwrap();
    }
}
