//! App preferences: kept in a GPUI global, edited in the Settings dialog, saved with
//! `ovs-store`.

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Theme, ThemeMode, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    form::{Field, Form},
    h_flex,
    input::{Input, InputState},
    link::Link,
    radio::RadioGroup,
    scroll::ScrollbarMode,
    searchable_list::SearchableVec,
    select::{Select, SelectState},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Axis, Context, Entity, Global, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_store::{Preferences, Store, ThemePreference};

use crate::update::{self, CheckResult};

/// The current preferences, readable from anywhere with [`prefs`].
pub struct Prefs(pub Preferences);

impl Global for Prefs {}

pub fn prefs(cx: &App) -> &Preferences {
    &cx.global::<Prefs>().0
}

/// Loads saved preferences into the global and applies the theme. A damaged file falls back
/// to defaults rather than stopping the app from starting.
pub fn init(cx: &mut App) {
    let preferences = Store::default().load_preferences().unwrap_or_default();
    cx.set_global(Prefs(preferences));
    apply_theme(None, cx);
    // Scroll bars stay visible rather than fading out, so it's clear what scrolls.
    Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
}

/// Applies the theme preference; `System` follows the window's (or the OS's) appearance.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    match prefs(cx).theme {
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Switches to `theme` and remembers it. A failure to save is ignored: the theme still
/// changes for this session.
pub fn set_theme(theme: ThemePreference, window: &mut Window, cx: &mut App) {
    let mut preferences = prefs(cx).clone();
    preferences.theme = theme;
    let _ = Store::default().save_preferences(&preferences);
    cx.set_global(Prefs(preferences));
    apply_theme(Some(window), cx);
    cx.refresh_windows();
}

/// Replaces the preferences with `change` applied, saves them, and redraws every window. A
/// failure to save is ignored: the change still applies for this session.
fn update_prefs(change: impl FnOnce(&mut Preferences), cx: &mut App) {
    let mut preferences = prefs(cx).clone();
    change(&mut preferences);
    let _ = Store::default().save_preferences(&preferences);
    cx.set_global(Prefs(preferences));
    cx.refresh_windows();
}

/// The first entry of the font lists: the theme's own font.
const DEFAULT_FONT: &str = "(Default)";

/// Font families installed on this machine, sorted. Read once: listing fonts is slow.
fn installed_fonts(cx: &App) -> &'static [String] {
    static FONTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| cx.text_system().all_font_names())
}

/// `family` when it's set and installed, else `fallback`. GPUI stops the app on a family it
/// can't find, so a font that was uninstalled since it was chosen must never reach it.
fn installed_or(family: &str, fallback: SharedString, cx: &App) -> SharedString {
    if !family.is_empty() && installed_fonts(cx).iter().any(|f| f == family) {
        family.to_owned().into()
    } else {
        fallback
    }
}

/// The font for SQL: the editor and the records/text result views.
pub fn editor_font(cx: &App) -> SharedString {
    let fallback = cx.theme().mono_font_family.clone();
    installed_or(&prefs(cx).editor_font_family, fallback, cx)
}

/// The font for the results grid.
pub fn grid_font(cx: &App) -> SharedString {
    let fallback = cx.theme().font_family.clone();
    installed_or(&prefs(cx).grid_font_family, fallback, cx)
}

type FontSelect = Entity<SelectState<SearchableVec<SharedString>>>;

/// A searchable list of installed fonts, with "(Default)" first and `current` selected.
fn font_select(current: &str, window: &mut Window, cx: &mut App) -> FontSelect {
    let mut names: Vec<SharedString> = vec![DEFAULT_FONT.into()];
    names.extend(
        installed_fonts(cx)
            .iter()
            .map(|f| SharedString::from(f.clone())),
    );
    let selected = names
        .iter()
        .position(|n| n.as_ref() == current)
        .unwrap_or(0);
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(names),
            Some(IndexPath::new(selected)),
            window,
            cx,
        )
        .searchable(true)
    })
}

/// The chosen family, or empty for "(Default)".
fn chosen_font(select: &FontSelect, cx: &App) -> String {
    match select.read(cx).selected_value() {
        Some(name) if name.as_ref() != DEFAULT_FONT => name.to_string(),
        _ => String::new(),
    }
}

/// How a query tab shows a result set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResultView {
    /// The editable, sortable grid.
    #[default]
    Grid,
    /// One block per row, `column: value` lines (like MySQL's `\G`).
    Records,
    /// A boxed plain-text table.
    Text,
}

/// The view chosen in this run of the app. Not saved: every launch starts on the grid.
struct CurrentResultView(ResultView);

impl Global for CurrentResultView {}

/// How results are shown right now.
pub fn result_view(cx: &App) -> ResultView {
    cx.try_global::<CurrentResultView>()
        .map_or(ResultView::Grid, |v| v.0)
}

/// Shows results as a grid, records or text until the app quits.
pub fn set_result_view(view: ResultView, cx: &mut App) {
    cx.set_global(CurrentResultView(view));
    cx.refresh_windows();
}

/// Steps the results text size up or down by one point, or back to the default (`None`).
pub fn zoom_results(step: Option<f32>, cx: &mut App) {
    update_prefs(
        |p| {
            p.results_font_size = match step {
                Some(step) => (p.results_font_size + step).clamp(
                    *Preferences::FONT_SIZE_RANGE.start(),
                    *Preferences::FONT_SIZE_RANGE.end(),
                ),
                None => Preferences::default().results_font_size,
            };
        },
        cx,
    );
}

/// The name of a theme choice, as menus show it.
pub fn theme_label(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::System => "Match System",
        ThemePreference::Light => "Light",
        ThemePreference::Dark => "Dark",
    }
}

pub const THEMES: [ThemePreference; 3] = [
    ThemePreference::System,
    ThemePreference::Light,
    ThemePreference::Dark,
];

pub struct SettingsForm {
    row_limit: Entity<InputState>,
    font_size: Entity<InputState>,
    results_font_size: Entity<InputState>,
    editor_font: FontSelect,
    grid_font: FontSelect,
    theme: ThemePreference,
    stop_on_error: bool,
    check_for_updates: bool,
    /// The last Check now, if there's been one.
    update_check: Option<UpdateCheck>,
    update_task: Option<Task<()>>,
    error: Option<String>,
}

impl SettingsForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = prefs(cx).clone();
        Self {
            row_limit: cx
                .new(|cx| InputState::new(window, cx).default_value(current.row_limit.to_string())),
            font_size: cx.new(|cx| {
                InputState::new(window, cx).default_value(current.editor_font_size.to_string())
            }),
            results_font_size: cx.new(|cx| {
                InputState::new(window, cx).default_value(current.results_font_size.to_string())
            }),
            editor_font: font_select(&current.editor_font_family, window, cx),
            grid_font: font_select(&current.grid_font_family, window, cx),
            theme: current.theme,
            stop_on_error: current.stop_on_error,
            check_for_updates: current.check_for_updates,
            update_check: None,
            update_task: None,
            error: None,
        }
    }

    fn check_now(&mut self, cx: &mut Context<Self>) {
        self.update_check = Some(UpdateCheck::Running);
        let job = cx.background_executor().spawn(async { update::check() });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if let Ok(Some(found)) = &result {
                    update::set_available(Some(found.clone()), cx);
                }
                this.update_check = Some(UpdateCheck::Done(result));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The edited preferences, or why they're invalid.
    fn read(&self, cx: &App) -> Result<Preferences, String> {
        let row_text = self.row_limit.read(cx).value().trim().to_owned();
        let row_limit: usize = row_text
            .parse()
            .ok()
            .filter(|n| Preferences::ROW_LIMIT_RANGE.contains(n))
            .ok_or_else(|| {
                format!(
                    "Row limit must be a whole number from {} to {}.",
                    Preferences::ROW_LIMIT_RANGE.start(),
                    Preferences::ROW_LIMIT_RANGE.end()
                )
            })?;
        let font_text = self.font_size.read(cx).value().trim().to_owned();
        let editor_font_size: f32 = font_text
            .parse()
            .ok()
            .filter(|n| Preferences::FONT_SIZE_RANGE.contains(n))
            .ok_or_else(|| {
                format!(
                    "Font size must be from {} to {}.",
                    Preferences::FONT_SIZE_RANGE.start(),
                    Preferences::FONT_SIZE_RANGE.end()
                )
            })?;
        let results_text = self.results_font_size.read(cx).value().trim().to_owned();
        let results_font_size: f32 = results_text
            .parse()
            .ok()
            .filter(|n| Preferences::FONT_SIZE_RANGE.contains(n))
            .ok_or_else(|| {
                format!(
                    "Results font size must be from {} to {}.",
                    Preferences::FONT_SIZE_RANGE.start(),
                    Preferences::FONT_SIZE_RANGE.end()
                )
            })?;
        Ok(Preferences {
            row_limit,
            editor_font_size,
            results_font_size,
            editor_font_family: chosen_font(&self.editor_font, cx),
            grid_font_family: chosen_font(&self.grid_font, cx),
            theme: self.theme,
            stop_on_error: self.stop_on_error,
            check_for_updates: self.check_for_updates,
        })
    }
}

impl Render for SettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_ix = THEMES.iter().position(|t| *t == self.theme);
        v_flex()
            .gap_3()
            .child(
                Form::new()
                    .label_layout(Axis::Horizontal)
                    .child(
                        Field::new()
                            .label("Row limit")
                            .description("Added to SELECTs without a LIMIT; also the page size.")
                            .child(Input::new(&self.row_limit)),
                    )
                    .child(
                        Field::new()
                            .label("Editor font size")
                            .child(Input::new(&self.font_size)),
                    )
                    .child(
                        Field::new()
                            .label("Editor font")
                            .description(
                                "Editor and Records/Text views; monospace keeps them aligned.",
                            )
                            .child(Select::new(&self.editor_font).placeholder(DEFAULT_FONT)),
                    )
                    .child(
                        Field::new()
                            .label("Grid font")
                            .description("The results grid.")
                            .child(Select::new(&self.grid_font).placeholder(DEFAULT_FONT)),
                    )
                    .child(
                        Field::new()
                            .label("Results font size")
                            .description("Also ⌘/Ctrl + and − while a connection is open.")
                            .child(Input::new(&self.results_font_size)),
                    )
                    .child(
                        Field::new().label("Theme").child(
                            RadioGroup::horizontal("settings-theme")
                                .children(THEMES.map(theme_label))
                                .selected_index(theme_ix)
                                .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                    this.theme = THEMES[*ix];
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        Field::new().label("Shortcuts").child(
                            h_flex().child(
                                Button::new("settings-shortcuts")
                                    .label("Show keyboard shortcuts…")
                                    .on_click(|_, window, cx| crate::shortcuts::open(window, cx)),
                            ),
                        ),
                    )
                    .child(
                        Field::new().label_indent(true).child(
                            Checkbox::new("settings-stop-on-error")
                                .label("Execute all stops at the first error")
                                .checked(self.stop_on_error)
                                .on_change(cx.listener(|this, value: &bool, _, cx| {
                                    this.stop_on_error = *value;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        Field::new()
                            .label("Updates")
                            .description(format!(
                                "Version {}. A check sends only the app's name and version to \
                                 GitHub.",
                                update::current_version()
                            ))
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        Checkbox::new("settings-check-updates")
                                            .label("Check for updates at startup")
                                            .checked(self.check_for_updates)
                                            .on_change(cx.listener(|this, value: &bool, _, cx| {
                                                this.check_for_updates = *value;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Button::new("settings-check-now")
                                                    .label("Check now")
                                                    .loading(matches!(
                                                        self.update_check,
                                                        Some(UpdateCheck::Running)
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.check_now(cx);
                                                    })),
                                            )
                                            .children(
                                                self.update_check
                                                    .as_ref()
                                                    .map(|check| update_status(check, cx)),
                                            ),
                                    ),
                            ),
                    ),
            )
            .children(
                self.error
                    .clone()
                    .map(|err| div().text_sm().text_color(cx.theme().danger).child(err)),
            )
    }
}

/// Settings > Updates > Check now.
enum UpdateCheck {
    Running,
    Done(CheckResult),
}

/// What Check now found, as Settings shows it: the message, and a link to a newer release.
fn update_message(check: &UpdateCheck) -> (String, Option<String>) {
    match check {
        UpdateCheck::Running => ("Checking…".to_owned(), None),
        UpdateCheck::Done(Ok(None)) => (
            format!(
                "You have the latest version ({}).",
                update::current_version()
            ),
            None,
        ),
        UpdateCheck::Done(Ok(Some(found))) => (
            format!("Version {} is available.", found.version),
            Some(found.url.clone()),
        ),
        UpdateCheck::Done(Err(err)) => (err.clone(), None),
    }
}

fn update_status(check: &UpdateCheck, cx: &App) -> impl IntoElement {
    let failed = matches!(check, UpdateCheck::Done(Err(_)));
    let (message, url) = update_message(check);
    h_flex()
        .gap_2()
        .text_sm()
        .when(failed, |el| el.text_color(cx.theme().danger))
        .child(message)
        .children(url.map(|url| {
            Link::new("settings-update-link")
                .href(url)
                .child("Download")
        }))
}

/// Opens the Settings dialog in `window`.
pub fn open(window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| SettingsForm::new(window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let form = form.clone();
        dialog
            .title("Settings")
            .w(px(560.))
            .child(form.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("settings-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("settings-save")
                            .primary()
                            .label("Save")
                            .on_click(move |_, window, cx| {
                                let result = form.read(cx).read(cx);
                                match result {
                                    Ok(preferences) => {
                                        if let Err(err) =
                                            Store::default().save_preferences(&preferences)
                                        {
                                            form.update(cx, |f, cx| {
                                                f.error = Some(format!("Could not save: {err}"));
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        cx.set_global(Prefs(preferences));
                                        window.close_dialog(cx);
                                        apply_theme(Some(window), cx);
                                        cx.refresh_windows();
                                    }
                                    Err(err) => form.update(cx, |f, cx| {
                                        f.error = Some(err);
                                        cx.notify();
                                    }),
                                }
                            }),
                    ),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_now_reports_each_outcome() {
        assert_eq!(update_message(&UpdateCheck::Running).0, "Checking…");
        assert_eq!(
            update_message(&UpdateCheck::Done(Ok(None))),
            (
                format!(
                    "You have the latest version ({}).",
                    update::current_version()
                ),
                None
            )
        );
        let found = update::Update {
            version: "9.0.0".into(),
            url: "https://example.com/r".into(),
        };
        assert_eq!(
            update_message(&UpdateCheck::Done(Ok(Some(found)))),
            (
                "Version 9.0.0 is available.".to_owned(),
                Some("https://example.com/r".to_owned())
            )
        );
        assert_eq!(
            update_message(&UpdateCheck::Done(Err("Couldn't reach GitHub.".into()))).0,
            "Couldn't reach GitHub."
        );
    }
}
