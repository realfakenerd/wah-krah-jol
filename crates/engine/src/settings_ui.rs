//! Plain Bevy UI for settings and control reference; independent of the eventual Skyrim skin.

use crate::{
    console::{ConsoleInputSet, ConsoleState},
    physics::{CursorCapture, MotionReset},
    settings::{
        RenderingSettings, SETTING_SPECS, SettingKind, SettingSpec, SettingsFile, SettingsPage,
        apply_render_settings,
    },
};
use bevy::{
    input::{
        InputSystems,
        mouse::{MouseScrollUnit, MouseWheel},
    },
    input_focus::{
        FocusCause, InputFocus, InputFocusPlugin, InputFocusSystems,
        tab_navigation::{TabGroup, TabIndex, handle_tab_navigation},
    },
    prelude::*,
    render::renderer::RenderAdapterInfo,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

#[derive(Resource)]
pub struct SettingsMenuState {
    pub open: bool,
    pub closing: bool,
    pub page: SettingsPage,
    previous_capture: CursorCapture,
    was_paused: bool,
}

impl Default for SettingsMenuState {
    fn default() -> Self {
        Self {
            open: false,
            closing: false,
            page: SettingsPage::Graphics,
            previous_capture: CursorCapture::Released,
            was_paused: false,
        }
    }
}

impl SettingsMenuState {
    pub fn blocks_gameplay(&self) -> bool {
        self.open || self.closing
    }

    fn show(
        &mut self,
        capture: &mut CursorCapture,
        time: &mut Time<Virtual>,
        motion: &mut MotionReset,
    ) {
        self.open = true;
        self.previous_capture = *capture;
        self.was_paused = time.is_paused();
        *capture = CursorCapture::Released;
        time.pause();
        motion.clear_input();
    }

    fn close(&mut self, capture: &mut CursorCapture, time: &mut Time<Virtual>) {
        self.open = false;
        self.closing = true;
        *capture = CursorCapture::Released;
        if !self.was_paused {
            time.unpause();
        }
    }
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SettingsUiSet;

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RenderingSettings>()
            .init_resource::<SettingsMenuState>()
            .init_resource::<CursorCapture>()
            .init_resource::<InputFocus>();
        if !app.world().contains_resource::<SettingsFile>() {
            app.insert_resource(SettingsFile::session_only());
        }
        if !app.is_plugin_added::<InputFocusPlugin>() {
            app.add_plugins(InputFocusPlugin);
        }
        app.add_systems(Startup, spawn_settings_ui)
            .add_systems(
                PreUpdate,
                menu_keyboard
                    .after(InputSystems)
                    .before(InputFocusSystems::Dispatch)
                    .before(ConsoleInputSet),
            )
            .add_systems(
                Update,
                (
                    menu_actions,
                    apply_render_settings,
                    refresh_menu,
                    style_rows,
                    scroll_menu,
                    scroll_focused_button,
                    update_scrollbar,
                )
                    .chain()
                    .in_set(SettingsUiSet),
            );
    }
}

const TEXT: Color = Color::srgb(0.9, 0.92, 0.95);
const TEXT_MUTED: Color = Color::srgb(0.58, 0.63, 0.7);
const TEXT_DIM: Color = Color::srgb(0.38, 0.42, 0.48);
const ACCENT: Color = Color::srgb(0.45, 0.7, 1.0);
const ACCENT_DIM: Color = Color::srgb(0.2, 0.3, 0.42);
const GOOD: Color = Color::srgb(0.5, 0.85, 0.62);
const WARNING: Color = Color::srgb(1.0, 0.68, 0.4);
const PANEL: Color = Color::srgb(0.055, 0.065, 0.08);
const SEPARATOR: Color = Color::srgb(0.12, 0.14, 0.18);
const BORDER: Color = Color::srgb(0.26, 0.3, 0.36);
const FOCUS: Color = Color::srgb(0.85, 0.93, 1.0);
const KEYCAP: Color = Color::srgb(0.1, 0.12, 0.16);
const PAGES: [SettingsPage; 3] = [
    SettingsPage::Graphics,
    SettingsPage::Streaming,
    SettingsPage::Controls,
];

/// Resting, hovered, and pressed backgrounds for one button style.
type ButtonStyle = (Color, Color, Color);
const NEUTRAL: ButtonStyle = (
    Color::srgb(0.13, 0.16, 0.21),
    Color::srgb(0.2, 0.26, 0.34),
    Color::srgb(0.28, 0.38, 0.5),
);
const TAB_ACTIVE: ButtonStyle = (
    Color::srgb(0.17, 0.25, 0.36),
    Color::srgb(0.21, 0.31, 0.44),
    Color::srgb(0.28, 0.38, 0.5),
);
const PRIMARY: ButtonStyle = (
    Color::srgb(0.2, 0.4, 0.68),
    Color::srgb(0.26, 0.48, 0.8),
    Color::srgb(0.32, 0.54, 0.88),
);
const SWITCH_ON: ButtonStyle = (
    Color::srgb(0.13, 0.38, 0.27),
    Color::srgb(0.17, 0.46, 0.33),
    Color::srgb(0.22, 0.54, 0.4),
);

#[derive(Component)]
struct MenuRoot;
#[derive(Component)]
struct MenuLauncher;
/// Holds one page's scroll area and scrollbar; hidden unless that page is selected.
#[derive(Component)]
struct PageFrame(SettingsPage);
#[derive(Component)]
struct PageRoot(SettingsPage);
#[derive(Component)]
struct PageScrollbar(SettingsPage);
#[derive(Component)]
struct SettingLabel(usize);
#[derive(Component)]
struct SettingValue(usize);
/// Accent bar at the start of a row whose value differs from the defaults.
#[derive(Component)]
struct SettingMark(usize);
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct RendererText;
#[derive(Component)]
struct CollisionText;
#[derive(Component, Clone, Copy)]
enum MenuAction {
    Open,
    Page(SettingsPage),
    Adjust(usize, i32),
    CollisionDebug,
    Save,
    Defaults,
    Resume,
}

impl MenuAction {
    fn available(self, menu: &SettingsMenuState, console_open: bool) -> bool {
        if console_open || menu.closing {
            return false;
        }
        match self {
            Self::Open => !menu.open,
            Self::Adjust(index, _) => menu.open && SETTING_SPECS[index].page == menu.page,
            Self::CollisionDebug => menu.open && menu.page == SettingsPage::Graphics,
            _ => menu.open,
        }
    }
}

fn step_page(page: SettingsPage, direction: i32) -> SettingsPage {
    let current = PAGES.iter().position(|candidate| *candidate == page);
    let next = (current.unwrap_or(0) as i32 + direction).rem_euclid(PAGES.len() as i32);
    PAGES[next as usize]
}

fn button_style(
    action: MenuAction,
    active: bool,
    settings: &RenderingSettings,
    collision_on: bool,
) -> ButtonStyle {
    match action {
        MenuAction::Save => PRIMARY,
        MenuAction::Page(_) if active => TAB_ACTIVE,
        MenuAction::Adjust(index, _) => {
            let spec = &SETTING_SPECS[index];
            if matches!(spec.kind, SettingKind::Toggle) && (spec.read)(settings) != 0.0 {
                SWITCH_ON
            } else {
                NEUTRAL
            }
        }
        MenuAction::CollisionDebug if collision_on => SWITCH_ON,
        _ => NEUTRAL,
    }
}

#[allow(clippy::too_many_arguments)]
fn menu_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    console: Option<Res<ConsoleState>>,
    mut menu: ResMut<SettingsMenuState>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    mut motion: MotionReset,
    mut options: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mut focus: ResMut<InputFocus>,
) {
    if menu.closing {
        menu.closing = false;
        *capture = menu.previous_capture;
    }
    if console.is_some_and(|console| console.open || console.closing) {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        if menu.open {
            menu.close(&mut capture, &mut time);
            focus.clear();
        } else {
            menu.show(&mut capture, &mut time, &mut motion);
        }
    }
    if menu.blocks_gameplay() {
        for mut cursor in &mut options {
            cursor.grab_mode = CursorGrabMode::None;
            cursor.visible = true;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn menu_actions(
    mut menu: ResMut<SettingsMenuState>,
    mut settings: ResMut<RenderingSettings>,
    mut file: ResMut<SettingsFile>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<
        (
            Entity,
            Ref<Interaction>,
            &MenuAction,
            &mut BackgroundColor,
            &mut BorderColor,
        ),
        With<Button>,
    >,
    mut debug: Option<ResMut<bevy_rapier3d::prelude::DebugRenderContext>>,
    mut pages: Query<&mut ScrollPosition, With<PageRoot>>,
    mut motion: MotionReset,
    console: Option<Res<ConsoleState>>,
) {
    let keyboard_activation = keys.just_pressed(KeyCode::Enter)
        || keys.just_pressed(KeyCode::NumpadEnter)
        || keys.just_pressed(KeyCode::Space);
    // Left / Right nudge the focused control, or move between tabs when a tab has focus.
    let sideways = i32::from(keys.just_pressed(KeyCode::ArrowRight))
        - i32::from(keys.just_pressed(KeyCode::ArrowLeft));
    let collision_on = debug.as_deref().is_some_and(|debug| debug.enabled);
    let mut actions = Vec::new();
    let mut tab_focus = None;
    for (entity, interaction, action, mut background, mut border) in &mut buttons {
        if !action.available(&menu, console.as_ref().is_some_and(|c| c.open || c.closing)) {
            continue;
        }
        let focused = focus.get() == Some(entity);
        let active = matches!(action, MenuAction::Page(page) if *page == menu.page);
        let (rest, hover, pressed) = button_style(*action, active, &settings, collision_on);
        let color = match *interaction {
            Interaction::Pressed => pressed,
            Interaction::Hovered => hover,
            Interaction::None => rest,
        };
        background.set_if_neq(BackgroundColor(color));
        border.set_if_neq(BorderColor::all(if focused {
            FOCUS
        } else if active {
            ACCENT
        } else {
            BORDER
        }));
        // Interactions remain Pressed while the mouse is held; only the transition activates.
        if (interaction.is_changed() && *interaction == Interaction::Pressed)
            || (focused && keyboard_activation)
        {
            focus.set(entity, FocusCause::Pressed);
            actions.push(*action);
        } else if focused && sideways != 0 {
            match action {
                MenuAction::Adjust(index, _) => actions.push(MenuAction::Adjust(*index, sideways)),
                MenuAction::Page(_) => {
                    let page = step_page(menu.page, sideways);
                    actions.push(MenuAction::Page(page));
                    tab_focus = Some(page);
                }
                _ => {}
            }
        }
    }
    for action in actions {
        match action {
            MenuAction::Open => {
                menu.show(&mut capture, &mut time, &mut motion);
            }
            MenuAction::Page(page) => {
                menu.page = page;
                for mut position in &mut pages {
                    position.y = 0.0;
                }
            }
            MenuAction::Adjust(index, direction) => {
                SETTING_SPECS[index].adjust(&mut settings, direction);
                file.status = "Changed for this session. Choose Save to keep settings.".into();
            }
            MenuAction::CollisionDebug => {
                if let Some(debug) = debug.as_deref_mut() {
                    debug.enabled = !debug.enabled;
                } else {
                    file.status = "Collision outlines are unavailable in this session.".into();
                }
            }
            MenuAction::Save => file.save(&settings),
            MenuAction::Defaults => {
                file.restore_defaults(&mut settings);
            }
            MenuAction::Resume => {
                menu.close(&mut capture, &mut time);
                focus.clear();
            }
        }
    }
    // Keyboard tab switching keeps focus on the tab that is now showing.
    if let Some(page) = tab_focus {
        for (entity, _, action, _, _) in &buttons {
            if matches!(action, MenuAction::Page(candidate) if *candidate == page) {
                focus.set(entity, FocusCause::Navigated);
            }
        }
    }
}

fn text_bundle(text: impl Into<String>, size: f32) -> impl Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor(TEXT),
    )
}

fn muted_text(text: impl Into<String>, size: f32) -> impl Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor(TEXT_MUTED),
    )
}

fn button_node(width: f32) -> Node {
    Node {
        min_width: Val::Px(width),
        min_height: Val::Px(32.0),
        flex_shrink: 0.0,
        padding: UiRect::axes(Val::Px(10.0), Val::Px(5.0)),
        border: UiRect::all(Val::Px(1.0)),
        align_items: AlignItems::Center,
        justify_content: JustifyContent::Center,
        ..default()
    }
}

fn button(parent: &mut ChildSpawnerCommands, label: &str, action: MenuAction, width: f32) {
    parent
        .spawn((
            Button,
            action,
            TabIndex(0),
            button_node(width),
            BackgroundColor(NEUTRAL.0),
            BorderColor::all(BORDER),
        ))
        .with_children(|parent| {
            parent.spawn(text_bundle(label, 16.0));
        });
}

/// A button whose caption is live text, so it reads as a switch showing its state.
fn switch(parent: &mut ChildSpawnerCommands, action: MenuAction, width: f32, caption: impl Bundle) {
    parent
        .spawn((
            Button,
            action,
            TabIndex(0),
            button_node(width),
            BackgroundColor(NEUTRAL.0),
            BorderColor::all(BORDER),
        ))
        .with_children(|parent| {
            parent.spawn(caption);
        });
}

fn row_node() -> Node {
    Node {
        min_height: Val::Px(42.0),
        flex_shrink: 0.0,
        align_items: AlignItems::Center,
        column_gap: Val::Px(10.0),
        padding: UiRect::horizontal(Val::Px(8.0)),
        border: UiRect::bottom(Val::Px(1.0)),
        ..default()
    }
}

fn group_heading(parent: &mut ChildSpawnerCommands, title: &str) {
    parent
        .spawn((
            Node {
                margin: UiRect::top(Val::Px(18.0)),
                padding: UiRect::new(Val::Px(8.0), Val::Px(8.0), Val::Px(0.0), Val::Px(6.0)),
                border: UiRect::bottom(Val::Px(1.0)),
                flex_shrink: 0.0,
                ..default()
            },
            BorderColor::all(ACCENT_DIM),
        ))
        .with_children(|heading| {
            heading.spawn((
                Text::new(title.to_uppercase()),
                TextFont::from_font_size(14.0),
                TextColor(ACCENT),
            ));
        });
}

fn note(parent: &mut ChildSpawnerCommands, text: &str) -> Entity {
    parent
        .spawn((
            muted_text(text, 14.0),
            Node {
                margin: UiRect::new(Val::Px(8.0), Val::Px(8.0), Val::Px(8.0), Val::Px(0.0)),
                flex_shrink: 0.0,
                ..default()
            },
        ))
        .id()
}

fn setting_row(
    parent: &mut ChildSpawnerCommands,
    index: usize,
    spec: &SettingSpec,
    settings: &RenderingSettings,
) {
    let value = spec.value_label(settings);
    parent
        .spawn((row_node(), BorderColor::all(SEPARATOR)))
        .with_children(|row| {
            row.spawn((
                SettingMark(index),
                Node {
                    width: Val::Px(4.0),
                    height: Val::Px(20.0),
                    flex_shrink: 0.0,
                    ..default()
                },
                BackgroundColor(Color::NONE),
            ));
            row.spawn((
                SettingLabel(index),
                text_bundle(spec.label, 16.0),
                Node {
                    flex_grow: 1.0,
                    flex_basis: Val::Px(0.0),
                    ..default()
                },
            ));
            row.spawn(Node {
                width: Val::Px(300.0),
                flex_shrink: 0.0,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexEnd,
                column_gap: Val::Px(8.0),
                ..default()
            })
            .with_children(|controls| {
                if matches!(spec.kind, SettingKind::Toggle) {
                    switch(
                        controls,
                        MenuAction::Adjust(index, 1),
                        96.0,
                        (SettingValue(index), text_bundle(value, 16.0)),
                    );
                    return;
                }
                let (less, more) = if matches!(spec.kind, SettingKind::Choice(_)) {
                    ("<", ">")
                } else {
                    ("-", "+")
                };
                button(controls, less, MenuAction::Adjust(index, -1), 36.0);
                controls.spawn((
                    SettingValue(index),
                    text_bundle(value, 16.0),
                    TextLayout::justify(Justify::Center),
                    Node {
                        flex_grow: 1.0,
                        flex_basis: Val::Px(0.0),
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                ));
                button(controls, more, MenuAction::Adjust(index, 1), 36.0);
            });
        });
}

/// Context shown after the last setting of a group.
fn group_note(group: &str) -> Option<&'static str> {
    match group {
        "Culling and drawing" => Some(
            "Room culling: not available yet (#188). Occlusion culling needs depth prepass \
             and indirect drawing; enabling it enables both.",
        ),
        "Image" => Some(
            "HDR composition stays on. Material colors, texture tiling, and per-water factors \
             come from converted assets. Visual profile requires Save and a restart.",
        ),
        "Threads" => Some(
            "Distance changes update streaming around the current camera. IO threads require \
             Save and a restart.",
        ),
        _ => None,
    }
}

fn spawn_settings(
    body: &mut ChildSpawnerCommands,
    page: SettingsPage,
    settings: &RenderingSettings,
) {
    let mut current: Option<&str> = None;
    let close_group = |body: &mut ChildSpawnerCommands, group: Option<&str>| {
        if let Some(text) = group.and_then(group_note) {
            note(body, text);
        }
    };
    for (index, spec) in SETTING_SPECS
        .iter()
        .enumerate()
        .filter(|(_, spec)| spec.page == page)
    {
        if current != Some(spec.group) {
            close_group(body, current);
            group_heading(body, spec.group);
            current = Some(spec.group);
        }
        setting_row(body, index, spec, settings);
    }
    close_group(body, current);
    if page != SettingsPage::Graphics {
        return;
    }
    group_heading(body, "Debug tools");
    body.spawn((row_node(), BorderColor::all(SEPARATOR)))
        .with_children(|row| {
            row.spawn(Node {
                width: Val::Px(4.0),
                flex_shrink: 0.0,
                ..default()
            });
            row.spawn((
                text_bundle("Collision outlines (session only)", 16.0),
                Node {
                    flex_grow: 1.0,
                    flex_basis: Val::Px(0.0),
                    ..default()
                },
            ));
            row.spawn(Node {
                width: Val::Px(300.0),
                flex_shrink: 0.0,
                justify_content: JustifyContent::FlexEnd,
                ..default()
            })
            .with_children(|controls| {
                switch(
                    controls,
                    MenuAction::CollisionDebug,
                    96.0,
                    (CollisionText, text_bundle("", 16.0)),
                );
            });
        });
    let renderer = note(body, "");
    body.commands().entity(renderer).insert(RendererText);
}

fn spawn_header(panel: &mut ChildSpawnerCommands) {
    panel
        .spawn(Node {
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::FlexEnd,
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|header| {
            header.spawn(text_bundle("Settings", 28.0));
            header.spawn(muted_text(
                "Changes apply immediately. Save keeps them for the next launch.",
                14.0,
            ));
        });
}

fn spawn_tabs(panel: &mut ChildSpawnerCommands) {
    panel
        .spawn(Node {
            column_gap: Val::Px(8.0),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|tabs| {
            for (label, page) in [
                ("Graphics", SettingsPage::Graphics),
                ("Streaming", SettingsPage::Streaming),
                ("Controls", SettingsPage::Controls),
            ] {
                button(tabs, label, MenuAction::Page(page), 130.0);
            }
        });
}

fn spawn_page(panel: &mut ChildSpawnerCommands, page: SettingsPage, settings: &RenderingSettings) {
    panel
        .spawn((
            PageFrame(page),
            Node {
                display: if page == SettingsPage::Graphics {
                    Display::Flex
                } else {
                    Display::None
                },
                flex_grow: 1.0,
                flex_basis: Val::Px(0.0),
                min_height: Val::Px(0.0),
                ..default()
            },
        ))
        .with_children(|frame| {
            frame
                .spawn((
                    PageRoot(page),
                    ScrollPosition::default(),
                    Node {
                        flex_direction: FlexDirection::Column,
                        flex_grow: 1.0,
                        flex_basis: Val::Px(0.0),
                        min_width: Val::Px(0.0),
                        min_height: Val::Px(0.0),
                        overflow: Overflow::scroll_y(),
                        padding: UiRect::right(Val::Px(16.0)),
                        ..default()
                    },
                ))
                .with_children(|body| {
                    if page == SettingsPage::Controls {
                        spawn_controls(body);
                    } else {
                        spawn_settings(body, page, settings);
                    }
                });
            frame.spawn((
                PageScrollbar(page),
                Node {
                    display: Display::None,
                    position_type: PositionType::Absolute,
                    right: Val::Px(2.0),
                    top: Val::Percent(0.0),
                    width: Val::Px(6.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                BackgroundColor(BORDER),
            ));
        });
}

fn spawn_footer(panel: &mut ChildSpawnerCommands) {
    panel
        .spawn((
            Node {
                column_gap: Val::Px(8.0),
                align_items: AlignItems::Center,
                flex_shrink: 0.0,
                padding: UiRect::top(Val::Px(12.0)),
                border: UiRect::top(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(SEPARATOR),
        ))
        .with_children(|footer| {
            footer.spawn((
                StatusText,
                muted_text("", 14.0),
                Node {
                    flex_grow: 1.0,
                    flex_basis: Val::Px(0.0),
                    margin: UiRect::right(Val::Px(12.0)),
                    ..default()
                },
            ));
            button(footer, "Reset defaults", MenuAction::Defaults, 150.0);
            button(footer, "Resume", MenuAction::Resume, 110.0);
            button(footer, "Save", MenuAction::Save, 110.0);
        });
    panel.spawn((
        muted_text(
            "Escape resume    Tab move    Left / Right adjust    Enter / Space activate    Wheel / Page Up / Page Down scroll",
            13.0,
        ),
        Node {
            flex_shrink: 0.0,
            ..default()
        },
    ));
}

fn spawn_settings_ui(mut commands: Commands, settings: Res<RenderingSettings>) {
    commands
        .spawn((
            MenuLauncher,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(12.0),
                right: Val::Px(12.0),
                ..default()
            },
            GlobalZIndex(900),
        ))
        .with_children(|root| {
            button(
                root,
                "Settings / Controls (Escape)",
                MenuAction::Open,
                180.0,
            );
        });
    commands
        .spawn((
            Name::new("Settings menu"),
            MenuRoot,
            TabGroup::modal(),
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.65)),
            GlobalZIndex(2000),
        ))
        .observe(handle_tab_navigation)
        .with_children(|root| {
            root.spawn((
                Node {
                    width: Val::Percent(92.0),
                    max_width: Val::Px(1000.0),
                    height: Val::Percent(92.0),
                    padding: UiRect::all(Val::Px(20.0)),
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(12.0),
                    border: UiRect::all(Val::Px(1.0)),
                    ..default()
                },
                BackgroundColor(PANEL),
                BorderColor::all(BORDER),
            ))
            .with_children(|panel| {
                spawn_header(panel);
                spawn_tabs(panel);
                for page in PAGES {
                    spawn_page(panel, page, &settings);
                }
                spawn_footer(panel);
            });
        });
}

pub const CONTROL_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Menus and cursor",
        &[
            (
                "Escape",
                "Open / close settings and controls; close developer console",
            ),
            (
                "F3",
                "Release cursor while playing; left click captures it again",
            ),
            (
                "Left click",
                "Capture cursor while playing; select a world reference in the console; activate menu buttons",
            ),
            ("Backtick", "Open / close developer console"),
            (
                "Tab / Shift+Tab",
                "Next / previous menu control; Tab completes console commands",
            ),
            (
                "Left / Right",
                "Adjust the focused setting; switch tabs when a tab has focus",
            ),
            (
                "Enter / Space",
                "Activate focused menu button; Enter submits console command",
            ),
            (
                "Wheel / Page Up / Page Down",
                "Scroll menu; Home / End jump to top / bottom",
            ),
        ],
    ),
    (
        "Movement (cursor captured)",
        &[
            ("Mouse", "Look around"),
            ("W / A / S / D", "Move relative to view"),
            (
                "V",
                "Toggle NOCLIP / WALK; walking requires a clear capsule placement",
            ),
            ("Space", "Fly up in NOCLIP; jump in WALK"),
            ("Ctrl", "Fly down in NOCLIP"),
            ("Shift", "Fast flight in NOCLIP; slow walking in WALK"),
            ("Alt", "Toggle sprint in WALK; stops when movement stops"),
        ],
    ),
    (
        "Debug tools",
        &[
            (
                "Graphics tab",
                "Toggle collision outlines in the streamed world",
            ),
            ("T", "Spawn a debug tankard on terrain (cursor captured)"),
            ("E", "Grab / drop a debug tankard (cursor captured)"),
            (
                "Console: Help",
                "List available commands; Help <text> filters them",
            ),
            ("Console: tcl", "Toggle NOCLIP"),
            ("Console: tcg", "Toggle collision outlines"),
            (
                "Console: SceneGrade",
                "Color grade: off, saturation, full, snapshot-srgb, snapshot-linear or record-srgb (also in Graphics settings)",
            ),
        ],
    ),
];

fn spawn_controls(parent: &mut ChildSpawnerCommands) {
    for (heading, controls) in CONTROL_GROUPS {
        group_heading(parent, heading);
        for (keys, description) in *controls {
            parent
                .spawn((
                    Node {
                        padding: UiRect::vertical(Val::Px(6.0)),
                        ..row_node()
                    },
                    BorderColor::all(SEPARATOR),
                ))
                .with_children(|row| {
                    row.spawn(Node {
                        width: Val::Px(400.0),
                        flex_shrink: 0.0,
                        flex_wrap: FlexWrap::Wrap,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(6.0),
                        row_gap: Val::Px(4.0),
                        ..default()
                    })
                    .with_children(|cell| {
                        for (position, key) in keys.split(" / ").enumerate() {
                            if position > 0 {
                                cell.spawn(muted_text("/", 14.0));
                            }
                            cell.spawn((
                                Node {
                                    padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                                    border: UiRect::all(Val::Px(1.0)),
                                    ..default()
                                },
                                BackgroundColor(KEYCAP),
                                BorderColor::all(BORDER),
                            ))
                            .with_children(|cap| {
                                cap.spawn(text_bundle(key, 15.0));
                            });
                        }
                    });
                    row.spawn((
                        muted_text(*description, 15.0),
                        Node {
                            flex_grow: 1.0,
                            flex_basis: Val::Px(0.0),
                            ..default()
                        },
                    ));
                });
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn refresh_menu(
    menu: Res<SettingsMenuState>,
    settings: Res<RenderingSettings>,
    file: Res<SettingsFile>,
    adapter: Option<Res<RenderAdapterInfo>>,
    mut roots: Query<&mut Node, (With<MenuRoot>, Without<PageFrame>)>,
    mut pages: Query<(&PageFrame, &mut Node), Without<MenuRoot>>,
    mut launchers: Query<&mut Node, (With<MenuLauncher>, Without<MenuRoot>, Without<PageFrame>)>,
    mut text: Query<(
        &mut Text,
        Option<&SettingValue>,
        Has<StatusText>,
        Has<RendererText>,
        Has<CollisionText>,
    )>,
    collision: Option<Res<bevy_rapier3d::prelude::DebugRenderContext>>,
    console: Option<Res<ConsoleState>>,
    mut buttons: Query<(Entity, &MenuAction, &mut TabIndex)>,
    mut focus: ResMut<InputFocus>,
) {
    let console_open = console.as_ref().is_some_and(|c| c.open || c.closing);
    let mut focus_available = false;
    let mut initial_focus = None;
    // Bevy's tab traversal uses TabIndex, including on nodes hidden by Display::None.
    // Exclude the other pages and seed the modal group's focus when opening the menu.
    for (entity, action, mut index) in &mut buttons {
        let available = action.available(&menu, console_open);
        index.set_if_neq(TabIndex(if available { 0 } else { -1 }));
        focus_available |= available && focus.get() == Some(entity);
        if available && matches!(action, MenuAction::Page(page) if *page == menu.page) {
            initial_focus = Some(entity);
        }
    }
    if !focus_available {
        if let Some(entity) = initial_focus {
            focus.set(entity, FocusCause::Navigated);
        } else if focus.get().is_some() {
            focus.clear();
        }
    }
    for mut node in &mut roots {
        let display = if menu.open {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
    for (page, mut node) in &mut pages {
        let display = if page.0 == menu.page {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
    for mut node in &mut launchers {
        let display = if menu.open || console_open {
            Display::None
        } else {
            Display::Flex
        };
        if node.display != display {
            node.display = display;
        }
    }
    if !menu.open {
        return;
    }
    for (mut text, value, status, renderer, collision_text) in &mut text {
        let value = if let Some(value) = value {
            Some(SETTING_SPECS[value.0].value_label(&settings))
        } else if status {
            Some(file.status.clone())
        } else if renderer {
            Some(adapter.as_deref().map_or_else(
                || "Renderer information unavailable".into(),
                |adapter| {
                    format!(
                        "Renderer: {:?} / {}. Backend and GPU capabilities are selected at startup.",
                        adapter.backend, adapter.name
                    )
                },
            ))
        } else if collision_text {
            Some(
                collision
                    .as_deref()
                    .map_or(
                        "Unavailable",
                        |debug| {
                            if debug.enabled { "On" } else { "Off" }
                        },
                    )
                    .to_owned(),
            )
        } else {
            None
        };
        if let Some(value) = value
            && text.0 != value
        {
            text.0 = value;
        }
    }
}

/// Marks modified rows, dims settings whose controlling option is off, and colors the status.
#[allow(clippy::type_complexity)]
fn style_rows(
    menu: Res<SettingsMenuState>,
    settings: Res<RenderingSettings>,
    file: Res<SettingsFile>,
    mut labels: Query<(&SettingLabel, &mut TextColor), Without<SettingValue>>,
    mut values: Query<(&SettingValue, &mut TextColor), Without<SettingLabel>>,
    mut marks: Query<(&SettingMark, &mut BackgroundColor)>,
    mut status: Query<
        &mut TextColor,
        (
            With<StatusText>,
            Without<SettingLabel>,
            Without<SettingValue>,
        ),
    >,
) {
    if !menu.open {
        return;
    }
    for (label, mut color) in &mut labels {
        let wanted = if SETTING_SPECS[label.0].relevant(&settings) {
            TEXT
        } else {
            TEXT_DIM
        };
        if color.0 != wanted {
            color.0 = wanted;
        }
    }
    for (value, mut color) in &mut values {
        let wanted = if SETTING_SPECS[value.0].relevant(&settings) {
            TEXT
        } else {
            TEXT_DIM
        };
        if color.0 != wanted {
            color.0 = wanted;
        }
    }
    for (mark, mut background) in &mut marks {
        let modified = SETTING_SPECS[mark.0].is_modified(&settings);
        background.set_if_neq(BackgroundColor(if modified { ACCENT } else { Color::NONE }));
    }
    let wanted = if file.status.starts_with("Could not")
        || file.status.contains("ignored")
        || file.status.contains("unavailable")
    {
        WARNING
    } else if file.status.starts_with("Settings saved") {
        GOOD
    } else {
        TEXT_MUTED
    };
    for mut color in &mut status {
        if color.0 != wanted {
            color.0 = wanted;
        }
    }
}

fn scroll_focused_button(
    menu: Res<SettingsMenuState>,
    focus: Res<InputFocus>,
    nodes: Query<(&ComputedNode, &UiGlobalTransform)>,
    parents: Query<&ChildOf>,
    mut pages: Query<(
        &PageRoot,
        &ComputedNode,
        &UiGlobalTransform,
        &mut ScrollPosition,
    )>,
    mut previous: Local<Option<Entity>>,
) {
    if !menu.open || focus.get() == *previous {
        return;
    }
    *previous = focus.get();
    let Some(entity) = focus.get() else { return };
    let Ok((button, transform)) = nodes.get(entity) else {
        return;
    };
    for ancestor in parents.iter_ancestors(entity) {
        let Ok((page, viewport, viewport_transform, mut scroll)) = pages.get_mut(ancestor) else {
            continue;
        };
        if page.0 != menu.page {
            return;
        }
        let center = transform.translation.y - viewport_transform.translation.y;
        let top = center - button.size().y * 0.5 + viewport.size().y * 0.5;
        let bottom = top + button.size().y;
        let delta = if top < 0.0 {
            top
        } else if bottom > viewport.size().y {
            bottom - viewport.size().y
        } else {
            0.0
        };
        let scale = viewport.inverse_scale_factor();
        let maximum = ((viewport.content_size().y - viewport.size().y) * scale).max(0.0);
        scroll.y = (scroll.y + delta * scale).clamp(0.0, maximum);
        return;
    }
}

fn scroll_menu(
    menu: Res<SettingsMenuState>,
    keys: Res<ButtonInput<KeyCode>>,
    mut wheel: MessageReader<MouseWheel>,
    mut pages: Query<(&PageRoot, &ComputedNode, &mut ScrollPosition)>,
) {
    let mut delta = 0.0;
    for event in wheel.read() {
        delta -= event.y
            * if event.unit == MouseScrollUnit::Line {
                32.0
            } else {
                1.0
            };
    }
    if !menu.open {
        return;
    }
    if delta == 0.0
        && !keys.any_just_pressed([
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::End,
        ])
    {
        return;
    }
    for (page, computed, mut position) in &mut pages {
        if page.0 != menu.page {
            continue;
        }
        let height = computed.size().y * computed.inverse_scale_factor();
        let max = ((computed.content_size().y - computed.size().y)
            * computed.inverse_scale_factor())
        .max(0.0);
        if keys.just_pressed(KeyCode::PageDown) {
            delta += height * 0.8;
        }
        if keys.just_pressed(KeyCode::PageUp) {
            delta -= height * 0.8;
        }
        position.y = (position.y + delta).clamp(0.0, max);
        if keys.just_pressed(KeyCode::Home) {
            position.y = 0.0;
        }
        if keys.just_pressed(KeyCode::End) {
            position.y = max;
        }
    }
}

/// Sizes each page's scrollbar thumb from its scroll position; hides it when nothing scrolls.
fn update_scrollbar(
    menu: Res<SettingsMenuState>,
    pages: Query<(&PageRoot, &ComputedNode, &ScrollPosition)>,
    mut thumbs: Query<(&PageScrollbar, &mut Node)>,
) {
    if !menu.open {
        return;
    }
    for (page, computed, position) in &pages {
        if page.0 != menu.page {
            continue;
        }
        let view = computed.size().y;
        let content = computed.content_size().y;
        let scrolls = content > view + 1.0 && view > 0.0;
        let top = position.y / (content * computed.inverse_scale_factor()).max(1.0) * 100.0;
        for (thumb, mut node) in &mut thumbs {
            if thumb.0 != page.0 {
                continue;
            }
            let display = if scrolls {
                Display::Flex
            } else {
                Display::None
            };
            if node.display != display {
                node.display = display;
            }
            if scrolls {
                let (top, height) = (Val::Percent(top), Val::Percent(view / content * 100.0));
                if node.top != top {
                    node.top = top;
                }
                if node.height != height {
                    node.height = height;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::EngineConfig,
        console::{ConsolePlugin, console_closed},
        physics::{SprintLatch, WalkIntent},
    };
    use bevy::{
        ecs::system::RunSystemOnce,
        input::{
            ButtonState,
            keyboard::{Key, KeyboardInput},
        },
    };

    fn menu_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, ConsolePlugin))
            .insert_resource(EngineConfig::default())
            .add_plugins(SettingsPlugin);
        app.finish();
        app.update();
        app
    }

    fn key(app: &mut App, code: KeyCode, state: ButtonState) {
        app.world_mut().write_message(KeyboardInput {
            key_code: code,
            logical_key: Key::Character("test".into()),
            state,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
    }

    #[test]
    fn opening_menu_clears_motion_and_closing_restores_capture_after_one_frame() {
        let mut app = menu_app();
        app.insert_resource(CursorCapture::Captured)
            .insert_resource(WalkIntent {
                jump_pressed: true,
                ..default()
            })
            .insert_resource(SprintLatch(true));
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert!(!app.world().resource::<WalkIntent>().jump_pressed);
        assert!(!app.world().resource::<SprintLatch>().0);
        assert!(!app.world_mut().run_system_once(console_closed).unwrap());
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().closing);
        assert!(!app.world_mut().run_system_once(console_closed).unwrap());
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        assert!(app.world_mut().run_system_once(console_closed).unwrap());
        assert!(!app.world().resource::<Time<Virtual>>().is_paused());
    }

    /// A saved vsync and window size reach the window on the first frame. The run fingerprint must
    /// describe that window, not the one the renderer created before the settings were applied.
    #[test]
    fn run_fingerprint_records_the_window_the_first_frame_applies() {
        use bevy::window::{PresentMode, PrimaryWindow, Window};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, ConsolePlugin))
            .insert_resource(EngineConfig::default())
            .insert_resource(RenderingSettings {
                vsync: false,
                window_size: 2,
                ..RenderingSettings::default()
            })
            .add_plugins(SettingsPlugin)
            .add_plugins(crate::fingerprint::RunFingerprintPlugin);
        app.world_mut().spawn((
            PrimaryWindow,
            Window {
                present_mode: PresentMode::AutoVsync,
                resolution: (1280, 720).into(),
                ..Window::default()
            },
        ));
        app.finish();
        app.update();

        let world = app.world_mut();
        let window = world
            .query_filtered::<&Window, With<PrimaryWindow>>()
            .single(world)
            .unwrap();
        let fingerprint = world.resource::<crate::fingerprint::RunFingerprint>();
        assert_eq!(
            fingerprint.graphics.present_mode.as_deref(),
            Some(format!("{:?}", window.present_mode).as_str()),
            "the fingerprint records the vsync the first frame applied"
        );
        assert_eq!(
            fingerprint.resolution,
            Some([
                window.resolution.physical_width(),
                window.resolution.physical_height(),
            ]),
            "the fingerprint records the window size the first frame applied"
        );
    }

    #[test]
    fn opening_menu_mid_jump_keeps_the_airborne_state_for_resume() {
        let mut app = menu_app();
        let body = app
            .world_mut()
            .spawn(crate::physics::WalkState {
                vertical_velocity: 200.0,
                grounded: false,
                jump_active: true,
                ..default()
            })
            .id();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().open);
        let state = app.world().get::<crate::physics::WalkState>(body).unwrap();
        assert_eq!(state.vertical_velocity, 200.0);
        assert!(state.jump_active);
        assert!(!state.grounded);
    }

    #[test]
    fn menu_preserves_an_existing_pause_and_does_not_open_over_console() {
        let mut app = menu_app();
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(!app.world().resource::<SettingsMenuState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Backquote, ButtonState::Pressed);
        assert!(app.world().resource::<ConsoleState>().open);
        key(&mut app, KeyCode::Backquote, ButtonState::Released);
        // Escape belongs to the console while it is open: it closes the console, not opens the menu.
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(!app.world().resource::<SettingsMenuState>().open);
        assert!(!app.world().resource::<ConsoleState>().open);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        assert!(!app.world().resource::<SettingsMenuState>().open);
    }

    #[test]
    fn console_toggle_is_ignored_while_menu_owns_input() {
        let mut app = menu_app();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Backquote, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().open);
        assert!(!app.world().resource::<ConsoleState>().open);
    }

    #[test]
    fn button_press_changes_setting_once_and_keyboard_can_activate_a_focused_button() {
        let mut app = menu_app();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        let button = app
            .world_mut()
            .query::<(Entity, &MenuAction)>()
            .iter(app.world())
            .find_map(|(entity, action)| {
                matches!(action, MenuAction::Adjust(0, 1)).then_some(entity)
            })
            .unwrap();
        app.world_mut()
            .entity_mut(button)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RenderingSettings>().point_lights);
        app.update();
        assert!(app.world().resource::<RenderingSettings>().point_lights);
        app.world_mut().entity_mut(button).insert(Interaction::None);
        app.update();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(button, FocusCause::Navigated);
        key(&mut app, KeyCode::Space, ButtonState::Pressed);
        assert!(!app.world().resource::<RenderingSettings>().point_lights);
    }

    #[test]
    fn arrow_keys_adjust_the_focused_setting_and_step_between_tabs() {
        let mut app = menu_app();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        let find = |app: &mut App, wanted: fn(&MenuAction) -> bool| {
            app.world_mut()
                .query::<(Entity, &MenuAction)>()
                .iter(app.world())
                .find_map(|(entity, action)| wanted(action).then_some(entity))
                .unwrap()
        };
        let adjust = find(&mut app, |action| {
            matches!(action, MenuAction::Adjust(0, 1))
        });
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(adjust, FocusCause::Navigated);
        key(&mut app, KeyCode::ArrowRight, ButtonState::Pressed);
        assert!(app.world().resource::<RenderingSettings>().point_lights);
        key(&mut app, KeyCode::ArrowRight, ButtonState::Released);
        key(&mut app, KeyCode::ArrowLeft, ButtonState::Pressed);
        assert!(!app.world().resource::<RenderingSettings>().point_lights);
        key(&mut app, KeyCode::ArrowLeft, ButtonState::Released);

        let tab = find(&mut app, |action| {
            matches!(action, MenuAction::Page(SettingsPage::Graphics))
        });
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(tab, FocusCause::Navigated);
        key(&mut app, KeyCode::ArrowRight, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().page == SettingsPage::Streaming);
        let streaming = find(&mut app, |action| {
            matches!(action, MenuAction::Page(SettingsPage::Streaming))
        });
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(streaming));
    }
}
