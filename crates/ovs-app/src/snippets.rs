//! Saved queries ("snippets"): pick one (F2), fill in its `{{name}}` parameters, and it runs
//! in a new query tab. Designed to be driven from the keyboard: F2, type part of the name,
//! Enter, type the values, Enter.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use ovs_core::sql::snippets;
use ovs_store::{Snippet, Store};

/// Runs `sql` in a new query tab titled `title`.
pub type RunSql = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

/// Puts the cursor in `input` once the dialog that was just opened has taken focus (opening a
/// dialog focuses the dialog itself).
fn focus_later(input: Entity<InputState>, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
}

/// Opens the saved-queries picker. Enter runs the first match.
pub fn open_picker(run: RunSql, window: &mut Window, cx: &mut App) {
    let picker = cx.new(|cx| Picker::new(run, window, cx));
    let filter = picker.read(cx).filter.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let picker = picker.clone();
        dialog
            .title("Saved queries")
            .w(px(640.))
            .child(picker.clone())
            // Enter confirms the dialog: run the first match (which closes it).
            .on_ok(move |_, window, cx| {
                picker.update(cx, |picker, cx| picker.run_first(window, cx));
                false
            })
    });
    focus_later(filter, window, cx);
}

/// Asks for a name and saves `sql` as a snippet.
pub fn open_save(sql: &str, window: &mut Window, cx: &mut App) {
    let sql = sql.trim().to_owned();
    if sql.is_empty() {
        window.push_notification("There's no query to save: the editor is empty.", cx);
        return;
    }
    let form = cx.new(|cx| SaveForm::new(sql, window, cx));
    let name = form.read(cx).name.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let form = form.clone();
        let enter_form = form.clone();
        dialog
            .title("Save query as snippet")
            .w(px(560.))
            .child(form.clone())
            .on_ok(move |_, window, cx| {
                enter_form.update(cx, |form, cx| form.save(window, cx));
                false
            })
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("snippet-save-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("snippet-save-ok")
                            .primary()
                            .label("Save")
                            .on_click(move |_, window, cx| {
                                form.update(cx, |form, cx| form.save(window, cx));
                            }),
                    ),
            )
    });
    focus_later(name, window, cx);
}

/// Runs `snippet`, first asking for its parameters if it has any.
fn choose(snippet: Snippet, run: &RunSql, window: &mut Window, cx: &mut App) {
    let names = snippets::params(&snippet.sql);
    if names.is_empty() {
        run(snippet.name, snippet.sql, window, cx);
        return;
    }
    let title = format!("Run \u{201c}{}\u{201d}", snippet.name);
    let form = cx.new(|cx| ParamForm::new(snippet, names, run.clone(), window, cx));
    let first = form.read(cx).inputs.first().map(|(_, input)| input.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        let form = form.clone();
        let enter_form = form.clone();
        dialog
            .title(title.clone())
            .w(px(520.))
            .child(form.clone())
            // Enter in any field runs it; Tab moves between fields.
            .on_ok(move |_, window, cx| {
                enter_form.update(cx, |form, cx| form.submit(window, cx));
                false
            })
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("snippet-params-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("snippet-params-run")
                            .primary()
                            .label("Run")
                            .on_click(move |_, window, cx| {
                                form.update(cx, |form, cx| form.submit(window, cx));
                            }),
                    ),
            )
    });
    if let Some(first) = first {
        focus_later(first, window, cx);
    }
}

/// Snippets whose name or SQL contains `filter` (ignoring case), names matches first.
fn matching<'a>(all: &'a [Snippet], filter: &str) -> Vec<&'a Snippet> {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return all.iter().collect();
    }
    let (mut by_name, by_sql): (Vec<&Snippet>, Vec<&Snippet>) = all
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&filter) || s.sql.to_lowercase().contains(&filter)
        })
        .partition(|s| s.name.to_lowercase().contains(&filter));
    by_name.extend(by_sql);
    by_name
}

struct Picker {
    filter: Entity<InputState>,
    snippets: Vec<Snippet>,
    error: Option<String>,
    run: RunSql,
    _subscription: Subscription,
}

impl Picker {
    fn new(run: RunSql, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Type to filter, Enter runs the first match")
        });
        // The list follows the filter as it's typed; Enter is the dialog's OK.
        let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let (snippets, error) = match Store::default().load_snippets() {
            Ok(snippets) => (snippets, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        Self {
            filter,
            snippets,
            error,
            run,
            _subscription: subscription,
        }
    }

    fn run_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        let Some(first) = matching(&self.snippets, &filter)
            .first()
            .map(|s| (*s).clone())
        else {
            return;
        };
        Self::start(first, self.run.clone(), window, cx);
    }

    /// Closes the picker, then runs (or asks for parameters) a moment later, so the Enter
    /// that chose the snippet can't also confirm the parameter dialog.
    fn start(snippet: Snippet, run: RunSql, window: &mut Window, cx: &mut App) {
        window.close_dialog(cx);
        window
            .spawn(cx, async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                cx.update(|window, cx| choose(snippet, &run, window, cx))
                    .ok();
            })
            .detach();
    }

    fn delete(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let store = Store::default();
        match store.delete_snippet(name) {
            Ok(()) => self.snippets = store.load_snippets().unwrap_or_default(),
            Err(err) => window.push_notification(format!("Could not delete: {err}"), cx),
        }
        cx.notify();
    }
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let mono = theme.mono_font_family.clone();
        let filter = self.filter.read(cx).value().to_string();
        let shown: Vec<Snippet> = matching(&self.snippets, &filter)
            .into_iter()
            .cloned()
            .collect();
        let empty = if self.snippets.is_empty() {
            Some(
                "No saved queries yet. Write a query, then Query \u{2192} Save Query as \
                 Snippet\u{2026} (\u{2318}\u{21e7}K / Ctrl+Shift+K). Use {{name}} for values \
                 to ask for when it runs.",
            )
        } else if shown.is_empty() {
            Some("No saved query matches.")
        } else {
            None
        };
        v_flex()
            .gap_2()
            .child(Input::new(&self.filter))
            .when_some(self.error.clone(), |el, err| {
                el.child(div().text_color(danger).child(err))
            })
            .when_some(empty, |el, text| {
                el.child(div().text_sm().text_color(muted).child(text))
            })
            .child(
                v_flex()
                    .id("snippet-list")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .children(shown.into_iter().enumerate().map(|(ix, snippet)| {
                        let params = snippets::params(&snippet.sql);
                        let preview: String = snippet
                            .sql
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(110)
                            .collect();
                        let (run_snippet, delete_name) = (snippet.clone(), snippet.name.clone());
                        let run = self.run.clone();
                        h_flex()
                            .id(("snippet", ix))
                            .gap_2()
                            .py_1()
                            .border_b_1()
                            .border_color(border)
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Button::new(("snippet-run", ix))
                                            .ghost()
                                            .small()
                                            .label(snippet.name.clone())
                                            .on_click(move |_, window, cx| {
                                                Self::start(
                                                    run_snippet.clone(),
                                                    run.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .text_xs()
                                            .text_color(muted)
                                            .font_family(mono.clone())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(if params.is_empty() {
                                                preview
                                            } else {
                                                format!("({}) {preview}", params.join(", "))
                                            }),
                                    ),
                            )
                            .child(
                                Button::new(("snippet-delete", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Delete")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete(&delete_name, window, cx);
                                    })),
                            )
                    })),
            )
    }
}

struct ParamForm {
    snippet: Snippet,
    inputs: Vec<(String, Entity<InputState>)>,
    run: RunSql,
}

impl ParamForm {
    fn new(
        snippet: Snippet,
        names: Vec<String>,
        run: RunSql,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inputs = names
            .into_iter()
            .map(|name| {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(name.clone()));
                (name, input)
            })
            .collect();
        Self {
            snippet,
            inputs,
            run,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let values: HashMap<String, String> = self
            .inputs
            .iter()
            .map(|(name, input)| (name.clone(), input.read(cx).value().trim().to_owned()))
            .collect();
        let sql = snippets::fill(&self.snippet.sql, &values);
        let (title, run) = (self.snippet.name.clone(), self.run.clone());
        window.close_dialog(cx);
        window.defer(cx, move |window, cx| run(title, sql, window, cx));
    }
}

impl Render for ParamForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .children(self.inputs.iter().map(|(name, input)| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(140.)).flex_none().child(name.clone()))
                    .child(div().flex_1().child(Input::new(input)))
            }))
            .child(div().text_xs().text_color(muted).child(
                "Values are put in exactly as typed; the query decides whether they're quoted.",
            ))
    }
}

struct SaveForm {
    sql: String,
    name: Entity<InputState>,
    error: Option<String>,
}

impl SaveForm {
    fn new(sql: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name, e.g. Order lines"));
        Self {
            sql,
            name,
            error: None,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.error = Some("Give the snippet a name.".into());
            cx.notify();
            return;
        }
        let result = Store::default().upsert_snippet(Snippet {
            name: name.clone(),
            sql: self.sql.clone(),
        });
        match result {
            Ok(()) => {
                window.close_dialog(cx);
                window.push_notification(format!("Saved \u{201c}{name}\u{201d} (F2 to run)"), cx);
            }
            Err(err) => {
                self.error = Some(format!("Could not save: {err}"));
                cx.notify();
            }
        }
    }
}

impl Render for SaveForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let params = snippets::params(&self.sql);
        let preview: String = self.sql.chars().take(600).collect();
        v_flex()
            .gap_2()
            .child(Input::new(&self.name))
            .when_some(self.error.clone(), |el, err| {
                el.child(div().text_color(theme.danger).child(err))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if params.is_empty() {
                        "No {{name}} parameters: it will run as is. Saving with an existing \
                         name replaces that snippet."
                            .to_owned()
                    } else {
                        format!(
                            "Asks for: {}. Saving with an existing name replaces that snippet.",
                            params.join(", ")
                        )
                    }),
            )
            .child(
                div()
                    .p_2()
                    .max_h(px(220.))
                    .overflow_hidden()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(preview),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, sql: &str) -> Snippet {
        Snippet {
            name: name.into(),
            sql: sql.into(),
        }
    }

    #[test]
    fn name_matches_come_before_sql_matches() {
        let all = [
            s("Affected rows", "SELECT * FROM order_items"),
            s("Order lines", "SELECT 1"),
            s("Tenders", "SELECT 2"),
        ];
        let names: Vec<&str> = matching(&all, "ORDER")
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["Order lines", "Affected rows"]);
        assert_eq!(matching(&all, "  ").len(), 3);
        assert_eq!(matching(&all, "nothing").len(), 0);
    }
}
