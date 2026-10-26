//! A query tab: SQL editor on top, results below. Each tab owns its own server session so a
//! long-running query in one tab never blocks another tab or the object browser.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent},
    resizable::{resizable_panel, v_resizable},
    tab::{Tab, TabBar},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::editing::{EditTarget, single_source_table};
use ovs_core::sql::{self, Statement, StatementKind};
use ovs_core::{
    CancelHandle, ConnectionConfig, Error, QueryOutcome, ResultSet, StatementReport, Value,
};
use ovs_store::{HistoryEntry, SavedQueryTab, Store};

use crate::grid::{Editability, ResultGrid};
use crate::runtime::{self, SharedSession};

/// `LIMIT` appended to plain SELECTs that don't have one.
pub const DEFAULT_ROW_LIMIT: usize = 50;
/// Hard cap per result set for statements we can't add a LIMIT to (SHOW, CALL, ...).
const MAX_FETCH_ROWS: usize = 50_000;
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
    _subscriptions: [Subscription; 2],
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
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(spec.sql)
        });
        // Re-render (and let the workspace refresh its Ln/Col) whenever the editor changes.
        cx.observe(&editor, |_, _, cx| {
            cx.emit(QueryEvent::StateChanged);
            cx.notify();
        })
        .detach();
        // Text edits are what the saved session cares about; cursor moves are not.
        cx.subscribe(&editor, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
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

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn rename(&mut self, title: SharedString, cx: &mut Context<Self>) {
        self.title = title;
        cx.notify();
    }

    /// The tab as it should be restored next time the app starts.
    pub fn snapshot(&self, cx: &gpui_kit::App) -> SavedQueryTab {
        SavedQueryTab {
            title: self.title.to_string(),
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
                    Ok(()) => this.database = Some(db),
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

    pub fn execute(&mut self, mode: RunMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
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
                let limits = Limits {
                    auto_limited,
                    hit_limit,
                    total,
                    source_sql,
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
                    self.database = Some(db.clone());
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
        let editability = if source.is_some() {
            Editability::Checking
        } else if limits.source_sql.is_none() {
            Editability::ReadOnly("only SELECT results can be edited".into())
        } else {
            Editability::ReadOnly("the result doesn't come from a single table".into())
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
        if let Some((_, (schema, name))) = &source {
            self.detect_editability(table.clone(), schema.clone(), name.clone(), cx);
        }
        self.results.push(ResultTab {
            label: format!("{n} Result").into(),
            table,
            rows,
            elapsed,
            total: limits.total,
            auto_limited: limits.auto_limited,
            hit_limit: limits.hit_limit,
            truncated,
            source_sql: source.map(|(sql, _)| sql),
            _subscriptions: [events, observe],
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
            let meta = runtime::run(async move {
                let mut session = session.lock().await;
                let columns = session.list_columns(&schema, &name).await?;
                let indexes = session.list_indexes(&schema, &name).await?;
                Ok::<_, Error>((columns, indexes))
            })
            .await;
            table.update(cx, |table, cx| {
                let grid = table.delegate_mut();
                let editability = match meta {
                    Ok((columns, indexes)) => {
                        match EditTarget::detect(grid.data(), &columns, &indexes) {
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
        table.update(cx, |table, cx| {
            let editability = std::mem::replace(
                table.delegate_mut().editability_mut(),
                Editability::Checking,
            );
            *table.delegate_mut() = ResultGrid::new(Arc::new(rs), editability);
            table.refresh(cx);
            cx.notify();
        });
        if let Some(result) = self.results.iter_mut().find(|r| &r.table == table) {
            result.rows = rows;
        }
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

    fn render_result_footer(&self, result: &ResultTab, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let grid = result.table.read(cx).delegate();
        let saving = self.running.is_some();
        let controls = match grid.editability() {
            Editability::Checking => h_flex(),
            Editability::ReadOnly(reason) => h_flex()
                .text_color(muted)
                .child(format!("Read-only: {reason}")),
            Editability::Editable(target) => {
                let pending = grid.pending().row_count();
                let mut row = h_flex().gap_2();
                if !target.has_unique_key() {
                    row = row.child(
                        div()
                            .text_color(warning)
                            .child("No usable key: rows are matched on all columns (LIMIT 1)"),
                    );
                }
                if pending == 0 {
                    row.child(div().text_color(muted).child("Double-click a cell to edit"))
                        .child(
                            Button::new("add-row")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .label("Row")
                                .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
                        )
                } else {
                    row.child(div().child(format!(
                        "{pending} pending change{}",
                        if pending == 1 { "" } else { "s" }
                    )))
                    .child(
                        Button::new("add-row")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("Row")
                            .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
                    )
                    .child(
                        Button::new("discard-edits")
                            .xsmall()
                            .label("Discard")
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))),
                    )
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
                }
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
            .child(controls)
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
    let mut text = match r.total {
        Some(total) if total > r.rows as u64 => {
            format!("Showing {rows} of {} rows{added}", thousands(total))
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
        tabs.push(Tab::new().label(format!("{} Messages", messages_ix + 1)));

        let body = if let Some(result) = self.results.get(active) {
            v_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(DataTable::new(&result.table).small().stripe(true)),
                )
                .child(self.render_result_footer(result, cx))
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
                        .children(tabs),
                )
                .when_some(banner, |el, (text, color)| {
                    el.child(div().px_2().py_1().text_sm().text_color(color).child(text))
                })
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
        )
    }
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
