//! App preferences: kept in a GPUI global, edited in the Settings dialog, saved with
//! `ovs-store`.

use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    form::{Field, Form},
    h_flex,
    input::{Input, InputState},
    link::Link,
    radio::RadioGroup,
    scroll::ScrollbarMode,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Axis, Context, Entity, Global, IntoElement, ParentElement as _, Render,
    Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
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
        Ok(Preferences {
            row_limit,
            editor_font_size,
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
