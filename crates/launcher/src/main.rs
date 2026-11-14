//! The Mudcrab launcher: find Skyrim, convert its assets, and start the engine on them.
//!
//! One window: the conversion panel (folders, Start / Stop / Resume, progress, Check), the mod
//! manager's drop zone, and Play, which is enabled once the Output folder holds a complete
//! conversion. `cargo run -p launcher` starts it.

mod components;
mod conversion;
mod engine_process;
mod game_detection;
mod handlers;
mod mo2_settings;
mod ui;

use bevy::{prelude::*, window::WindowResolution};

use conversion::ConversionPanelPlugin;
use handlers::*;
use ui::*;

#[derive(States, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum LauncherState {
    /// Before the first frame's look at the folders.
    #[default]
    DetectingGameFiles,
    /// No Skyrim `Data` folder has been found or dropped yet.
    FirstRunSetup,
    /// The Output folder holds a complete conversion; the mod manager and Play are what matter.
    ModManager,
    /// A conversion is running, or one is still to be made from a known Data folder.
    ConvertingAssets,
    /// Play was pressed; the engine is started on entering this state.
    LaunchingEngine,
}

/// The order the launcher's work happens in each frame: read the controls, act on them, do what the
/// conversion's state machine asked for, then draw what the result looks like.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LauncherSet {
    /// Buttons and dropped items turn into inputs; nothing is shown from here.
    Input,
    /// The inputs and the running conversion's messages are folded into the state and the status.
    Logic,
    /// The effects the state machine asked for are carried out (start a run, stop it, delete a
    /// staging folder, start a check).
    Actuate,
    /// The widgets are drawn from the state and the status.
    Draw,
}

/// Everything the launcher is, apart from the window: `main` adds it to the default plugins, and
/// the tests add it to a headless app.
pub struct LauncherPlugin;

impl Plugin for LauncherPlugin {
    /// Configures launcher system ordering, UI setup, MO2 settings, and engine launch handling.
    fn build(&self, app: &mut App) {
        app.configure_sets(
            Update,
            (
                LauncherSet::Input,
                LauncherSet::Logic,
                LauncherSet::Actuate,
                LauncherSet::Draw,
            )
                .chain(),
        )
        .init_state::<LauncherState>()
        .add_plugins(ConversionPanelPlugin)
        .add_systems(Startup, setup_ui)
        .add_systems(
            Update,
            (handle_play_button_click, handle_mod_drag_and_drop).in_set(LauncherSet::Input),
        )
        .add_systems(
            Update,
            (sync_launcher_state, draw_play).in_set(LauncherSet::Draw),
        )
        .add_systems(Update, watch_engine_process)
        .add_systems(OnEnter(LauncherState::LaunchingEngine), launch_engine);
        mo2_settings::install(app);
    }
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Mudcrab Launcher & Mod Manager".into(),
                resolution: WindowResolution::new(900, 680),
                resizable: false,
                ..default()
            }),
            ..default()
        }))
        .add_plugins(LauncherPlugin)
        .run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::PlayButton;
    use crate::conversion::panel::{BUTTON_OFF, ControlButton, ControlLabel, DataPathText};
    use crate::conversion::state::ConversionState;
    use crate::conversion::tests::{complete_output, temp_dir};
    use crate::conversion::{
        CurrentConversion, GamePathConfig, OutputReady, PendingEffects, RunChannel,
    };
    use bevy::state::app::StatesPlugin;
    use std::path::{Path, PathBuf};

    /// The whole launcher in a headless app: every system it adds, the window's scene spawned,
    /// and the folders given rather than detected, so nothing depends on the machine running the
    /// test.
    fn launcher_app(data: Option<PathBuf>, output: PathBuf) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            AssetPlugin::default(),
            bevy::scene::ScenePlugin,
        ))
        .add_message::<FileDragAndDrop>()
        .insert_resource(GamePathConfig {
            skyrim_data_path: data,
            converted_assets_path: output,
        })
        .add_plugins(LauncherPlugin);
        // Startup, the first frame's output check, and the state transition it asks for.
        for _ in 0..3 {
            app.update();
        }
        app
    }

    fn data_folder(name: &str) -> PathBuf {
        let data = temp_dir(name);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("Skyrim.esm"), []).unwrap();
        data
    }

    fn drawn_on<T: Component>(app: &mut App) -> bool {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&BackgroundColor, With<T>>();
        query
            .iter(world)
            .any(|background| background.0 != BUTTON_OFF)
    }

    fn control_drawn_on(app: &mut App, control: ControlButton) -> bool {
        let world = app.world_mut();
        let mut query = world.query::<(&ControlButton, &BackgroundColor)>();
        query
            .iter(world)
            .any(|(button, background)| *button == control && background.0 != BUTTON_OFF)
    }

    fn launcher_state(app: &App) -> LauncherState {
        app.world().resource::<State<LauncherState>>().get().clone()
    }

    /// Bevy refuses two queries in one system that could alias (B0001) only when a schedule is
    /// built, and a window that crashes at start is the result. Building and running every
    /// schedule of the whole launcher here makes such a conflict fail a test instead.
    #[test]
    fn the_whole_launcher_runs_without_conflicting_queries() {
        let data = data_folder("whole-app");
        let mut app = launcher_app(Some(data.clone()), temp_dir("whole-app-output"));
        app.update();
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// A complete output on disk: the launcher lands ready to play, and converts nothing.
    #[test]
    fn a_complete_output_lands_ready_to_play() {
        let data = data_folder("ready-data");
        let output = complete_output("ready-output");
        let mut app = launcher_app(Some(data.clone()), output.clone());

        assert_eq!(*app.world().resource::<OutputReady>(), OutputReady(true));
        assert!(drawn_on::<PlayButton>(&mut app), "Play is not enabled");
        assert_eq!(launcher_state(&app), LauncherState::ModManager);
        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Idle
        );
        assert!(
            app.world().get_resource::<RunChannel>().is_none(),
            "the launcher started a run by itself"
        );
        assert!(
            app.world()
                .resource::<PendingEffects>()
                .pending()
                .is_empty()
        );
        let world = app.world_mut();
        let mut labels = world.query::<(&ControlLabel, &Text)>();
        let start = labels
            .iter(world)
            .find(|(label, _)| label.0 == ControlButton::Start)
            .map(|(_, text)| text.0.clone());
        assert_eq!(start.as_deref(), Some("Convert again"));

        std::fs::remove_dir_all(&data).unwrap();
        std::fs::remove_dir_all(&output).unwrap();
    }

    /// No output yet: Start is there to press, Play is not, and nothing starts on its own.
    #[test]
    fn without_an_output_start_is_enabled_and_play_is_not() {
        let data = data_folder("no-output-data");
        let mut app = launcher_app(Some(data.clone()), temp_dir("no-output"));

        assert_eq!(*app.world().resource::<OutputReady>(), OutputReady(false));
        assert!(control_drawn_on(&mut app, ControlButton::Start));
        assert!(!drawn_on::<PlayButton>(&mut app), "Play is enabled");
        assert_eq!(launcher_state(&app), LauncherState::ConvertingAssets);
        assert!(app.world().get_resource::<RunChannel>().is_none());

        std::fs::remove_dir_all(&data).unwrap();
    }

    /// A staging folder an earlier session left beside the output: the launcher opens with Resume
    /// and Delete staging on, Check off (there is no manifest yet), and nothing started.
    #[test]
    fn a_leftover_staging_folder_is_offered_for_resume_at_start_up() {
        let data = data_folder("leftover-data");
        let root = temp_dir("leftover-root");
        let output = root.join("modern_assets");
        let staging = root.join("modern_assets.staging-1-1");
        std::fs::create_dir_all(&staging).unwrap();
        let mut app = launcher_app(Some(data.clone()), output);

        assert_eq!(
            app.world().resource::<CurrentConversion>().0,
            ConversionState::Stopped {
                staging: Some(staging),
                cancelled: false,
            }
        );
        assert!(control_drawn_on(&mut app, ControlButton::Resume));
        assert!(control_drawn_on(&mut app, ControlButton::DeleteStaging));
        assert!(control_drawn_on(&mut app, ControlButton::Start));
        assert!(!control_drawn_on(&mut app, ControlButton::Check));
        assert!(!control_drawn_on(&mut app, ControlButton::FullCheck));
        assert!(app.world().get_resource::<RunChannel>().is_none());

        std::fs::remove_dir_all(&data).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A Data folder dropped onto the window fills the Data row.
    #[test]
    fn a_dropped_data_folder_fills_the_data_row() {
        let mut app = launcher_app(None, temp_dir("drop-output"));
        // Detection is skipped only when a Data folder is given, so clear whatever the machine
        // running the test has installed.
        app.world_mut()
            .resource_mut::<GamePathConfig>()
            .skyrim_data_path = None;
        app.update();
        let install = temp_dir("drop-install");
        let data = install.join("Data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("Skyrim.esm"), []).unwrap();

        drop_path(&mut app, &install);
        assert_eq!(
            app.world()
                .resource::<GamePathConfig>()
                .skyrim_data_path
                .as_deref(),
            Some(data.as_path()),
            "an installation root means its Data folder"
        );
        let world = app.world_mut();
        let mut row = world.query_filtered::<&Text, With<DataPathText>>();
        let shown = row.single(world).unwrap().0.clone();
        assert_eq!(shown, data.display().to_string());
        assert!(control_drawn_on(&mut app, ControlButton::Start));

        std::fs::remove_dir_all(&install).unwrap();
    }

    fn drop_path(app: &mut App, path: &Path) {
        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: path.to_path_buf(),
        });
        app.update();
    }
}
