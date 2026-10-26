//! One connection: toolbar, object browser on the left, query tabs on the right, status bar at
//! the bottom. Shown inside a connection tab of the main window.

use std::cell::Cell;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel},
    status_bar::StatusBar,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use ovs_core::sql::Dialect;
use ovs_core::{ConnectionConfig, ObjectKind};
use ovs_store::SavedConnectionTab;

use crate::browser::{BrowserEvent, ObjectBrowser};
use crate::query::{DEFAULT_ROW_LIMIT, QueryEvent, QueryTab, RunMode, TabSpec, format_duration};
use crate::runtime::{self, SharedSession};
use crate::tab_fit::{TAB_BAR_RESERVED, TabFit};
use crate::{
    CloseQueryTab, ExecuteAll, ExecuteCurrent, NewConnection, NewQueryTab, RefreshBrowser,
    StopQuery, WORKSPACE_CONTEXT,
};

/// Payload of a query tab being dragged to a new position.
#[derive(Clone)]
struct DraggedQueryTab {
    workspace: EntityId,
    ix: usize,
    title: SharedString,
}

/// The floating label that follows the pointer while a tab is dragged.
impl Render for DraggedQueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_sm()
            .child(self.title.clone())
    }
}

/// Moves `items[from]` to index `to` and returns where the item that was at `active` ended up.
fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize, active: usize) -> usize {
    let item = items.remove(from);
    items.insert(to, item);
    if active == from {
        to
    } else if from < active && active <= to {
        active - 1
    } else if to <= active && active < from {
        active + 1
    } else {
        active
    }
}

pub enum WorkspaceEvent {
    /// Something the saved session records changed: tabs, titles, SQL, databases.
    Changed,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

pub struct Workspace {
    config: ConnectionConfig,
    password: Option<String>,
    server_version: String,
    browser: Entity<ObjectBrowser>,
    tabs: Vec<Entity<QueryTab>>,
    active: usize,
    next_tab_number: usize,
    tab_fit: TabFit,
    subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        config: ConnectionConfig,
        password: Option<String>,
        session: SharedSession,
        restore: Option<SavedConnectionTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let allowed = config
            .database_filter()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let browser = cx.new(|cx| ObjectBrowser::new(session.clone(), allowed, window, cx));
        let sub = cx.subscribe_in(&browser, window, Self::on_browser_event);

        // The version is read once; it's needed for the status bar only.
        cx.spawn(async move |this, cx| {
            let version =
                runtime::run(async move { session.lock().await.server_version().to_owned() }).await;
            this.update(cx, |this, cx| {
                this.server_version = version;
                cx.notify();
            })
            .ok();
        })
        .detach();

        let mut ws = Self {
            config,
            password,
            server_version: String::new(),
            browser,
            tabs: Vec::new(),
            active: 0,
            next_tab_number: 1,
            tab_fit: TabFit::default(),
            subscriptions: vec![sub],
        };
        let saved = restore.unwrap_or_default();
        for tab in saved.tabs {
            ws.add_tab(Some(tab.title), tab.database, tab.sql, false, window, cx);
        }
        if ws.tabs.is_empty() {
            let first_db = ws.config.database_filter().first().map(|s| (*s).to_owned());
            ws.add_tab(None, first_db, String::new(), false, window, cx);
        }
        ws.active = saved.active_tab.min(ws.tabs.len() - 1);
        // Keep default names unique after a restore: "Query 3" follows "Query 2".
        ws.next_tab_number = ws
            .tabs
            .iter()
            .filter_map(|t| {
                t.read(cx)
                    .title()
                    .strip_prefix("Query ")?
                    .parse::<usize>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            + 1;
        ws
    }

    /// The connection tab as it should be restored next time the app starts.
    pub fn snapshot(&self, cx: &App) -> SavedConnectionTab {
        SavedConnectionTab {
            connection: Some(self.config.id),
            tabs: self.tabs.iter().map(|t| t.read(cx).snapshot(cx)).collect(),
            active_tab: self.active,
        }
    }

    fn changed(cx: &mut Context<Self>) {
        cx.emit(WorkspaceEvent::Changed);
        cx.notify();
    }

    /// Moves the tab at `from` to position `to`, keeping the same tab active.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        self.active = move_item(&mut self.tabs, from, to, self.active);
        Self::changed(cx);
    }

    fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active = ix.min(self.tabs.len().saturating_sub(1));
        Self::changed(cx);
    }

    fn open_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else {
            return;
        };
        let current = tab.read(cx).title();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let apply = {
            let input = input.clone();
            let tab = tab.clone();
            let this = cx.entity().downgrade();
            move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if !name.is_empty() {
                    tab.update(cx, |tab, cx| tab.rename(name.into(), cx));
                    this.update(cx, |_, cx| Self::changed(cx)).ok();
                }
                window.close_dialog(cx);
            }
        };
        let on_enter = apply.clone();
        self.subscriptions.push(cx.subscribe_in(
            &input,
            window,
            move |_, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    on_enter(window, cx);
                }
            },
        ));
        let first_build = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let apply = apply.clone();
            dialog
                .title("Rename query tab")
                .w(px(380.))
                .child(Input::new(&input))
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("rename-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("rename-ok")
                                .primary()
                                .label("Rename")
                                .on_click(move |_, window, cx| apply(window, cx)),
                        ),
                )
        });
    }

    fn add_tab(
        &mut self,
        title: Option<String>,
        database: Option<String>,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.unwrap_or_else(|| {
            let n = self.next_tab_number;
            self.next_tab_number += 1;
            format!("Query {n}")
        });
        let config = self.config.clone();
        let password = self.password.clone();
        let tab = cx.new(|cx| {
            let spec = TabSpec {
                title: title.into(),
                database,
                sql,
                run_on_connect: run,
            };
            QueryTab::new(spec, config, password, window, cx)
        });
        let sub = cx.subscribe_in(&tab, window, |this, _, event: &QueryEvent, _, cx| {
            match event {
                QueryEvent::SchemaChanged => this
                    .browser
                    .update(cx, super::browser::ObjectBrowser::refresh),
                QueryEvent::DatabaseChanged | QueryEvent::Edited => {
                    cx.emit(WorkspaceEvent::Changed);
                }
                QueryEvent::StateChanged => {}
            }
            cx.notify();
        });
        self.subscriptions.push(sub);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Self::changed(cx);
    }

    /// Label for the connection tab.
    pub fn title(&self) -> SharedString {
        self.config.name.clone().into()
    }

    fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active)
    }

    fn active_database(&self, cx: &App) -> Option<String> {
        self.active_tab()
            .and_then(|t| t.read(cx).database().map(str::to_owned))
    }

    fn on_browser_event(
        &mut self,
        _: &Entity<ObjectBrowser>,
        event: &BrowserEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let d = Dialect::MySql;
        match event {
            BrowserEvent::OpenData { db, name } => {
                let sql = format!(
                    "SELECT * FROM {} LIMIT {DEFAULT_ROW_LIMIT};",
                    d.qualified(db, name)
                );
                self.add_tab(Some(name.clone()), Some(db.clone()), sql, true, window, cx);
            }
            BrowserEvent::ShowDdl { db, name, kind } => {
                let target = if *kind == ObjectKind::Database {
                    d.quote_ident(name)
                } else {
                    d.qualified(db, name)
                };
                let sql = format!("SHOW CREATE {} {target};", kind.sql_keyword());
                self.add_tab(
                    Some(format!("{name} (DDL)")),
                    Some(db.clone()),
                    sql,
                    true,
                    window,
                    cx,
                );
            }
            BrowserEvent::InsertText(text) => {
                if let Some(tab) = self.active_tab() {
                    tab.update(cx, |tab, cx| tab.insert_text(text, window, cx));
                }
            }
            BrowserEvent::UseDatabase(db) => {
                if let Some(tab) = self.active_tab() {
                    tab.update(cx, |tab, cx| tab.set_database(db.clone(), cx));
                }
            }
        }
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            let db = self.active_database(cx);
            self.add_tab(None, db, String::new(), false, window, cx);
        }
        self.active = self.active.min(self.tabs.len() - 1);
        Self::changed(cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.active_tab().is_some_and(|t| t.read(cx).is_running());
        let database = self
            .active_database(cx)
            .unwrap_or_else(|| "(no database selected)".to_owned());
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("new-tab")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip("New query tab (⌘T / Ctrl+T)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let db = this.active_database(cx);
                        this.add_tab(None, db, String::new(), false, window, cx);
                    })),
            )
            .child(
                Button::new("execute")
                    .ghost()
                    .small()
                    .icon(IconName::Play)
                    .label("Execute")
                    .disabled(running)
                    .tooltip("Execute current statement or selection (F9 / ⌘↵)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.execute(RunMode::Current, window, cx);
                    })),
            )
            .child(
                Button::new("execute-all")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronsUpDown)
                    .label("Execute all")
                    .disabled(running)
                    .tooltip("Execute every statement (Ctrl+F9 / ⇧⌘↵)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.execute(RunMode::All, window, cx);
                    })),
            )
            .child(
                Button::new("stop")
                    .ghost()
                    .small()
                    .icon(IconName::Square)
                    .label("Stop")
                    .disabled(!running)
                    .tooltip("Stop the running query (⌘. / Ctrl+.)")
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
            )
            .child(
                Button::new("refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh object browser (F5)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.browser
                            .update(cx, super::browser::ObjectBrowser::refresh);
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(database),
            )
            .child(
                Button::new("connections")
                    .ghost()
                    .small()
                    .icon(IconName::Settings2)
                    .label("Connections")
                    .tooltip("Add or edit connections (⌘N / Ctrl+N)")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewConnection), cx)),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Per-tab click handlers (rather than TabBar::on_click) so a double click can rename.
        let tab_width = self
            .tab_fit
            .tab_width(self.tabs.len(), px(TAB_BAR_RESERVED));
        let bar = TabBar::new("query-tabs")
            .small()
            .menu(true)
            .max_width(tab_width)
            .selected_index(self.active)
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let title = tab.read(cx).title();
                let drag = DraggedQueryTab {
                    workspace: cx.entity_id(),
                    ix,
                    title: title.clone(),
                };
                Tab::new()
                    .label(title)
                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                    .drag_over::<DraggedQueryTab>(|style, _, _, cx| {
                        style.border_l_2().border_color(cx.theme().drag_border)
                    })
                    .on_drop(cx.listener(move |this, drag: &DraggedQueryTab, _, cx| {
                        // Tabs belong to one connection; ignore drops from another workspace.
                        if drag.workspace == cx.entity_id() {
                            this.move_tab(drag.ix, ix, cx);
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.open_rename(ix, window, cx);
                            } else {
                                this.select_tab(ix, cx);
                            }
                        }),
                    )
                    .suffix(
                        Button::new(("close-tab", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_tab(ix, window, cx);
                            })),
                    )
            }))
            .last_empty_space(
                h_flex().flex_1().h_full().items_center().pl_1().child(
                    Button::new("new-query-tab")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New query tab (⌘T / Ctrl+T)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let db = this.active_database(cx);
                            this.add_tab(None, db, String::new(), false, window, cx);
                        })),
                ),
            );
        div()
            .relative()
            .w_full()
            .child(bar)
            .child(self.tab_fit.measure(cx.entity_id()))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab().map(|t| t.read(cx));
        let left: SharedString = match tab {
            Some(t) if t.is_running() => "Executing…".into(),
            Some(t) => match (t.connection_error(), t.last_run()) {
                (Some(err), _) => format!("Not connected: {err}").into(),
                (None, Some(run)) if run.errors > 0 => {
                    format!("Completed with {} error(s)", run.errors).into()
                }
                (None, Some(run)) => format!("{} statement(s) completed", run.statements).into(),
                (None, None) => "Ready".into(),
            },
            None => "Ready".into(),
        };
        let mut bar = StatusBar::new().left(left);
        if let Some(run) = tab.and_then(QueryTab::last_run) {
            bar = bar
                .right(format!("Time: {}", format_duration(run.elapsed)))
                .right(format!("{} row(s)", run.rows));
        }
        if let Some(t) = tab {
            let (line, col) = t.cursor_line_col(cx);
            bar = bar.right(format!("Ln {line}, Col {col}"));
        }
        bar.right(format!(
            "{} · {}",
            self.config.endpoint(),
            self.server_version
        ))
    }

    fn execute(&mut self, mode: RunMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab().cloned() {
            tab.update(cx, |tab, cx| tab.execute(mode, window, cx));
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab().cloned() {
            tab.update(cx, super::query::QueryTab::stop);
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.active_tab() {
            Some(tab) => div().size_full().child(tab.clone()),
            None => div(),
        };
        v_flex()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(|this, _: &ExecuteCurrent, window, cx| {
                this.execute(RunMode::Current, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExecuteAll, window, cx| {
                this.execute(RunMode::All, window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopQuery, _, cx| this.stop(cx)))
            .on_action(cx.listener(|this, _: &RefreshBrowser, _, cx| {
                this.browser
                    .update(cx, super::browser::ObjectBrowser::refresh);
            }))
            .on_action(cx.listener(|this, _: &NewQueryTab, window, cx| {
                let db = this.active_database(cx);
                this.add_tab(None, db, String::new(), false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseQueryTab, window, cx| {
                this.close_tab(this.active, window, cx);
            }))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace-split")
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(160.)..px(600.))
                                .child(self.browser.clone()),
                        )
                        .child(
                            v_flex()
                                .size_full()
                                .child(self.render_tabs(cx))
                                .child(div().flex_1().min_h_0().child(content))
                                .into_any_element(),
                        ),
                ),
            )
            .child(self.render_status(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::move_item;

    #[test]
    fn moving_tabs_keeps_the_active_tab() {
        // Move the active tab itself.
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 0, 2, 0), 2);
        assert_eq!(v, ['b', 'c', 'a', 'd']);

        // Move a tab from before the active one to after it.
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 0, 3, 2), 1);
        assert_eq!(v[1], 'c');

        // Move a tab from after the active one to before it.
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 3, 0, 1), 2);
        assert_eq!(v[2], 'b');

        // Unrelated move.
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 2, 3, 0), 0);
    }
}
