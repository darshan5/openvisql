//! A query tab: SQL editor on top, results below. Each tab owns its own server session so a
//! long-running query in one tab never blocks another tab or the object browser.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent},
    input::{Input, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    resizable::{resizable_panel, v_resizable},
    tab::{Tab, TabBar},
    table::{ColumnSort, DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::editing::{EditTarget, single_source_table, source_tables};
use ovs_core::export::{ExportOptions, export};
use ovs_core::sql::paging::{PageSpec, SortSpec};
use ovs_core::sql::{self, Statement, StatementKind};
use ovs_core::{
    CancelHandle, ConnectionConfig, Error, QueryOutcome, ResultSet, StatementReport, Value,
};
use ovs_store::{HistoryEntry, SavedQueryTab, Store};

use crate::completion::{CompletionSource, SqlCompletions};
use crate::export_dialog::ExportForm;
use crate::grid::{Editability, OpenValue, ResultGrid, ServerSort};
use crate::runtime::{self, SharedSession};

/// `LIMIT` appended to plain SELECTs that don't have one.
pub const DEFAULT_ROW_LIMIT: usize = 50;
/// Hard cap per result set for statements we can't add a LIMIT to (SHOW, CALL, ...).
const MAX_FETCH_ROWS: usize = 50_000;
/// Largest part of a binary value shown as hex in the value viewer.
const VALUE_HEX_PREVIEW: usize = 64 * 1024;
/// Longest statement text shown in the Messages tab.
const MESSAGE_SQL_CHARS: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// The selection, or else the statement under the cursor.
    Current,
    All,
}

#[allow(clippy::enum_variant_names)]
pub enum QueryEvent {
    /// A statement may have created, altered or dropped objects.
    SchemaChanged,
    /// The tab's default database changed (e.g. after `USE`).
    DatabaseChanged,
    /// Execution started or finished; status-bar data changed.
    StateChanged,
    /// The SQL text changed.
    Edited,
}

impl EventEmitter<QueryEvent> for QueryTab {}

enum SessionState {
    Connecting,
    Ready {
        session: SharedSession,
        cancel: Arc<dyn CancelHandle>,
    },
    Failed(String),
}

struct ResultTab {
    label: SharedString,
    table: Entity<TableState<ResultGrid>>,
    rows: usize,
    elapsed: Duration,
    /// Rows the statement would return without the automatic `LIMIT`, when it was cut short.
    total: Option<u64>,
    auto_limited: bool,
    hit_limit: bool,
    /// Reading stopped at `MAX_FETCH_ROWS`.
    truncated: bool,
    /// Re-run after saving edits; `None` for results that can't be edited.
    source_sql: Option<String>,
    /// Tables the result's columns come from; more than one for a join.
    edit_tables: Vec<(String, String)>,
    /// The table whose columns are being edited, when there is one.
    edit_table: Option<String>,
    paging: Option<Paging>,
    _subscriptions: Vec<Subscription>,
}

/// Paging, filtering and server-side sorting of a SELECT result.
struct Paging {
    spec: PageSpec,
    filter_input: Entity<InputState>,
    /// The filter `ResultTab::total` was counted with, so totals are only re-counted when the
    /// filter changes.
    counted_filter: Option<String>,
    loading: bool,
    error: Option<String>,
}

impl Paging {
    /// True when more rows exist than are loaded, so sorting has to happen on the server.
    fn is_partial(&self, rows: usize, total: Option<u64>, hit_limit: bool) -> bool {
        self.spec.offset > 0
            || !self.spec.filter.trim().is_empty()
            || total.map_or(hit_limit, |t| t > rows as u64)
    }
}

/// How the automatic `LIMIT` affected one statement.
#[derive(Clone, Default)]
struct Limits {
    auto_limited: bool,
    /// The result has exactly as many rows as its `LIMIT`, so more may exist.
    hit_limit: bool,
    total: Option<u64>,
    /// The SQL that produced the result, when it's a plain SELECT whose rows may be edited
    /// and re-fetched after saving.
    source_sql: Option<String>,
    /// How to fetch other pages of a SELECT's result.
    page: Option<PageSpec>,
}

/// Summary of the last run, shown in the status bar.
#[derive(Clone, Default)]
pub struct RunSummary {
    pub statements: usize,
    pub errors: usize,
    pub elapsed: Duration,
    pub rows: usize,
}

pub struct QueryTab {
    title: SharedString,
    config: ConnectionConfig,
    editor: Entity<EditorState>,
    session: SessionState,
    database: Option<String>,
    /// Shared with the completion provider.
    database_cell: Rc<RefCell<Option<String>>>,
    /// The `.sql` file this tab is linked to.
    path: Option<PathBuf>,
    /// The text changed since it was opened or saved.
    modified: bool,
    results: Vec<ResultTab>,
    reports: Vec<StatementReport>,
    /// Index into `results`; `results.len()` selects Messages.
    active_result: usize,
    running: Option<Task<()>>,
    last_run: Option<RunSummary>,
    store: Store,
}

/// What a new query tab starts with.
pub struct TabSpec {
    /// Schema cache and metadata session shared by the connection's tabs, for autocomplete.
    pub completion: CompletionSource,
    pub title: SharedString,
    pub database: Option<String>,
    pub sql: String,
    /// Execute `sql` as soon as the tab's session is ready.
    pub run_on_connect: bool,
}

impl QueryTab {
    pub fn new(
        spec: TabSpec,
        config: ConnectionConfig,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let database_cell = Rc::new(RefCell::new(spec.database.clone()));
        let provider = Rc::new(SqlCompletions {
            source: spec.completion.clone(),
            database: database_cell.clone(),
        });
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(spec.sql);
            state.lsp_mut().completion_provider = Some(provider);
            state
        });
        // Re-render (and let the workspace refresh its Ln/Col) whenever the editor changes.
        cx.observe(&editor, |_, _, cx| {
            cx.emit(QueryEvent::StateChanged);
            cx.notify();
        })
        .detach();
        // Text edits are what the saved session cares about; cursor moves are not.
        cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.modified = true;
                cx.emit(QueryEvent::Edited);
            }
        })
        .detach();

        let tab = Self {
            title: spec.title,
            config: config.clone(),
            editor,
            session: SessionState::Connecting,
            database: spec.database.clone(),
            database_cell,
            path: None,
            modified: false,
            results: Vec::new(),
            reports: Vec::new(),
            active_result: 0,
            running: None,
            last_run: None,
            store: Store::default(),
        };
        Self::connect(
            config,
            password,
            spec.database,
            spec.run_on_connect,
            window,
            cx,
        );
        tab
    }

    fn connect(
        config: ConnectionConfig,
        password: Option<String>,
        database: Option<String>,
        run_on_connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let session = runtime::connect(config, password).await?;
                let db = database.clone();
                let session = runtime::run(async move {
                    if let Some(db) = db {
                        session.lock().await.use_database(&db).await?;
                    }
                    Ok::<_, Error>(session)
                })
                .await?;
                let cancel = runtime::run({
                    let session = session.clone();
                    async move { session.lock().await.cancel_handle() }
                })
                .await;
                Ok::<_, Error>((session, cancel))
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.session = match result {
                    Ok((session, cancel)) => SessionState::Ready { session, cancel },
                    Err(err) => SessionState::Failed(err.to_string()),
                };
                if run_on_connect {
                    this.execute(RunMode::All, window, cx);
                }
                cx.emit(QueryEvent::StateChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Records the tab's default database, also for autocomplete.
    fn set_database_name(&mut self, database: Option<String>) {
        self.database_cell.replace(database.clone());
        self.database = database;
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn rename(&mut self, title: SharedString, cx: &mut Context<Self>) {
        self.title = title;
        cx.notify();
    }

    /// The tab title, with "•" while a file-backed tab has unsaved changes.
    pub fn display_title(&self) -> SharedString {
        if self.path.is_some() && self.modified {
            format!("{} •", self.title).into()
        } else {
            self.title.clone()
        }
    }

    /// Links the tab to a `.sql` file; its contents count as saved.
    pub fn set_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.path = path;
        self.modified = false;
        cx.notify();
    }

    /// Writes the editor's text to the tab's file, asking for a file name first when the tab
    /// has none or `save_as` is set.
    pub fn save_file(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let target = self.path.clone().filter(|_| !save_as);
        let chosen = if target.is_some() {
            None
        } else {
            let dir = self
                .path
                .as_ref()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .or_else(dirs::document_dir)
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| PathBuf::from("."));
            let name = match self.path.as_ref().and_then(|p| p.file_name()) {
                Some(name) => name.to_string_lossy().into_owned(),
                None => format!("{}.sql", self.title),
            };
            Some(cx.prompt_for_new_path(&dir, Some(&name)))
        };
        cx.spawn_in(window, async move |this, cx| {
            let path = match (target, chosen) {
                (Some(path), _) => path,
                (None, Some(chosen)) => match chosen.await {
                    Ok(Ok(Some(path))) => path,
                    _ => return,
                },
                (None, None) => return,
            };
            let written = std::fs::write(&path, text);
            this.update_in(cx, |this, window, cx| match written {
                Ok(()) => {
                    if let Some(name) = path.file_name() {
                        this.title = name.to_string_lossy().into_owned().into();
                    }
                    this.set_path(Some(path), cx);
                    cx.emit(QueryEvent::Edited);
                }
                Err(err) => {
                    window
                        .push_notification(format!("Could not save {}: {err}", path.display()), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Comments or uncomments the lines touched by the selection (or the cursor's line).
    pub fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let text = editor.value().to_string();
            let lines = sql::text::line_range(&text, editor.selected_range());
            let replaced = sql::text::toggle_line_comments(&text[lines.clone()]);
            editor.set_selected_range(lines.clone(), cx);
            editor.replace(replaced.clone(), window, cx);
            editor.set_selected_range(lines.start..lines.start + replaced.len(), cx);
        });
    }

    /// Upper- or lower-cases the selected text, keeping it selected.
    pub fn change_case(&mut self, upper: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let range = editor.selected_range();
            if range.is_empty() {
                return;
            }
            let text = editor.value().to_string();
            let changed = if upper {
                text[range.clone()].to_uppercase()
            } else {
                text[range.clone()].to_lowercase()
            };
            editor.replace(changed.clone(), window, cx);
            editor.set_selected_range(range.start..range.start + changed.len(), cx);
        });
    }

    /// The tab as it should be restored next time the app starts.
    pub fn snapshot(&self, cx: &gpui_kit::App) -> SavedQueryTab {
        SavedQueryTab {
            title: self.title.to_string(),
            path: self.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            database: self.database.clone(),
            sql: self.editor.read(cx).value().to_string(),
        }
    }

    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn last_run(&self) -> Option<&RunSummary> {
        self.last_run.as_ref()
    }

    /// 1-based line and column of the cursor.
    pub fn cursor_line_col(&self, cx: &gpui_kit::App) -> (u32, u32) {
        let pos = self.editor.read(cx).cursor_position();
        (pos.line + 1, pos.character + 1)
    }

    pub fn connection_error(&self) -> Option<&str> {
        match &self.session {
            SessionState::Failed(err) => Some(err),
            _ => None,
        }
    }

    pub fn set_database(&mut self, db: String, cx: &mut Context<Self>) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        if self.running.is_some() {
            return;
        }
        let session = session.clone();
        let target = db.clone();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::run(async move { session.lock().await.use_database(&target).await }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.set_database_name(Some(db)),
                    Err(err) => this.push_note(format!("USE {db}: {err}")),
                }
                cx.emit(QueryEvent::DatabaseChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Statements to run for `mode`, taken from the editor.
    fn statements(&self, mode: RunMode, cx: &gpui_kit::App) -> Vec<Statement> {
        let editor = self.editor.read(cx);
        let text = editor.value();
        let selection = editor.selected_range();
        if mode == RunMode::Current && !selection.is_empty() {
            return sql::split_statements(&text[selection]);
        }
        match mode {
            RunMode::All => sql::split_statements(&text),
            RunMode::Current => sql::statement_at(&text, editor.cursor())
                .into_iter()
                .collect(),
        }
    }

    /// Rows with unsaved grid edits across all results of this tab.
    pub fn unsaved_edit_rows(&self, cx: &gpui_kit::App) -> usize {
        self.results
            .iter()
            .map(|r| r.table.read(cx).delegate().pending().row_count())
            .sum()
    }

    pub fn execute(&mut self, mode: RunMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        // Running replaces the results, which would silently drop unsaved edits.
        let unsaved = self.unsaved_edit_rows(cx);
        if unsaved > 0 {
            let this = cx.entity().downgrade();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let this = this.clone();
                alert
                    .title("Discard unsaved changes?")
                    .description(format!(
                        "Running a query replaces the results, and {unsaved} edited row{} \
                         haven't been saved.",
                        if unsaved == 1 { "" } else { "s" }
                    ))
                    .confirm()
                    .ok_text("Discard and run")
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.discard_all_edits(cx);
                            this.execute(mode, window, cx);
                        })
                        .ok();
                        true
                    })
            });
            return;
        }
        match &self.session {
            SessionState::Ready { .. } => {}
            SessionState::Connecting => return,
            SessionState::Failed(err) => {
                let err = err.clone();
                window.push_notification(format!("Not connected: {err}"), cx);
                return;
            }
        }
        let statements = self.statements(mode, cx);
        if statements.is_empty() {
            return;
        }

        let unsafe_count = statements
            .iter()
            .filter(|s| sql::missing_where(&s.sql))
            .count();
        if self.config.safe_updates && unsafe_count > 0 {
            let this = cx.entity().downgrade();
            let description = if unsafe_count == 1 {
                "An UPDATE or DELETE statement has no WHERE clause and will change every row in its table.".to_owned()
            } else {
                format!(
                    "{unsafe_count} UPDATE or DELETE statements have no WHERE clause and will change every row in their tables."
                )
            };
            window.open_alert_dialog(cx, move |alert, _, _| {
                let this = this.clone();
                let statements = statements.clone();
                alert
                    .title("Run without WHERE?")
                    .description(description.clone())
                    .confirm()
                    .ok_text("Run anyway")
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let statements = statements.clone();
                        this.update(cx, |this, cx| this.run(statements, window, cx))
                            .ok();
                        true
                    })
            });
            return;
        }
        self.run(statements, window, cx);
    }

    fn run(&mut self, statements: Vec<Statement>, window: &mut Window, cx: &mut Context<Self>) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        let session = session.clone();
        self.results.clear();
        self.reports.clear();
        self.active_result = 0;
        self.last_run = None;
        let started = Instant::now();

        self.running = Some(cx.spawn_in(window, async move |this, cx| {
            for stmt in statements {
                let limited = sql::apply_row_limit(&stmt.sql, DEFAULT_ROW_LIMIT);
                let auto_limited = limited.is_some();
                let to_run = limited.unwrap_or_else(|| stmt.sql.clone());
                let t0 = Instant::now();
                let result = runtime::run({
                    let session = session.clone();
                    async move {
                        session
                            .lock()
                            .await
                            .execute(&to_run, Some(MAX_FETCH_ROWS))
                            .await
                    }
                })
                .await;
                let elapsed = t0.elapsed();
                // When a LIMIT (ours or the user's) cut the result short, count what the query
                // would return without it. A failed count (e.g. duplicate column names in a
                // join) just leaves the total unknown.
                let is_select = sql::classify(&stmt.sql) == StatementKind::Select;
                let limit = if auto_limited {
                    Some(DEFAULT_ROW_LIMIT as u64)
                } else if is_select {
                    sql::limit_clause(&stmt.sql).map(|l| l.row_count)
                } else {
                    None
                };
                let hit_limit = limit.is_some_and(|n| {
                    matches!(
                        &result,
                        Ok(outcomes) if matches!(
                            outcomes.first(),
                            Some(QueryOutcome::Rows(rs)) if rs.rows.len() as u64 == n
                        )
                    )
                });
                let total = if hit_limit {
                    let count_sql = sql::count_query(&stmt.sql);
                    let session = session.clone();
                    runtime::run(async move {
                        let out = session.lock().await.execute(&count_sql, None).await.ok()?;
                        match out.first()? {
                            QueryOutcome::Rows(rs) => match rs.rows.first()?.first()? {
                                Value::Int(n) => u64::try_from(*n).ok(),
                                Value::UInt(n) => Some(*n),
                                other => other.to_string().parse().ok(),
                            },
                            QueryOutcome::Affected { .. } => None,
                        }
                    })
                    .await
                } else {
                    None
                };
                let source_sql = is_select.then(|| {
                    sql::apply_row_limit(&stmt.sql, DEFAULT_ROW_LIMIT)
                        .unwrap_or_else(|| stmt.sql.clone())
                });
                let page = is_select.then(|| {
                    let own = sql::limit_clause(&stmt.sql);
                    PageSpec {
                        base: sql::strip_limit(&stmt.sql),
                        filter: String::new(),
                        sort: None,
                        offset: own.as_ref().map_or(0, |l| l.offset),
                        page_size: own.map_or(DEFAULT_ROW_LIMIT as u64, |l| l.row_count).max(1),
                    }
                });
                let limits = Limits {
                    auto_limited,
                    hit_limit,
                    total,
                    source_sql,
                    page,
                };
                let cancelled = matches!(result, Err(Error::Cancelled));
                let report = match result {
                    Ok(outcomes) => StatementReport {
                        sql: stmt.sql,
                        outcomes,
                        elapsed,
                        error: None,
                    },
                    Err(err) => StatementReport {
                        sql: stmt.sql,
                        outcomes: Vec::new(),
                        elapsed,
                        error: Some(err.to_string()),
                    },
                };
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        this.push_report(report, &limits, window, cx);
                    })
                    .is_ok();
                if cancelled || !alive {
                    break;
                }
            }
            this.update(cx, |this, cx| this.finish(started.elapsed(), cx))
                .ok();
        }));
        cx.emit(QueryEvent::StateChanged);
        cx.notify();
    }

    fn push_report(
        &mut self,
        report: StatementReport,
        limits: &Limits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = sql::classify(&report.sql);
        if report.error.is_none() {
            match &kind {
                StatementKind::Use(db) => {
                    self.set_database_name(Some(db.clone()));
                    cx.emit(QueryEvent::DatabaseChanged);
                }
                k if k.changes_schema() => cx.emit(QueryEvent::SchemaChanged),
                _ => {}
            }
        }
        self.record_history(&report);
        for (ix, rs) in report.result_sets().enumerate() {
            // The automatic LIMIT only ever applies to a single-result SELECT.
            let limits = if ix == 0 {
                limits.clone()
            } else {
                Limits::default()
            };
            self.add_result(rs.clone(), report.elapsed, &limits, window, cx);
        }
        self.reports.push(report);
        cx.notify();
    }

    fn add_result(
        &mut self,
        rs: ResultSet,
        elapsed: Duration,
        limits: &Limits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.results.len() + 1;
        let rows = rs.rows.len();
        let truncated = rs.truncated;
        let source = limits.source_sql.clone().zip(single_source_table(&rs));
        // A join can still be edited one table at a time; the user picks which.
        let edit_tables = if limits.source_sql.is_some() {
            source_tables(&rs)
        } else {
            Vec::new()
        };
        let editability = if source.is_some() {
            Editability::Checking
        } else if limits.source_sql.is_none() {
            Editability::ReadOnly("only SELECT results can be edited".into())
        } else if edit_tables.len() > 1 {
            Editability::ReadOnly("choose a table to edit".into())
        } else {
            Editability::ReadOnly("the result doesn't come from a table".into())
        };
        let table = cx.new(|cx| {
            TableState::new(ResultGrid::new(Arc::new(rs), editability), window, cx)
                .col_resizable(true)
                .col_movable(false)
                .sortable(true)
                .cell_selectable(true)
        });
        let events = cx.subscribe_in(
            &table,
            window,
            |_, table, event: &TableEvent, window, cx| {
                table.update(cx, |table, cx| match *event {
                    TableEvent::DoubleClickedCell(row, col) => {
                        table.delegate_mut().begin_edit(row, col, window, cx);
                    }
                    TableEvent::SelectCell(row, col) | TableEvent::RightClickedCell(row, col) => {
                        table.delegate_mut().focus_cell(row, col);
                    }
                    _ => {}
                });
            },
        );
        // Pending-change counts in the footer follow the grid.
        let observe = cx.observe(&table, |_, _, cx| cx.notify());
        let ix = self.results.len();
        let mut subscriptions = vec![events, observe];
        let paging = limits.page.clone().map(|spec| {
            let filter_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Filter rows, e.g. status = 'paid' AND id > 100")
            });
            subscriptions.push(cx.subscribe(
                &filter_input,
                move |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.apply_filter(ix, cx);
                    }
                },
            ));
            Paging {
                spec,
                filter_input,
                counted_filter: Some(String::new()),
                loading: false,
                error: None,
            }
        });
        if let Some(p) = &paging
            && p.is_partial(rows, limits.total, limits.hit_limit)
        {
            let callback = Self::server_sort_callback(ix, cx);
            table.update(cx, |t, _| t.delegate_mut().set_server_sort(Some(callback)));
        }
        let open_value = Self::open_value_callback(&table, cx);
        table.update(cx, |t, _| {
            t.delegate_mut().set_on_open_value(Some(open_value));
        });
        if let Some((_, (schema, name))) = &source {
            self.detect_editability(table.clone(), schema.clone(), name.clone(), cx);
        }
        self.results.push(ResultTab {
            label: if n == 1 {
                "Results".into()
            } else {
                format!("Results {n}").into()
            },
            table,
            rows,
            elapsed,
            total: limits.total,
            auto_limited: limits.auto_limited,
            hit_limit: limits.hit_limit,
            truncated,
            edit_table: source.as_ref().map(|(_, (_, t))| t.clone()),
            source_sql: source
                .map(|(sql, _)| sql)
                .or_else(|| limits.source_sql.clone().filter(|_| edit_tables.len() > 1)),
            edit_tables,
            paging,
            _subscriptions: subscriptions,
        });
    }

    /// Looks up the source table's columns and keys to decide how rows can be written back.
    fn detect_editability(
        &self,
        table: Entity<TableState<ResultGrid>>,
        schema: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        let session = session.clone();
        cx.spawn(async move |_, cx| {
            let (db, tbl) = (schema.clone(), name.clone());
            let meta = runtime::run(async move {
                let mut session = session.lock().await;
                let columns = session.list_columns(&db, &tbl).await?;
                let indexes = session.list_indexes(&db, &tbl).await?;
                Ok::<_, Error>((columns, indexes))
            })
            .await;
            table.update(cx, |table, cx| {
                let grid = table.delegate_mut();
                let editability = match meta {
                    Ok((columns, indexes)) => {
                        match EditTarget::detect_for(
                            grid.data(),
                            &schema,
                            &name,
                            &columns,
                            &indexes,
                        ) {
                            Some(target) => Editability::Editable(target),
                            None => Editability::ReadOnly("no editable columns".into()),
                        }
                    }
                    Err(err) => Editability::ReadOnly(format!("couldn't read table keys: {err}")),
                };
                grid.set_editability(editability);
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes the active result's pending edits in one transaction, then re-runs its query so
    /// the grid shows what the server stored.
    fn save_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        if self.running.is_some() {
            return;
        }
        let Some(result) = self.results.get(self.active_result) else {
            return;
        };
        let statements = result.table.read(cx).delegate().save_statements();
        let Some(source_sql) = result.source_sql.clone() else {
            return;
        };
        if statements.is_empty() {
            return;
        }
        let session = session.clone();
        let table = result.table.clone();
        let started = Instant::now();
        self.running = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = runtime::run(async move {
                let mut session = session.lock().await;
                session.execute("START TRANSACTION", None).await?;
                let mut reports = Vec::new();
                for sql in statements {
                    let t0 = Instant::now();
                    match session.execute(&sql, None).await {
                        Ok(outcomes) => reports.push(StatementReport {
                            sql,
                            outcomes,
                            elapsed: t0.elapsed(),
                            error: None,
                        }),
                        Err(err) => {
                            // Undo the statements that did succeed; report the original error.
                            let _ = session.execute("ROLLBACK", None).await;
                            reports.push(StatementReport {
                                sql,
                                outcomes: Vec::new(),
                                elapsed: t0.elapsed(),
                                error: Some(err.to_string()),
                            });
                            return Ok((reports, None));
                        }
                    }
                }
                session.execute("COMMIT", None).await?;
                let refreshed = session.execute(&source_sql, Some(MAX_FETCH_ROWS)).await?;
                let rs = refreshed.into_iter().find_map(|o| match o {
                    QueryOutcome::Rows(rs) => Some(rs),
                    QueryOutcome::Affected { .. } => None,
                });
                Ok::<_, Error>((reports, rs))
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                match outcome {
                    Ok((reports, refreshed)) => {
                        let failed = reports.iter().any(|r| r.error.is_some());
                        for report in &reports {
                            this.record_history(report);
                        }
                        this.reports.extend(reports);
                        if failed {
                            window.push_notification(
                                "Saving failed and was rolled back. See Messages for the error.",
                                cx,
                            );
                        } else if let Some(rs) = refreshed {
                            this.replace_result(&table, rs, cx);
                        }
                    }
                    Err(err) => {
                        this.push_note(format!("Saving failed: {err}"));
                        window.push_notification(format!("Saving failed: {err}"), cx);
                    }
                }
                this.last_run = Some(RunSummary {
                    statements: this.reports.len(),
                    errors: this.reports.iter().filter(|r| r.error.is_some()).count(),
                    elapsed: started.elapsed(),
                    rows: 0,
                });
                cx.emit(QueryEvent::StateChanged);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swaps in freshly fetched rows after a save, keeping the result editable.
    fn replace_result(
        &mut self,
        table: &Entity<TableState<ResultGrid>>,
        rs: ResultSet,
        cx: &mut Context<Self>,
    ) {
        let rows = rs.rows.len();
        let Some(ix) = self.results.iter().position(|r| &r.table == table) else {
            return;
        };
        // Keep the server-side sort visible on its column header.
        let sorted = self.results[ix].paging.as_ref().and_then(|p| {
            let sort = p.spec.sort.as_ref()?;
            let col = rs.columns.iter().position(|c| c.name == sort.column)?;
            let dir = if sort.descending {
                ColumnSort::Descending
            } else {
                ColumnSort::Ascending
            };
            Some((col, dir))
        });
        let server_sort = self.results[ix]
            .paging
            .as_ref()
            .filter(|p| {
                let r = &self.results[ix];
                p.is_partial(rows, r.total, r.hit_limit)
            })
            .map(|_| Self::server_sort_callback(ix, cx));
        let open_value = Self::open_value_callback(table, cx);
        table.update(cx, |table, cx| {
            let editability = std::mem::replace(
                table.delegate_mut().editability_mut(),
                Editability::Checking,
            );
            let mut grid = ResultGrid::new(Arc::new(rs), editability);
            if let Some((col, dir)) = sorted {
                grid = grid.with_sort(col, dir);
            }
            grid.set_server_sort(server_sort);
            grid.set_on_open_value(Some(open_value));
            *table.delegate_mut() = grid;
            table.refresh(cx);
            cx.notify();
        });
        self.results[ix].rows = rows;
    }

    fn server_sort_callback(ix: usize, cx: &mut Context<Self>) -> ServerSort {
        let this = cx.entity().downgrade();
        Rc::new(move |col, sort, cx: &mut gpui_kit::App| {
            this.update(cx, |this, cx| this.sort_page(ix, col, sort, cx))
                .ok();
        })
    }

    /// Sorts a partially loaded result on the server and shows the first page.
    fn sort_page(&mut self, ix: usize, col: usize, sort: ColumnSort, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(ix) else {
            return;
        };
        let column = result
            .table
            .read(cx)
            .delegate()
            .data()
            .columns
            .get(col)
            .map(|c| c.name.clone());
        let Some(paging) = self.results[ix].paging.as_mut() else {
            return;
        };
        paging.spec.sort = match (sort, column) {
            (ColumnSort::Ascending, Some(column)) => Some(SortSpec {
                column,
                descending: false,
            }),
            (ColumnSort::Descending, Some(column)) => Some(SortSpec {
                column,
                descending: true,
            }),
            _ => None,
        };
        paging.spec.offset = 0;
        self.fetch_page(ix, cx);
    }

    fn apply_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(paging) = self.results.get_mut(ix).and_then(|r| r.paging.as_mut()) else {
            return;
        };
        paging
            .filter_input
            .read(cx)
            .value()
            .trim()
            .clone_into(&mut paging.spec.filter);
        paging.spec.offset = 0;
        self.fetch_page(ix, cx);
    }

    fn clear_filter(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(paging) = self.results.get(ix).and_then(|r| r.paging.as_ref()) {
            paging
                .filter_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.apply_filter(ix, cx);
    }

    fn turn_page(&mut self, ix: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(paging) = self.results.get_mut(ix).and_then(|r| r.paging.as_mut()) else {
            return;
        };
        if forward {
            paging.spec.next();
        } else {
            paging.spec.previous();
        }
        self.fetch_page(ix, cx);
    }

    /// Fetches the page described by result `ix`'s paging state, re-counting the total when
    /// the filter changed. Refuses while edits are unsaved so they can't be lost.
    fn fetch_page(&mut self, ix: usize, cx: &mut Context<Self>) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        let session = session.clone();
        let Some(result) = self.results.get_mut(ix) else {
            return;
        };
        if !result.table.read(cx).delegate().pending().is_empty() {
            if let Some(p) = result.paging.as_mut() {
                p.error = Some("Save or discard your changes first.".into());
            }
            cx.notify();
            return;
        }
        let total = result.total;
        let Some(paging) = result.paging.as_mut() else {
            return;
        };
        if paging.loading {
            return;
        }
        let page_sql = paging.spec.page_query(ovs_core::sql::Dialect::MySql);
        let count_sql = (paging.counted_filter.as_deref() != Some(paging.spec.filter.as_str())
            || total.is_none())
        .then(|| paging.spec.count_query());
        let filter = paging.spec.filter.clone();
        paging.loading = true;
        paging.error = None;
        let table = result.table.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let page_query = page_sql.clone();
            let outcome = runtime::run(async move {
                let mut session = session.lock().await;
                let out = session.execute(&page_query, Some(MAX_FETCH_ROWS)).await?;
                let rs = out
                    .into_iter()
                    .find_map(|o| match o {
                        QueryOutcome::Rows(rs) => Some(rs),
                        QueryOutcome::Affected { .. } => None,
                    })
                    .unwrap_or_default();
                let total = match count_sql {
                    Some(sql) => session
                        .execute(&sql, None)
                        .await
                        .ok()
                        .and_then(|out| match out.into_iter().next()? {
                            QueryOutcome::Rows(rs) => {
                                rs.rows.first()?.first()?.to_string().parse().ok()
                            }
                            QueryOutcome::Affected { .. } => None,
                        })
                        .map(Some),
                    None => None,
                };
                Ok::<_, Error>((rs, total))
            })
            .await;
            this.update(cx, |this, cx| {
                let elapsed = started.elapsed();
                match outcome {
                    Ok((rs, total)) => {
                        if let Some(result) = this.results.get_mut(ix) {
                            if let Some(total) = total {
                                result.total = total;
                                if let Some(p) = result.paging.as_mut() {
                                    p.counted_filter = Some(filter);
                                }
                            }
                            let page_size = result
                                .paging
                                .as_ref()
                                .map_or(u64::MAX, |p| p.spec.page_size);
                            result.hit_limit = rs.rows.len() as u64 >= page_size;
                            result.auto_limited = false;
                            result.elapsed = elapsed;
                            if result.source_sql.is_some() {
                                result.source_sql = Some(page_sql);
                            }
                        }
                        this.replace_result(&table, rs, cx);
                    }
                    Err(err) => {
                        if let Some(p) = this.results.get_mut(ix).and_then(|r| r.paging.as_mut()) {
                            p.error = Some(err.to_string());
                        }
                    }
                }
                if let Some(p) = this.results.get_mut(ix).and_then(|r| r.paging.as_mut()) {
                    p.loading = false;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_filter_bar(&self, ix: usize, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let paging = self.results.get(ix)?.paging.as_ref()?;
        let active = !paging.spec.filter.is_empty();
        Some(
            h_flex()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(cx.theme().muted_foreground)
                        .child("WHERE"),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&paging.filter_input).xsmall()),
                )
                .when(active, |row| {
                    row.child(
                        Button::new("clear-filter")
                            .ghost()
                            .xsmall()
                            .label("Clear")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.clear_filter(ix, window, cx);
                            })),
                    )
                }),
        )
    }

    /// Opens the Export dialog for the active result.
    fn open_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(self.active_result) else {
            return;
        };
        let data = result.table.read(cx).delegate().data().clone();
        // "All rows" only makes sense when a LIMIT (or the fetch cap) cut the result short.
        let fetch_sql = result
            .source_sql
            .clone()
            .filter(|_| result.hit_limit || result.truncated);
        let table = single_source_table(&data).map_or_else(|| "result".to_owned(), |(_, t)| t);
        let shown = data.rows.len();
        let has_more = fetch_sql.is_some();
        let form = cx.new(|cx| ExportForm::new(shown, has_more, table, window, cx));
        let this = cx.entity().downgrade();
        let title = self.title.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (form, this, data) = (form.clone(), this.clone(), data.clone());
            let (fetch_sql, title) = (fetch_sql.clone(), title.clone());
            dialog
                .title("Export result")
                .w(px(640.))
                .child(form.clone())
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("export-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("export-go")
                                .primary()
                                .label("Export…")
                                .on_click(move |_, window, cx| {
                                    let (options, all) = {
                                        let form = form.read(cx);
                                        (form.options(cx), form.all_rows())
                                    };
                                    window.close_dialog(cx);
                                    let fetch = fetch_sql.clone().filter(|_| all);
                                    let (data, title) = (data.clone(), title.clone());
                                    this.update(cx, |this, cx| {
                                        this.run_export(data, fetch, options, &title, window, cx);
                                    })
                                    .ok();
                                }),
                        ),
                )
        });
    }

    /// Asks where to save, then writes the export, re-running `fetch_sql` without its LIMIT
    /// first when every row was requested.
    fn run_export(
        &mut self,
        data: Arc<ResultSet>,
        fetch_sql: Option<String>,
        options: ExportOptions,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SessionState::Ready { session, .. } = &self.session else {
            return;
        };
        let session = session.clone();
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let safe_title: String = title
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "-_ ".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!("{}.{}", safe_title.trim(), options.format.extension());
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let target = path.clone();
            let outcome = runtime::run(async move {
                let rs = match fetch_sql {
                    Some(sql) => {
                        let out = session
                            .lock()
                            .await
                            .execute(&sql::strip_limit(&sql), None)
                            .await
                            .map_err(|e| e.to_string())?;
                        out.into_iter()
                            .find_map(|o| match o {
                                QueryOutcome::Rows(rs) => Some(rs),
                                QueryOutcome::Affected { .. } => None,
                            })
                            .ok_or("the query returned no rows")?
                    }
                    None => (*data).clone(),
                };
                let file = std::fs::File::create(&target).map_err(|e| e.to_string())?;
                let mut out = std::io::BufWriter::new(file);
                export(&rs, &options, &mut out)
                    .and_then(|()| std::io::Write::flush(&mut out))
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>(rs.rows.len())
            })
            .await;
            cx.update(|window, cx| {
                let message = match outcome {
                    Ok(rows) => format!("Exported {rows} rows to {}", path.display()),
                    Err(err) => format!("Export failed: {err}"),
                };
                window.push_notification(message, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Drops unsaved edits in every result of this tab.
    pub fn discard_all_edits(&mut self, cx: &mut Context<Self>) {
        for result in &self.results {
            result.table.update(cx, |table, cx| {
                table.delegate_mut().discard();
                table.refresh(cx);
                cx.notify();
            });
        }
    }

    fn open_value_callback(
        table: &Entity<TableState<ResultGrid>>,
        cx: &mut Context<Self>,
    ) -> OpenValue {
        let this = cx.entity().downgrade();
        let table = table.downgrade();
        Rc::new(move |row, col, window, cx| {
            let Some(table) = table.upgrade() else { return };
            this.update(cx, |_, cx| {
                Self::open_value_editor(&table, row, col, window, cx);
            })
            .ok();
        })
    }

    /// A larger view of one cell's value: an editable text area, or hex with file import and
    /// export for binary values.
    fn open_value_editor(
        table: &Entity<TableState<ResultGrid>>,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let grid = table.read(cx).delegate();
        let value = grid.cell_value(row, col);
        let editable = grid.is_cell_editable(row, col);
        let column = grid.column_name(col).unwrap_or_default().to_owned();
        let binary = matches!(value, Some(Value::Bytes(_))) || grid.is_binary_column(col);
        let table = table.clone();
        let set = {
            let table = table.clone();
            move |value: Value, cx: &mut gpui_kit::App| {
                table.update(cx, |t, cx| {
                    t.delegate_mut().set_cell_value(row, col, value);
                    t.refresh(cx);
                    cx.notify();
                });
            }
        };

        if binary {
            let bytes = match &value {
                Some(Value::Bytes(b)) => b.clone(),
                Some(v) => v.to_string().into_bytes(),
                None => Vec::new(),
            };
            window.open_dialog(cx, move |dialog, _, cx| {
                let (set, bytes) = (set.clone(), bytes.clone());
                let shown = &bytes[..bytes.len().min(VALUE_HEX_PREVIEW)];
                let hex: String = shown
                    .chunks(16)
                    .map(|line| {
                        line.iter().fold(String::new(), |mut out, b| {
                            let _ = write!(out, "{b:02X} ");
                            out
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                dialog
                    .title(format!("{column} · {} bytes", bytes.len()))
                    .w(px(640.))
                    .child(
                        div()
                            .id("hex")
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .font_family("monospace")
                            .text_xs()
                            .whitespace_normal()
                            .child(hex)
                            .when(bytes.len() > VALUE_HEX_PREVIEW, |d| {
                                d.child(
                                    div()
                                        .pt_2()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Showing the first 64 KB."),
                                )
                            }),
                    )
                    .footer(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("value-save-file")
                                    .label("Save to file…")
                                    .on_click({
                                        let bytes = bytes.clone();
                                        move |_, _, cx| save_bytes_to_file(bytes.clone(), cx)
                                    }),
                            )
                            .when(editable, |row| {
                                let (load_set, null_set) = (set.clone(), set.clone());
                                row.child(
                                    Button::new("value-load-file")
                                        .label("Load from file…")
                                        .on_click(move |_, window, cx| {
                                            load_bytes_from_file(load_set.clone(), window, cx);
                                        }),
                                )
                                .child(
                                    Button::new("value-null").label("Set NULL").on_click(
                                        move |_, window, cx| {
                                            null_set(Value::Null, cx);
                                            window.close_dialog(cx);
                                        },
                                    ),
                                )
                            })
                            .child(
                                Button::new("value-close")
                                    .label("Close")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            ),
                    )
            });
            return;
        }

        let text = match &value {
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        };
        let area = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(14)
                .default_value(text.clone())
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let (set, area) = (set.clone(), area.clone());
            dialog
                .title(if editable {
                    format!("Edit {column}")
                } else {
                    format!("{column} (read-only)")
                })
                .w(px(640.))
                .child(Textarea::new(&area))
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(Button::new("value-copy").label("Copy").on_click({
                            let area = area.clone();
                            move |_, _, cx| {
                                let text = area.read(cx).value().to_string();
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                            }
                        }))
                        .when(editable, |row| {
                            let null_set = set.clone();
                            row.child(Button::new("value-null").label("Set NULL").on_click(
                                move |_, window, cx| {
                                    null_set(Value::Null, cx);
                                    window.close_dialog(cx);
                                },
                            ))
                        })
                        .child(
                            Button::new("value-cancel")
                                .label(if editable { "Cancel" } else { "Close" })
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when(editable, |row| {
                            let (set, area) = (set.clone(), area.clone());
                            row.child(
                                Button::new("value-apply")
                                    .primary()
                                    .label("Apply")
                                    .on_click(move |_, window, cx| {
                                        let text = area.read(cx).value().to_string();
                                        set(Value::Text(text), cx);
                                        window.close_dialog(cx);
                                    }),
                            )
                        }),
                )
        });
    }

    /// Switches which table of a join is editable.
    fn choose_edit_table(
        &mut self,
        ix: usize,
        schema: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(result) = self.results.get_mut(ix) else {
            return;
        };
        if !result.table.read(cx).delegate().pending().is_empty() {
            if let Some(p) = result.paging.as_mut() {
                p.error = Some("Save or discard your changes first.".into());
            }
            cx.notify();
            return;
        }
        result.edit_table = Some(name.clone());
        let table = result.table.clone();
        table.update(cx, |t, cx| {
            t.delegate_mut().set_editability(Editability::Checking);
            cx.notify();
        });
        self.detect_editability(table, schema, name, cx);
        cx.notify();
    }

    fn discard_edits(&mut self, cx: &mut Context<Self>) {
        if let Some(result) = self.results.get(self.active_result) {
            result.table.update(cx, |table, cx| {
                table.delegate_mut().discard();
                table.refresh(cx);
                cx.notify();
            });
        }
    }

    fn add_row(&mut self, cx: &mut Context<Self>) {
        if let Some(result) = self.results.get(self.active_result) {
            result.table.update(cx, |table, cx| {
                if let Some(row) = table.delegate_mut().add_row() {
                    table.refresh(cx);
                    table.scroll_to_row(row, cx);
                }
                cx.notify();
            });
        }
    }

    /// Save, Discard, add row and Export, shown in the results tab bar so they're easy to find.
    fn render_result_actions(
        &self,
        result: &ResultTab,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // For joins: which table's columns are editable.
        let table_picker = (result.edit_tables.len() > 1).then(|| {
            let this = cx.entity().downgrade();
            let tables = result.edit_tables.clone();
            let current = result.edit_table.clone();
            Button::new("edit-table")
                .ghost()
                .xsmall()
                .label(match &current {
                    Some(t) => format!("Editing {t}"),
                    None => "Edit table…".to_owned(),
                })
                .dropdown_caret(true)
                .tooltip("Choose which table's columns can be edited")
                .dropdown_menu(move |mut menu, _, _| {
                    for (schema, name) in &tables {
                        let this = this.clone();
                        let (schema, name) = (schema.clone(), name.clone());
                        menu = menu.item(
                            PopupMenuItem::new(name.clone())
                                .checked(current.as_deref() == Some(name.as_str()))
                                .on_click(move |_, _, cx| {
                                    let (schema, name) = (schema.clone(), name.clone());
                                    this.update(cx, |this, cx| {
                                        this.choose_edit_table(ix, schema, name, cx);
                                    })
                                    .ok();
                                }),
                        );
                    }
                    menu
                })
        });
        let grid = result.table.read(cx).delegate();
        let saving = self.running.is_some();
        let mut row = h_flex().gap_1().pr_1().items_center();
        if let Some(picker) = table_picker {
            row = row.child(picker);
        }
        if let Editability::Editable(_) = grid.editability() {
            let pending = grid.pending().row_count();
            if pending > 0 {
                row =
                    row.child(div().text_xs().text_color(cx.theme().warning).px_1().child(
                        format!(
                            "{pending} unsaved change{}",
                            if pending == 1 { "" } else { "s" }
                        ),
                    ))
                    .child(
                        Button::new("save-edits")
                            .primary()
                            .xsmall()
                            .label("Save")
                            .loading(saving)
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_edits(window, cx);
                            })),
                    )
                    .child(
                        Button::new("discard-edits")
                            .xsmall()
                            .label("Discard")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))),
                    );
            }
            row = row.child(
                Button::new("add-row")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .label("Row")
                    .tooltip("Add a row")
                    .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
            );
        }
        row.child(
            Button::new("export")
                .ghost()
                .xsmall()
                .icon(IconName::ArrowDown)
                .label("Export")
                .tooltip("Export to CSV, JSON, SQL, Markdown or HTML")
                .on_click(cx.listener(|this, _, window, cx| this.open_export(window, cx))),
        )
    }

    /// Row count and timing on the left; editability hints on the right.
    fn render_result_footer(
        result: &ResultTab,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pager = result.paging.as_ref().map(|p| {
            let (prev, next) = (
                p.spec.has_previous(),
                p.spec.has_next(result.rows as u64, result.total),
            );
            h_flex()
                .gap_1()
                .when_some(p.error.clone(), |row, err| {
                    row.child(div().text_color(cx.theme().danger).child(err))
                })
                .when(p.loading, |row| {
                    row.child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Loading…"),
                    )
                })
                .child(
                    Button::new("page-prev")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .label("Prev")
                        .disabled(!prev || p.loading)
                        .on_click(cx.listener(move |this, _, _, cx| this.turn_page(ix, false, cx))),
                )
                .child(
                    Button::new("page-next")
                        .ghost()
                        .xsmall()
                        .label("Next")
                        .icon(IconName::ChevronRight)
                        .disabled(!next || p.loading)
                        .on_click(cx.listener(move |this, _, _, cx| this.turn_page(ix, true, cx))),
                )
        });
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let grid = result.table.read(cx).delegate();
        let hint = match grid.editability() {
            Editability::Checking => div(),
            Editability::ReadOnly(reason) => div()
                .text_color(muted)
                .child(format!("Read-only: {reason}")),
            Editability::Editable(target) if !target.has_unique_key() => div()
                .text_color(warning)
                .child("No usable key: rows are matched on all columns (LIMIT 1)"),
            Editability::Editable(_) => {
                div().text_color(muted).child("Double-click a cell to edit")
            }
        };
        h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .child(div().text_color(muted).child(result_footer(result)))
            .child(h_flex().gap_3().child(hint).children(pager))
    }

    /// Inserts `text` at the editor cursor (replacing any selection) and focuses the editor.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.editor.update(cx, |editor, cx| {
            if editor.selected_range().is_empty() {
                editor.insert(text, window, cx);
            } else {
                editor.replace(text, window, cx);
            }
            editor.focus(window, cx);
        });
    }

    fn record_history(&self, report: &StatementReport) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // History is a convenience; a failure to write it must not interrupt the user.
        let _ = self.store.append_history(HistoryEntry {
            at,
            connection: self.config.name.clone(),
            database: self.database.clone(),
            sql: report.sql.clone(),
        });
    }

    fn push_note(&mut self, message: String) {
        self.reports.push(StatementReport {
            sql: String::new(),
            outcomes: Vec::new(),
            elapsed: Duration::ZERO,
            error: Some(message),
        });
    }

    fn finish(&mut self, elapsed: Duration, cx: &mut Context<Self>) {
        self.running = None;
        let errors = self.reports.iter().filter(|r| r.error.is_some()).count();
        self.last_run = Some(RunSummary {
            statements: self.reports.len(),
            errors,
            elapsed,
            rows: self
                .reports
                .iter()
                .flat_map(StatementReport::result_sets)
                .map(|rs| rs.rows.len())
                .sum(),
        });
        // Errors are easy to miss behind a result grid, so surface Messages when any occurred.
        self.active_result = if errors > 0 || self.results.is_empty() {
            self.results.len()
        } else {
            0
        };
        cx.emit(QueryEvent::StateChanged);
        cx.notify();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.running.is_none() {
            return;
        }
        if let SessionState::Ready { cancel, .. } = &self.session {
            let cancel = cancel.clone();
            cx.spawn(async move |this, cx| {
                if let Err(err) = runtime::run(async move { cancel.cancel().await }).await {
                    this.update(cx, |this, cx| {
                        this.push_note(format!("Could not stop the query: {err}"));
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let errors = self.reports.iter().filter(|r| r.error.is_some()).count();
        let header = if self.reports.is_empty() {
            "No statements executed yet.".to_owned()
        } else {
            format!(
                "{} statement(s) executed, {} succeeded, {} failed",
                self.reports.len(),
                self.reports.len() - errors,
                errors
            )
        };
        v_flex()
            .id("messages")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_2()
            .text_sm()
            .child(div().text_color(muted).child(header))
            .children(self.reports.iter().enumerate().map(|(ix, r)| {
                let mut sql: String = r.sql.chars().take(MESSAGE_SQL_CHARS).collect();
                if sql.len() < r.sql.len() {
                    sql.push('…');
                }
                v_flex()
                    .id(("report", ix))
                    .gap_0p5()
                    .pb_2()
                    .border_b_1()
                    .border_color(border)
                    .when(!sql.is_empty(), |el| {
                        el.child(div().font_family("monospace").child(sql))
                    })
                    .child(
                        div()
                            .when(r.error.is_some(), |el| el.text_color(danger))
                            .child(r.summary()),
                    )
                    .when(!sql_is_note(r), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("Execution time: {}", format_duration(r.elapsed))),
                        )
                    })
            }))
    }
}

/// The line under a result grid, e.g. "Showing 50 of 20,000 rows · 12 ms".
fn result_footer(r: &ResultTab) -> String {
    let rows = thousands(r.rows as u64);
    let added = if r.auto_limited {
        format!(" (LIMIT {DEFAULT_ROW_LIMIT} added automatically)")
    } else {
        String::new()
    };
    let offset = r.paging.as_ref().map_or(0, |p| p.spec.offset);
    let mut text = match r.total {
        // A page further in: "Showing 51–100 of 20,000 rows".
        Some(total) if offset > 0 || total > r.rows as u64 => {
            let first = if r.rows == 0 { offset } else { offset + 1 };
            let last = offset + r.rows as u64;
            format!(
                "Showing {}–{} of {} rows{added}",
                thousands(first),
                thousands(last),
                thousands(total)
            )
        }
        None if r.hit_limit => format!("{rows} rows returned{added}; more rows may exist"),
        _ => format!("{rows} row{} returned", if r.rows == 1 { "" } else { "s" }),
    };
    if r.truncated {
        text = format!(
            "{text} (stopped reading at {} rows)",
            thousands(MAX_FETCH_ROWS as u64)
        );
    }
    text.push_str(" · ");
    text.push_str(&format_duration(r.elapsed));
    text
}

/// `20000` → `20,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn sql_is_note(report: &StatementReport) -> bool {
    report.sql.is_empty()
}

pub fn format_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let messages_ix = self.results.len();
        let active = self.active_result.min(messages_ix);
        let mut tabs: Vec<Tab> = self
            .results
            .iter()
            .map(|r| Tab::new().label(r.label.clone()))
            .collect();
        tabs.push(Tab::new().label("Messages"));

        let actions = self.results.get(active).map(|result| {
            self.render_result_actions(result, active, cx)
                .into_any_element()
        });
        let body = if let Some(result) = self.results.get(active) {
            v_flex()
                .size_full()
                .children(self.render_filter_bar(active, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(DataTable::new(&result.table).small().stripe(true)),
                )
                .child(Self::render_result_footer(result, active, cx))
                .into_any_element()
        } else {
            self.render_messages(cx).into_any_element()
        };

        let banner = match &self.session {
            SessionState::Connecting => {
                Some(("Connecting…".to_owned(), cx.theme().muted_foreground))
            }
            SessionState::Failed(err) => {
                Some((format!("Connection failed: {err}"), cx.theme().danger))
            }
            SessionState::Ready { .. } if self.running.is_some() => Some((
                "Executing… (⌘. / Ctrl+. to stop)".to_owned(),
                cx.theme().muted_foreground,
            )),
            SessionState::Ready { .. } => None,
        };

        v_resizable(SharedString::from(format!(
            "query-split-{}",
            cx.entity_id()
        )))
        .child(
            resizable_panel()
                .size(px(280.))
                .size_range(px(80.)..px(2000.))
                .child(Editor::new(&self.editor).size_full().bordered(false)),
        )
        .child(
            v_flex()
                .size_full()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    TabBar::new("result-tabs")
                        .underline()
                        .small()
                        .selected_index(active)
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            this.active_result = *ix;
                            cx.notify();
                        }))
                        .children(tabs)
                        .when_some(actions, TabBar::suffix),
                )
                .when_some(banner, |el, (text, color)| {
                    el.child(div().px_2().py_1().text_sm().text_color(color).child(text))
                })
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
        )
    }
}

/// Asks where to save and writes a binary value there.
fn save_bytes_to_file(bytes: Vec<u8>, cx: &mut gpui_kit::App) {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let chosen = cx.prompt_for_new_path(&dir, Some("value.bin"));
    cx.spawn(async move |_| {
        if let Ok(Ok(Some(path))) = chosen.await {
            // There's no window here to report into; a failed write leaves no file behind.
            let _ = std::fs::write(path, bytes);
        }
    })
    .detach();
}

/// Asks for a file and stores its bytes as the cell's new value.
fn load_bytes_from_file(
    set: impl Fn(Value, &mut gpui_kit::App) + 'static,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Load".into()),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let bytes = std::fs::read(&path);
            cx.update(|window, cx| match bytes {
                Ok(bytes) => {
                    set(Value::Bytes(bytes), cx);
                    window.close_dialog(cx);
                }
                Err(err) => {
                    window
                        .push_notification(format!("Could not read {}: {err}", path.display()), cx);
                }
            })
            .ok();
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(20_000), "20,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
