//! Developer console: the backtick key opens a command line that pauses the game.
//!
//! Commands live in a [`ConsoleRegistry`]; every plugin registers its own with
//! [`AppConsoleExt::add_console_command`]. Names follow Skyrim's console: a command has a long
//! name (`Help`, `ClearConsole`, `ToggleCollision`) and, where Skyrim has one, a short form
//! (`tcl`, `tcg`); matching ignores case. Parsing, completion, the "did you mean" search and
//! the scrollback are pure functions so they are tested without a UI.

use std::sync::Arc;

use bevy::{
    input::{ButtonState, InputSystems, keyboard::KeyboardInput},
    prelude::*,
};

use crate::physics::{CursorCapture, MotionReset};

/// Scrollback lines kept in memory.
pub const SCROLLBACK_CAP: usize = 200;
/// Longest input line, in chars; further typing is ignored.
pub const INPUT_CAP: usize = 256;
/// Scrollback lines drawn on screen.
pub const VISIBLE_LINES: usize = 15;
/// A misspelt command is suggested a correction when it is at most this many edits away.
const SUGGESTION_DISTANCE: usize = 2;

/// Command handler: gets the whole world and the arguments after the command name.
type HandlerFn = dyn Fn(&mut World, &[&str]) -> Result<String, String> + Send + Sync;
pub type CommandHandler = Box<HandlerFn>;

/// One console command, as a plugin registers it.
pub struct ConsoleCommand {
    /// Long name, as Skyrim spells it (`Help`, `ClearConsole`, `ToggleCollision`, ...).
    pub name: &'static str,
    /// Skyrim's short form of the name, when it has one (`tcl`, `tcg`); no other aliases.
    pub short: Option<&'static str>,
    pub usage: &'static str,
    /// One line shown by `Help`.
    pub help: &'static str,
    pub handler: CommandHandler,
}

/// A registered command. The handler is shared so a command can run with `&mut World`.
pub struct RegisteredCommand {
    pub name: &'static str,
    pub short: Option<&'static str>,
    pub usage: &'static str,
    pub help: &'static str,
    handler: Arc<HandlerFn>,
}

#[derive(Resource, Default)]
pub struct ConsoleRegistry {
    commands: Vec<RegisteredCommand>,
}

impl ConsoleRegistry {
    /// Add a command; a later command with the same name replaces the earlier one.
    pub fn register(&mut self, command: ConsoleCommand) {
        self.commands
            .retain(|c| !c.name.eq_ignore_ascii_case(command.name));
        self.commands.push(RegisteredCommand {
            name: command.name,
            short: command.short,
            usage: command.usage,
            help: command.help,
            handler: Arc::from(command.handler),
        });
    }

    /// Look a command up by long name or short form, ignoring case.
    pub fn get(&self, name: &str) -> Option<&RegisteredCommand> {
        self.commands.iter().find(|c| {
            c.name.eq_ignore_ascii_case(name)
                || c.short
                    .is_some_and(|short| short.eq_ignore_ascii_case(name))
        })
    }

    /// Every name a command answers to, long names and short forms in the casing they were
    /// registered with, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self
            .commands
            .iter()
            .flat_map(|c| std::iter::once(c.name).chain(c.short))
            .collect();
        names.sort_unstable();
        names
    }

    /// One block listing every command with its usage and one-line help. With `filter`, only the
    /// commands whose long name, short form or help line contains the text (case-insensitive).
    pub fn help_text(&self, filter: Option<&str>) -> String {
        let filter = filter.map(str::to_ascii_lowercase);
        let mut names: Vec<_> = self.commands.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names
            .into_iter()
            .filter_map(|name| self.get(name))
            .filter(|command| match &filter {
                Some(text) => command_matches(command, text),
                None => true,
            })
            .map(format_help_line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Whether the lowercase `text` appears in a command's long name, short form or help line.
fn command_matches(command: &RegisteredCommand, text: &str) -> bool {
    let contains = |haystack: &str| haystack.to_ascii_lowercase().contains(text);
    contains(command.name) || command.short.is_some_and(contains) || contains(command.help)
}

fn format_help_line(command: &RegisteredCommand) -> String {
    match command.short {
        Some(short) => format!("{} [{short}] - {}", command.usage, command.help),
        None => format!("{} - {}", command.usage, command.help),
    }
}

/// Console state shared by the input, dispatch and UI systems.
#[derive(Resource, Default)]
pub struct ConsoleState {
    pub open: bool,
    /// Hide the overlay for a bug-report image without relinquishing console input ownership.
    pub capture_suppressed: bool,
    pub buffer: String,
    pub scrollback: Vec<String>,
    /// Cursor capture the player had when the console opened.
    pub previous_capture: CursorCapture,
    /// Capture to give back one frame after a backtick close. It is held back so the movement and
    /// look systems, which read the capture, ignore the input the closing frame carried.
    pub restore_capture: Option<CursorCapture>,
    /// Whether virtual time was already paused when the console opened.
    pub was_paused: bool,
    /// Lines entered with Enter, run by the dispatch system.
    pub pending: Vec<String>,
    /// Set on the frame the console closes: keys typed into it must not act on gameplay that frame.
    pub closing: bool,
}

/// Run condition: true while the console is closed, and when there is no console at all.
/// It stays false on the frame the console closes, so the keys that closing frame carried
/// (typed into the console) do not reach the game.
pub fn console_closed(
    state: Option<Res<ConsoleState>>,
    menu: Option<Res<crate::settings_ui::SettingsMenuState>>,
) -> bool {
    state.is_none_or(|state| !state.open && !state.closing && !state.capture_suppressed)
        && menu.is_none_or(|menu| !menu.blocks_gameplay())
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConsoleInputSet;

pub trait AppConsoleExt {
    fn add_console_command(&mut self, command: ConsoleCommand) -> &mut Self;
}

impl AppConsoleExt for App {
    fn add_console_command(&mut self, command: ConsoleCommand) -> &mut Self {
        self.init_resource::<ConsoleRegistry>();
        self.world_mut()
            .resource_mut::<ConsoleRegistry>()
            .register(command);
        self
    }
}

pub struct ConsolePlugin;

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ConsoleSystems {
    Ui,
}

impl Plugin for ConsolePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConsoleRegistry>()
            .init_resource::<ConsoleState>()
            .init_resource::<CursorCapture>()
            .add_message::<KeyboardInput>()
            .add_console_command(help_command())
            .add_console_command(clear_command())
            .add_systems(Startup, spawn_console_ui)
            .add_systems(
                PreUpdate,
                (console_input_system, console_dispatch_system)
                    .chain()
                    .after(InputSystems)
                    .in_set(ConsoleInputSet),
            )
            .add_systems(Update, refresh_console_ui.in_set(ConsoleSystems::Ui));
    }
}

fn help_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "Help",
        short: None,
        usage: "Help [text]",
        help: "list every command, or the ones whose name or description contains the text",
        handler: Box::new(|world, args| {
            let registry = world.resource::<ConsoleRegistry>();
            match args {
                [] => Ok(registry.help_text(None)),
                [text] => {
                    let listing = registry.help_text(Some(text));
                    if listing.is_empty() {
                        Ok(format!("no command matches \"{text}\""))
                    } else {
                        Ok(listing)
                    }
                }
                _ => Err("usage: Help [text]".to_owned()),
            }
        }),
    }
}

fn clear_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "ClearConsole",
        short: None,
        usage: "ClearConsole",
        help: "clear the console scrollback",
        handler: Box::new(|world, _| {
            world.resource_mut::<ConsoleState>().scrollback.clear();
            Ok(String::new())
        }),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------

/// Split a line into the command name as typed and its arguments; `None` for a blank line.
pub fn parse_line(line: &str) -> Option<(&str, Vec<&str>)> {
    let mut tokens = line.split_whitespace();
    let name = tokens.next()?;
    Some((name, tokens.collect()))
}

/// Edit distance between two strings.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The closest name within [`SUGGESTION_DISTANCE`] edits, if any. The distance must also be
/// smaller than the input's length, so a one- or two-letter typo (a stray `t`) is not answered
/// with an unrelated short form. Matching ignores case; the name is returned in the casing it
/// was registered with.
pub fn nearest(input: &str, names: &[&str]) -> Option<String> {
    let input = input.to_ascii_lowercase();
    let length = input.chars().count();
    names
        .iter()
        .map(|name| (levenshtein(&input, &name.to_ascii_lowercase()), *name))
        .filter(|(distance, _)| *distance <= SUGGESTION_DISTANCE && *distance < length)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.to_owned())
}

pub fn unknown_command_message(input: &str, names: &[&str]) -> String {
    match nearest(input, names) {
        Some(near) => format!(
            "unknown command \"{input}\", did you mean \"{near}\"? Type \"Help\" for the list."
        ),
        None => format!("unknown command \"{input}\". Type \"Help\" for the list."),
    }
}

/// Result of completing the first token.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    /// Nothing starts with the token.
    None,
    /// Exactly one candidate, in the casing it was registered with.
    Unique(String),
    /// Several candidates sharing a longer prefix than the token.
    Prefix(String),
    /// Several candidates with nothing more in common: list them.
    List(Vec<String>),
}

pub fn completion(token: &str, names: &[&str]) -> Completion {
    // A token that already is a name (in any case) completes to the registered casing.
    if let Some(exact) = names.iter().find(|name| name.eq_ignore_ascii_case(token)) {
        return Completion::Unique((*exact).to_owned());
    }
    let token = token.to_ascii_lowercase();
    let matches: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.to_ascii_lowercase().starts_with(&token))
        .collect();
    match matches.as_slice() {
        [] => Completion::None,
        [only] => Completion::Unique((*only).to_owned()),
        [first, rest @ ..] => {
            // Extend to the prefix all candidates share, ignoring case, spelled as the first one.
            let first_lower: Vec<char> = first.to_ascii_lowercase().chars().collect();
            let mut common = first_lower.len();
            for name in rest {
                let lower: Vec<char> = name.to_ascii_lowercase().chars().collect();
                common = common.min(
                    first_lower
                        .iter()
                        .zip(lower.iter())
                        .take_while(|(a, b)| a == b)
                        .count(),
                );
            }
            let prefix: String = first.chars().take(common).collect();
            if prefix.chars().count() > token.chars().count() {
                Completion::Prefix(prefix)
            } else {
                Completion::List(matches.iter().map(|m| (*m).to_owned()).collect())
            }
        }
    }
}

/// Apply Tab to an input line. Returns the new line and the candidates to list, if any.
pub fn autofill(buffer: &str, names: &[&str]) -> (String, Vec<String>) {
    // Leading whitespace the user typed is kept.
    let token = buffer.trim_start();
    let indent = &buffer[..buffer.len() - token.len()];
    if token.contains(char::is_whitespace) {
        return (buffer.to_owned(), Vec::new());
    }
    match completion(token, names) {
        Completion::None => (buffer.to_owned(), Vec::new()),
        Completion::Unique(name) => (format!("{indent}{name} "), Vec::new()),
        Completion::Prefix(prefix) => (format!("{indent}{prefix}"), Vec::new()),
        Completion::List(list) => (buffer.to_owned(), list),
    }
}

/// Append text (possibly several lines) to the scrollback, dropping the oldest beyond the cap.
pub fn push_scrollback(lines: &mut Vec<String>, text: &str) {
    lines.extend(text.lines().map(str::to_owned));
    if lines.len() > SCROLLBACK_CAP {
        lines.drain(..lines.len() - SCROLLBACK_CAP);
    }
}

pub fn visible_scrollback(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(VISIBLE_LINES);
    lines[start..].join("\n")
}

/// Insert typed text, dropping control characters and anything past [`INPUT_CAP`] chars.
pub fn insert_text(buffer: &mut String, text: &str) {
    let room = INPUT_CAP.saturating_sub(buffer.chars().count());
    buffer.extend(text.chars().filter(|c| !c.is_control()).take(room));
}

/// Run one entered line: echo it, show its output or error, and return the complete result.
pub fn execute_line(world: &mut World, line: &str) -> String {
    let line = line.trim();
    let Some((name, args)) = parse_line(line) else {
        return String::new();
    };
    push_output(world, &format!("> {line}"));
    let (handler, names) = {
        let registry = world.resource::<ConsoleRegistry>();
        (
            registry.get(name).map(|c| Arc::clone(&c.handler)),
            registry.names(),
        )
    };
    let output = match handler {
        Some(handler) => match handler(world, &args) {
            Ok(text) => text,
            Err(text) => format!("error: {text}"),
        },
        None => unknown_command_message(name, &names),
    };
    if !output.is_empty() {
        push_output(world, &output);
    }
    output
}

fn push_output(world: &mut World, text: &str) {
    if let Some(mut state) = world.get_resource_mut::<ConsoleState>() {
        push_scrollback(&mut state.scrollback, text);
    }
}

// ---------------------------------------------------------------------------
// Systems.
// ---------------------------------------------------------------------------

/// Backtick toggle and text entry. Runs after Bevy's input systems so every later system
/// sees the current frame's open state.
#[allow(clippy::too_many_arguments)]
fn console_input_system(
    keys: Res<ButtonInput<KeyCode>>,
    mut events: MessageReader<KeyboardInput>,
    mut state: ResMut<ConsoleState>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    registry: Res<ConsoleRegistry>,
    mut motion: MotionReset,
    menu: Option<Res<crate::settings_ui::SettingsMenuState>>,
) {
    if state.closing {
        state.closing = false;
    }
    // A report hides UI without yielding gameplay input. Preserve the user's pending backtick
    // restoration until readback ends; Escape still leaves it released permanently.
    // `is_some()` reads through `Deref`, so a frame with no pending restore does not dirty
    // `ConsoleState`; only the frame that actually restores capture mutates it.
    if state.restore_capture.is_some()
        && !state.capture_suppressed
        && let Some(restored) = state.restore_capture.take()
    {
        *capture = restored;
    }
    if menu.is_some_and(|menu| menu.blocks_gameplay()) {
        events.clear();
        return;
    }
    // The physical key under Escape on every layout (the same key Skyrim uses), not the character.
    if keys.just_pressed(KeyCode::Backquote) {
        if state.open {
            close_console(&mut state, &mut capture, &mut time, false);
        } else {
            state.open = true;
            state.buffer.clear();
            state.previous_capture = state.restore_capture.take().unwrap_or(*capture);
            state.was_paused = time.is_paused();
            *capture = CursorCapture::Released;
            // Virtual time already advanced this frame in `First`; the pause bites from the next.
            time.pause();
            // Reset the walk intent, sprint latch and queued controller translation so a key held
            // while opening does not leave stale movement behind. Velocity and jump state stay, so
            // closing resumes an airborne arc; `ButtonInput` itself is untouched.
            motion.clear_input();
        }
    }
    for event in events.read() {
        if !state.open
            || event.state != ButtonState::Pressed
            || event.key_code == KeyCode::Backquote
        {
            continue;
        }
        match event.key_code {
            KeyCode::Escape => close_console(&mut state, &mut capture, &mut time, true),
            KeyCode::Backspace => {
                state.buffer.pop();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let line = std::mem::take(&mut state.buffer);
                state.pending.push(line);
            }
            KeyCode::Tab => {
                let (buffer, list) = autofill(&state.buffer, &registry.names());
                state.buffer = buffer;
                if !list.is_empty() {
                    push_scrollback(&mut state.scrollback, &list.join("  "));
                }
            }
            _ => {
                if let Some(text) = &event.text {
                    insert_text(&mut state.buffer, text);
                }
            }
        }
    }
}

/// Close the console. Escape leaves the cursor released;
/// the backtick restores the capture the player had, one frame later (see `restore_capture`).
fn close_console(
    state: &mut ConsoleState,
    capture: &mut CursorCapture,
    time: &mut Time<Virtual>,
    escape: bool,
) {
    state.open = false;
    state.closing = true;
    state.buffer.clear();
    // The capture stays Released for the closing frame; Escape leaves it released for good.
    *capture = CursorCapture::Released;
    state.restore_capture = (!escape).then_some(state.previous_capture);
    if !state.was_paused {
        time.unpause();
    }
}

fn console_dispatch_system(world: &mut World) {
    // Check through `resource` first so an idle console is not marked changed every frame.
    if world.resource::<ConsoleState>().pending.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut world.resource_mut::<ConsoleState>().pending);
    for line in lines {
        execute_line(world, &line);
    }
}

#[derive(Component)]
pub(crate) struct ConsoleRoot;
#[derive(Component)]
struct ConsoleScrollText;
#[derive(Component)]
struct ConsoleInputText;

fn spawn_console_ui(mut commands: Commands) {
    commands
        .spawn((
            Name::new("Developer console"),
            ConsoleRoot,
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                bottom: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(40.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::FlexEnd,
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.75)),
            GlobalZIndex(1000),
        ))
        .with_children(|root| {
            root.spawn((
                crate::console_selection::ConsoleSelectionText,
                Text::new(crate::console_selection::SELECTION_HINT),
                TextFont::from_font_size(16.0),
                TextColor(Color::srgb(1.0, 0.9, 0.2)),
            ));
            root.spawn((
                ConsoleScrollText,
                Text::new(""),
                TextFont::from_font_size(16.0),
                TextColor(Color::WHITE),
            ));
            root.spawn((
                ConsoleInputText,
                Text::new("> "),
                TextFont::from_font_size(18.0),
                TextColor(Color::srgb(1.0, 0.9, 0.5)),
            ));
        });
}

#[allow(clippy::type_complexity)]
fn refresh_console_ui(
    state: Res<ConsoleState>,
    mut root: Query<&mut Node, With<ConsoleRoot>>,
    mut scroll: Query<&mut Text, (With<ConsoleScrollText>, Without<ConsoleInputText>)>,
    mut input: Query<&mut Text, (With<ConsoleInputText>, Without<ConsoleScrollText>)>,
) {
    if !state.is_changed() {
        return;
    }
    for mut node in &mut root {
        node.display = if state.open && !state.capture_suppressed {
            Display::Flex
        } else {
            Display::None
        };
    }
    for mut text in &mut scroll {
        **text = visible_scrollback(&state.scrollback);
    }
    for mut text in &mut input {
        **text = format!("> {}", state.buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::input::keyboard::Key;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    /// Every name the real commands answer to, in the sorted order `ConsoleRegistry::names` gives.
    const NAMES: &[&str] = &[
        "ClearConsole",
        "Help",
        "ToggleCollision",
        "ToggleCollisionGeometry",
        "tcg",
        "tcl",
    ];

    fn console_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::input::InputPlugin)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                20,
            )))
            .add_plugins(ConsolePlugin);
        app.finish();
        app.update();
        app
    }

    fn send_state(app: &mut App, key_code: KeyCode, state: ButtonState) {
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key: Key::Character("`".into()),
            state,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
    }

    /// A full backquote press and release, through `InputPlugin` like a real keyboard.
    fn press_backquote(app: &mut App) {
        send_state(app, KeyCode::Backquote, ButtonState::Pressed);
        send_state(app, KeyCode::Backquote, ButtonState::Released);
    }

    fn type_key(app: &mut App, key_code: KeyCode, logical_key: Key, text: Option<&str>) {
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: text.map(Into::into),
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
    }

    fn type_char(app: &mut App, key_code: KeyCode, c: &str) {
        type_key(app, key_code, Key::Character(c.into()), Some(c));
    }

    fn scrollback(app: &App) -> String {
        app.world().resource::<ConsoleState>().scrollback.join("\n")
    }

    /// Whether a system running at the end of the frame saw `ConsoleState` changed.
    #[derive(Resource, Default)]
    struct ConsoleStateProbe(Option<bool>);

    /// Records, at the end of the frame, whether `ConsoleState` was marked changed.
    fn probe_console_state(state: Res<ConsoleState>, mut probe: ResMut<ConsoleStateProbe>) {
        probe.0 = Some(state.is_changed());
    }

    /// A closed console with no input leaves `ConsoleState` untouched frame after frame.
    #[test]
    fn settled_closed_console_does_not_dirty_its_state() {
        let mut app = console_app();
        app.init_resource::<ConsoleStateProbe>()
            .add_systems(Last, probe_console_state);
        // The probe's own first run sets its change-detection baseline.
        app.update();
        for frame in 1..=3 {
            app.update();
            assert_eq!(
                app.world().resource::<ConsoleStateProbe>().0,
                Some(false),
                "frame {frame}: a settled closed console marked ConsoleState changed"
            );
        }
    }

    /// Records which command ran, in registration order.
    #[derive(Resource, Default)]
    struct Calls(Vec<&'static str>);

    /// The two commands the physics plugin registers, with handlers that record the call.
    fn register_skyrim_commands(app: &mut App) {
        app.init_resource::<Calls>();
        for (name, short, usage, help) in [
            (
                "ToggleCollision",
                Some("tcl"),
                "ToggleCollision",
                "toggle noclip flight and walking (key V)",
            ),
            (
                "ToggleCollisionGeometry",
                Some("tcg"),
                "ToggleCollisionGeometry",
                "toggle the collision outlines (also in Graphics settings)",
            ),
        ] {
            app.add_console_command(ConsoleCommand {
                name,
                short,
                usage,
                help,
                handler: Box::new(move |world, _| {
                    world.resource_mut::<Calls>().0.push(name);
                    Ok(String::new())
                }),
            });
        }
    }

    #[test]
    fn parse_line_splits_name_and_args() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   \t "), None);
        assert_eq!(parse_line("  Help  tcl  "), Some(("Help", vec!["tcl"])));
        assert_eq!(parse_line("a b c"), Some(("a", vec!["b", "c"])));
    }

    #[test]
    fn registry_lookup_is_case_insensitive_and_knows_short_forms() {
        let mut registry = ConsoleRegistry::default();
        registry.register(ConsoleCommand {
            name: "ToggleCollision",
            short: Some("tcl"),
            usage: "ToggleCollision",
            help: "toggle",
            handler: Box::new(|_, _| Ok(String::new())),
        });
        assert!(registry.get("togglecollision").is_some());
        assert!(registry.get("ToggleCollision").is_some());
        assert_eq!(registry.get("TCL").map(|c| c.name), Some("ToggleCollision"));
        assert!(registry.get("noclip").is_none());
        assert_eq!(registry.names(), vec!["ToggleCollision", "tcl"]);
    }

    #[test]
    fn completion_covers_none_unique_prefix_and_list() {
        assert_eq!(completion("zz", NAMES), Completion::None);
        assert_eq!(completion("h", NAMES), Completion::Unique("Help".into()));
        assert_eq!(
            completion("Clear", NAMES),
            Completion::Unique("ClearConsole".into())
        );
        // An exact name in the wrong case completes to the registered casing.
        assert_eq!(
            completion("CLEARCONSOLE", NAMES),
            Completion::Unique("ClearConsole".into())
        );
        assert_eq!(completion("tcg", NAMES), Completion::Unique("tcg".into()));
        // `tc` only shares its two letters with the two short forms.
        assert_eq!(
            completion("tc", NAMES),
            Completion::List(vec!["tcg".into(), "tcl".into()])
        );
        assert_eq!(
            completion("Tog", NAMES),
            Completion::Prefix("ToggleCollision".into())
        );
        assert_eq!(
            completion("t", NAMES),
            Completion::List(vec![
                "ToggleCollision".into(),
                "ToggleCollisionGeometry".into(),
                "tcg".into(),
                "tcl".into()
            ])
        );
        assert_eq!(
            completion("c", &["calc", "calm", "cold"]),
            Completion::List(vec!["calc".into(), "calm".into(), "cold".into()])
        );
        assert_eq!(
            completion("ca", &["calc", "calm", "cold"]),
            Completion::Prefix("cal".into())
        );
    }

    #[test]
    fn autofill_completes_only_the_first_token() {
        // `tc` is a prefix of both short forms and of nothing else: list them.
        assert_eq!(
            autofill("tc", NAMES),
            ("tc".to_owned(), vec!["tcg".into(), "tcl".into()])
        );
        assert_eq!(
            autofill("Tog", NAMES),
            ("ToggleCollision".to_owned(), vec![])
        );
        assert_eq!(autofill("Cle", NAMES), ("ClearConsole ".to_owned(), vec![]));
        // A name typed in the wrong case is filled in with the registered casing.
        assert_eq!(autofill("TCL", NAMES), ("tcl ".to_owned(), vec![]));
        assert_eq!(autofill("togglecollision", NAMES).0, "ToggleCollision ");
        assert_eq!(autofill("tcl foo", NAMES).0, "tcl foo");
        assert_eq!(autofill("qq", NAMES).0, "qq");
        assert_eq!(autofill("  tcg", NAMES).0, "  tcg ");
        let wide = ["café", "cafétéria"];
        assert_eq!(completion("c", &wide), Completion::Prefix("café".into()));
        assert_eq!(completion("ca", &["éa", "éb"]), Completion::None);
        assert_eq!(
            completion("", &["éa", "éb"]),
            Completion::Prefix("é".into())
        );
    }

    #[test]
    fn levenshtein_and_suggestion() {
        assert_eq!(levenshtein("tcl", "tcl"), 0);
        assert_eq!(levenshtein("tcll", "tcl"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(
            unknown_command_message("tcll", NAMES),
            "unknown command \"tcll\", did you mean \"tcl\"? Type \"Help\" for the list."
        );
        let none = unknown_command_message("zzzzzzzz", NAMES);
        assert_eq!(
            none,
            "unknown command \"zzzzzzzz\". Type \"Help\" for the list."
        );
        assert_eq!(nearest("TCLl", NAMES).as_deref(), Some("tcl"));
        assert_eq!(nearest("HELP", NAMES).as_deref(), Some("Help"));
        // Removed names are not close to any real one.
        assert_eq!(nearest("noclip", NAMES), None);
        assert_eq!(nearest("qqqqq", NAMES), None);
        // A suggestion must be closer than the input is long: `t` gets none, `tc` does.
        assert_eq!(nearest("t", NAMES), None);
        assert_eq!(nearest("tc", NAMES).as_deref().map(|n| &n[..2]), Some("tc"));
        assert_eq!(
            nearest("tcx", NAMES).as_deref().map(|n| &n[..2]),
            Some("tc")
        );
    }

    #[test]
    fn command_results_are_complete_independent_of_scrollback() {
        let mut app = console_app();
        let output = (0..SCROLLBACK_CAP + 25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = output.clone();
        app.add_console_command(ConsoleCommand {
            name: "ManyLines",
            short: None,
            usage: "ManyLines",
            help: "test complete output",
            handler: Box::new(move |_, _| Ok(output.clone())),
        });
        assert_eq!(execute_line(app.world_mut(), "ManyLines"), expected);
        assert_eq!(
            app.world().resource::<ConsoleState>().scrollback.len(),
            SCROLLBACK_CAP
        );
        assert_eq!(execute_line(app.world_mut(), "ClearConsole"), "");
        assert!(app.world().resource::<ConsoleState>().scrollback.is_empty());
        assert_eq!(execute_line(app.world_mut(), "  "), "");
        let help = execute_line(app.world_mut(), "Help");
        assert!(help.lines().count() >= 3);
        app.add_console_command(ConsoleCommand {
            name: "Failure",
            short: None,
            usage: "Failure",
            help: "test errors",
            handler: Box::new(|_, _| Err("first\nsecond".into())),
        });
        assert_eq!(
            execute_line(app.world_mut(), "Failure"),
            "error: first\nsecond"
        );
        assert!(execute_line(app.world_mut(), "Unknown").contains("unknown command"));
    }

    #[test]
    fn scrollback_is_capped_and_multiline_text_splits() {
        let mut lines = Vec::new();
        for i in 0..SCROLLBACK_CAP + 25 {
            push_scrollback(&mut lines, &format!("line {i}"));
        }
        assert_eq!(lines.len(), SCROLLBACK_CAP);
        assert_eq!(lines[0], "line 25");
        push_scrollback(&mut lines, "a\nb");
        assert_eq!(lines.len(), SCROLLBACK_CAP);
        assert_eq!(lines.last().map(String::as_str), Some("b"));
        let shown = visible_scrollback(&lines);
        assert_eq!(shown.lines().count(), VISIBLE_LINES);
    }

    #[test]
    fn insert_text_drops_control_characters() {
        let mut buffer = String::new();
        insert_text(&mut buffer, "ab\r\t");
        insert_text(&mut buffer, "c");
        assert_eq!(buffer, "abc");
        insert_text(&mut buffer, &"x".repeat(400));
        assert_eq!(buffer.chars().count(), INPUT_CAP);
        insert_text(&mut buffer, "y");
        assert_eq!(buffer.chars().count(), INPUT_CAP);
        assert!(!buffer.contains('y'));
    }

    #[test]
    fn opening_console_mid_jump_drops_input_but_keeps_the_airborne_state() {
        use crate::physics::{SprintLatch, WalkIntent, WalkState};
        let mut app = console_app();
        app.insert_resource(WalkIntent {
            jump_pressed: true,
            ..default()
        })
        .insert_resource(SprintLatch(true));
        let body = app
            .world_mut()
            .spawn(WalkState {
                vertical_velocity: 200.0,
                grounded: false,
                jump_active: true,
                ..default()
            })
            .id();
        send_state(&mut app, KeyCode::Backquote, ButtonState::Pressed);
        assert!(app.world().resource::<ConsoleState>().open);
        assert!(!app.world().resource::<WalkIntent>().jump_pressed);
        assert!(!app.world().resource::<SprintLatch>().0);
        let state = app.world().get::<WalkState>(body).unwrap();
        assert_eq!(state.vertical_velocity, 200.0);
        assert!(state.jump_active);
        assert!(!state.grounded);
    }

    #[test]
    fn console_closed_is_true_without_the_resource() {
        let mut world = World::new();
        assert!(world.run_system_once(console_closed).unwrap());
        world.insert_resource(ConsoleState::default());
        assert!(world.run_system_once(console_closed).unwrap());
        world.resource_mut::<ConsoleState>().open = true;
        assert!(!world.run_system_once(console_closed).unwrap());
    }

    #[test]
    fn every_command_answers_to_its_long_and_short_names_in_any_case() {
        let mut app = console_app();
        register_skyrim_commands(&mut app);
        for line in [
            "ToggleCollision",
            "togglecollision",
            "TOGGLECOLLISION",
            "tcl",
            "TCL",
            "tCl",
        ] {
            execute_line(app.world_mut(), line);
            assert_eq!(
                app.world().resource::<Calls>().0,
                ["ToggleCollision"],
                "{line}"
            );
            app.world_mut().resource_mut::<Calls>().0.clear();
        }
        for line in [
            "ToggleCollisionGeometry",
            "togglecollisiongeometry",
            "TcG",
            "tcg",
            "TCG",
        ] {
            execute_line(app.world_mut(), line);
            assert_eq!(
                app.world().resource::<Calls>().0,
                ["ToggleCollisionGeometry"],
                "{line}"
            );
            app.world_mut().resource_mut::<Calls>().0.clear();
        }
        // Help has a long name only.
        execute_line(app.world_mut(), "hElP");
        let text = scrollback(&app);
        for line in [
            "Help [text] - list every command",
            "ClearConsole - clear the console scrollback",
            "ToggleCollision [tcl] - toggle noclip flight and walking (key V)",
            "ToggleCollisionGeometry [tcg] - toggle the collision outlines (also in Graphics settings)",
        ] {
            assert!(text.contains(line), "Help is missing {line}: {text}");
        }
        // ClearConsole has a long name only.
        execute_line(app.world_mut(), "CLEARCONSOLE");
        assert!(app.world().resource::<ConsoleState>().scrollback.is_empty());
    }

    #[test]
    fn help_filters_by_name_short_form_and_description() {
        let mut app = console_app();
        register_skyrim_commands(&mut app);
        execute_line(app.world_mut(), "Help toggle");
        let text = scrollback(&app);
        assert!(text.contains("ToggleCollision [tcl]"), "{text}");
        assert!(text.contains("ToggleCollisionGeometry [tcg]"), "{text}");
        assert!(!text.contains("ClearConsole"), "{text}");
        // A short form as the filter text matches its command only.
        execute_line(app.world_mut(), "ClearConsole");
        execute_line(app.world_mut(), "Help TCG");
        let text = scrollback(&app);
        assert!(text.contains("ToggleCollisionGeometry [tcg]"), "{text}");
        assert!(!text.contains("ToggleCollision [tcl]"), "{text}");
        // The description counts too, whatever the case.
        execute_line(app.world_mut(), "ClearConsole");
        execute_line(app.world_mut(), "Help FLIGHT");
        let text = scrollback(&app);
        assert!(text.contains("ToggleCollision [tcl]"), "{text}");
        assert!(!text.contains("ToggleCollisionGeometry"), "{text}");
        execute_line(app.world_mut(), "Help zzz");
        assert!(scrollback(&app).contains("no command matches \"zzz\""));
        execute_line(app.world_mut(), "Help a b");
        assert!(scrollback(&app).contains("error: usage: Help [text]"));
    }

    #[test]
    fn removed_commands_and_aliases_are_unknown_with_a_suggestion_only_when_close() {
        let mut app = console_app();
        register_skyrim_commands(&mut app);
        let removed = [
            "noclip",
            "collision",
            "clear",
            "tankard",
            "grab",
            "v",
            "f3",
            "t",
            "e",
        ];
        for line in removed {
            execute_line(app.world_mut(), line);
        }
        let text = scrollback(&app);
        for line in removed {
            assert!(
                text.contains(&format!("unknown command \"{line}\"")),
                "unknown missing for {line}: {text}"
            );
            // `t` (the old tankard alias) is two edits from `tcl`/`tcg`, but a suggestion
            // must be closer than the input is long, so nothing is offered for it.
            assert!(
                !text.contains(&format!("unknown command \"{line}\", did you mean")),
                "unexpected suggestion for {line}: {text}"
            );
        }
        assert!(
            app.world().resource::<Calls>().0.is_empty(),
            "an unknown command ran something"
        );
    }

    #[test]
    fn unknown_command_runs_nothing() {
        let mut app = console_app();
        register_skyrim_commands(&mut app);
        execute_line(app.world_mut(), "togglecollisio");
        let text = scrollback(&app);
        assert!(text.contains("did you mean \"ToggleCollision\""), "{text}");
        assert!(app.world().resource::<Calls>().0.is_empty());
    }

    #[test]
    fn typed_backtick_never_reaches_the_buffer() {
        let mut app = console_app();
        // The opening press carries text "`" like a real keyboard event.
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Backquote,
            logical_key: Key::Character("`".into()),
            state: ButtonState::Pressed,
            text: Some("`".into()),
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
        send_state(&mut app, KeyCode::Backquote, ButtonState::Released);
        assert!(app.world().resource::<ConsoleState>().open);
        assert!(app.world().resource::<ConsoleState>().buffer.is_empty());
        type_char(&mut app, KeyCode::KeyH, "h");
        type_char(&mut app, KeyCode::KeyI, "i");
        assert_eq!(app.world().resource::<ConsoleState>().buffer, "hi");
        // The closing backtick toggles and is not typed either.
        type_key(
            &mut app,
            KeyCode::Backquote,
            Key::Character("`".into()),
            Some("`"),
        );
        send_state(&mut app, KeyCode::Backquote, ButtonState::Released);
        let state = app.world().resource::<ConsoleState>();
        assert!(!state.open);
        assert!(state.buffer.is_empty(), "buffer: {:?}", state.buffer);
        press_backquote(&mut app);
        assert!(app.world().resource::<ConsoleState>().open);
        assert!(app.world().resource::<ConsoleState>().buffer.is_empty());
    }

    #[test]
    fn closing_frame_is_not_closed_for_gameplay() {
        let mut app = console_app();
        press_backquote(&mut app);
        press_backquote(&mut app);
        // Closed, and no longer closing after the release frame.
        assert!(!app.world().resource::<ConsoleState>().closing);
        let mut world = World::new();
        world.insert_resource(ConsoleState {
            closing: true,
            ..default()
        });
        assert!(!world.run_system_once(console_closed).unwrap());
    }

    #[test]
    fn input_line_is_capped() {
        let mut app = console_app();
        press_backquote(&mut app);
        for _ in 0..INPUT_CAP + 10 {
            type_char(&mut app, KeyCode::KeyA, "a");
        }
        assert_eq!(
            app.world()
                .resource::<ConsoleState>()
                .buffer
                .chars()
                .count(),
            INPUT_CAP
        );
    }

    #[test]
    fn enter_runs_the_line_and_tab_autofills() {
        let mut app = console_app();
        press_backquote(&mut app);
        for (code, c) in [(KeyCode::KeyC, "c"), (KeyCode::KeyL, "l")] {
            type_char(&mut app, code, c);
        }
        type_key(&mut app, KeyCode::Tab, Key::Tab, None);
        assert_eq!(
            app.world().resource::<ConsoleState>().buffer,
            "ClearConsole "
        );
        type_key(&mut app, KeyCode::Enter, Key::Enter, None);
        let state = app.world().resource::<ConsoleState>();
        assert!(state.buffer.is_empty() && state.pending.is_empty());
        assert!(
            state.scrollback.is_empty(),
            "ClearConsole emptied it, got {:?}",
            state.scrollback
        );
    }

    #[derive(Resource, Default)]
    struct Probe(u32);

    fn probe_app() -> App {
        let mut app = console_app();
        app.init_resource::<Probe>()
            .add_systems(FixedUpdate, |mut probe: ResMut<Probe>| probe.0 += 1);
        app
    }

    #[test]
    fn open_pauses_virtual_time_and_close_resumes() {
        let mut app = probe_app();
        for _ in 0..5 {
            app.update();
        }
        assert!(app.world().resource::<Probe>().0 > 0, "probe never ran");
        press_backquote(&mut app);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        app.update();
        let frozen = app.world().resource::<Probe>().0;
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(app.world().resource::<Probe>().0, frozen);
        press_backquote(&mut app);
        assert!(!app.world().resource::<Time<Virtual>>().is_paused());
        for _ in 0..10 {
            app.update();
        }
        assert!(app.world().resource::<Probe>().0 > frozen);
    }

    #[test]
    fn close_keeps_a_pause_that_predates_the_console() {
        let mut app = console_app();
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        press_backquote(&mut app);
        assert!(app.world().resource::<ConsoleState>().was_paused);
        press_backquote(&mut app);
        assert!(!app.world().resource::<ConsoleState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
    }

    #[test]
    fn open_releases_the_cursor_and_close_restores_it() {
        let mut app = console_app();
        app.insert_resource(CursorCapture::Captured);
        press_backquote(&mut app);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        press_backquote(&mut app);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        press_backquote(&mut app);
        type_key(&mut app, KeyCode::Escape, Key::Escape, None);
        assert!(!app.world().resource::<ConsoleState>().open);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
    }
}
