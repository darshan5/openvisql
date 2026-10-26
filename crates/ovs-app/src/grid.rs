//! Result grid backed by `DataTable`, with inline editing when the result maps to one table.
//!
//! Edits are kept as [`PendingEdits`] on top of the immutable result and only reach the server
//! when the query tab saves them.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    table::{Column, ColumnSort, TableDelegate, TableState},
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _,
    Styled as _, Subscription, Window, div, px,
};
use ovs_core::editing::{EditTarget, KeyPlan, PendingEdits};
use ovs_core::export::{ExportFormat, ExportOptions, export_to_string};
use ovs_core::{ResultSet, Value};

/// Rough average glyph width used to size columns before the user resizes them.
const CHAR_WIDTH: f32 = 7.5;
const SAMPLE_ROWS: usize = 200;

/// Whether the grid's rows can be changed.
pub enum Editability {
    /// Looking up the source table's keys.
    Checking,
    ReadOnly(String),
    Editable(EditTarget),
}

/// A row as displayed: an original result row (by data index) or a newly inserted one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Existing(usize),
    Inserted(usize),
}

/// What a cell displays.
enum Cell<'a> {
    Value(&'a Value),
    /// A cell of a new row left empty, so the column default will apply.
    Default,
}

struct CellEditor {
    row: usize,
    col: usize,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// Sorts on the server instead of in memory: called with the column index and direction.
pub type ServerSort = Rc<dyn Fn(usize, ColumnSort, &mut App)>;
/// Opens the value viewer/editor for a cell (display row, column).
pub type OpenValue = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;
/// Kills a server process: (process id, whole connection rather than just the query).
pub type KillProcess = Rc<dyn Fn(u64, bool, &mut Window, &mut App)>;

pub struct ResultGrid {
    data: Arc<ResultSet>,
    columns: Vec<Column>,
    /// Display order of original rows; sorting permutes this instead of the data. Inserted
    /// rows are shown after them.
    order: Vec<usize>,
    editability: Editability,
    pending: PendingEdits,
    editing: Option<CellEditor>,
    /// Last selected or right-clicked cell, as display coordinates.
    focused_cell: Option<(usize, usize)>,
    /// Set when only a page of the rows is loaded, so sorting must re-query.
    server_sort: Option<ServerSort>,
    on_open_value: Option<OpenValue>,
    /// Set for process-list results.
    on_kill: Option<KillProcess>,
}

impl ResultGrid {
    pub fn new(data: Arc<ResultSet>, editability: Editability) -> Self {
        let columns = data
            .columns
            .iter()
            .enumerate()
            .map(|(ix, col)| {
                Column::new(format!("c{ix}"), col.name.clone())
                    .width(px(initial_width(&data, ix)))
                    .sortable()
            })
            .collect();
        Self {
            order: (0..data.rows.len()).collect(),
            columns,
            data,
            editability,
            pending: PendingEdits::default(),
            editing: None,
            focused_cell: None,
            server_sort: None,
            on_open_value: None,
            on_kill: None,
        }
    }

    pub fn set_on_kill(&mut self, kill: Option<KillProcess>) {
        self.on_kill = kill;
    }

    /// The process id in a process-list row (its `ID` column).
    fn process_id(&self, row: usize) -> Option<u64> {
        let col = self
            .data
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case("id"))?;
        match self.cell(row, col)? {
            Cell::Value(Value::Int(n)) => u64::try_from(*n).ok(),
            Cell::Value(Value::UInt(n)) => Some(*n),
            Cell::Value(v) => v.to_string().parse().ok(),
            Cell::Default => None,
        }
    }

    pub fn set_on_open_value(&mut self, open: Option<OpenValue>) {
        self.on_open_value = open;
    }

    pub fn column_name(&self, col: usize) -> Option<&str> {
        self.data.columns.get(col).map(|c| c.name.as_str())
    }

    pub fn is_binary_column(&self, col: usize) -> bool {
        self.data.columns.get(col).is_some_and(|c| c.binary)
    }

    pub fn is_cell_editable(&self, row: usize, col: usize) -> bool {
        self.target().is_some_and(|t| t.is_column_editable(col))
            && !matches!(self.slot(row), Some(Slot::Existing(r)) if self.pending.is_deleted(r))
    }

    /// The value shown in a cell; `None` for a new row's cell left to its default.
    pub fn cell_value(&self, row: usize, col: usize) -> Option<Value> {
        match self.cell(row, col)? {
            Cell::Value(v) => Some(v.clone()),
            Cell::Default => None,
        }
    }

    /// Stores a new value for a cell as a pending change.
    pub fn set_cell_value(&mut self, row: usize, col: usize, value: Value) {
        if !self.is_cell_editable(row, col) {
            return;
        }
        match self.slot(row) {
            Some(Slot::Existing(r)) => {
                if let Some(original) = self.data.rows.get(r).and_then(|row| row.get(col)) {
                    self.pending.set_cell(r, col, value, original);
                }
            }
            Some(Slot::Inserted(i)) => self.pending.set_inserted_cell(i, col, Some(value)),
            None => {}
        }
    }

    /// Copies a row into a new unsaved row, leaving key columns empty so they get new values.
    pub fn duplicate_row(&mut self, row: usize) -> Option<usize> {
        let target = self.target()?;
        let keys: Vec<usize> = match &target.key {
            KeyPlan::Unique(cols) => cols.clone(),
            KeyPlan::AllColumns(_) => Vec::new(),
        };
        let values: Vec<Option<Value>> = (0..self.columns.len())
            .map(|col| {
                if keys.contains(&col) || !target.is_column_editable(col) {
                    None
                } else {
                    self.cell_value(row, col)
                }
            })
            .collect();
        let ix = self.pending.add_row_with(values);
        Some(self.order.len() + ix)
    }

    /// Shows `sort` on column `col`'s header, for rows that arrive already sorted.
    pub fn with_sort(mut self, col: usize, sort: ColumnSort) -> Self {
        if let Some(column) = self.columns.get_mut(col) {
            *column = column.clone().sort(sort);
        }
        self
    }

    pub fn set_server_sort(&mut self, server_sort: Option<ServerSort>) {
        self.server_sort = server_sort;
    }

    pub fn data(&self) -> &Arc<ResultSet> {
        &self.data
    }

    pub fn editability(&self) -> &Editability {
        &self.editability
    }

    pub fn editability_mut(&mut self) -> &mut Editability {
        &mut self.editability
    }

    pub fn set_editability(&mut self, editability: Editability) {
        self.editability = editability;
    }

    pub fn pending(&self) -> &PendingEdits {
        &self.pending
    }

    fn target(&self) -> Option<&EditTarget> {
        match &self.editability {
            Editability::Editable(t) => Some(t),
            _ => None,
        }
    }

    fn slot(&self, row: usize) -> Option<Slot> {
        if let Some(&data_row) = self.order.get(row) {
            return Some(Slot::Existing(data_row));
        }
        let ix = row - self.order.len();
        (ix < self.pending.inserted_rows().len()).then_some(Slot::Inserted(ix))
    }

    /// The value shown in a cell: pending change, original value, or `None` for an inserted
    /// cell left to its default.
    fn cell(&self, row: usize, col: usize) -> Option<Cell<'_>> {
        Some(match self.slot(row)? {
            Slot::Existing(r) => Cell::Value(
                self.pending
                    .changed_value(r, col)
                    .or_else(|| self.data.rows.get(r)?.get(col))?,
            ),
            Slot::Inserted(i) => match self.pending.inserted_rows().get(i)?.get(col)? {
                Some(v) => Cell::Value(v),
                None => Cell::Default,
            },
        })
    }

    pub fn focus_cell(&mut self, row: usize, col: usize) {
        self.focused_cell = Some((row, col));
    }

    /// Opens an inline editor on a cell, if the result and the column are editable.
    pub fn begin_edit(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let editable = self.target().is_some_and(|t| t.is_column_editable(col));
        let Some(slot) = self.slot(row) else { return };
        if !editable || matches!(slot, Slot::Existing(r) if self.pending.is_deleted(r)) {
            return;
        }
        let text = match self.cell(row, col) {
            Some(Cell::Value(Value::Null) | Cell::Default) | None => String::new(),
            Some(Cell::Value(v)) => v.to_string(),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |table: &mut TableState<Self>, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    table.delegate_mut().commit_edit(cx);
                    cx.notify();
                }
            },
        );
        self.editing = Some(CellEditor {
            row,
            col,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Stores the open editor's text as a pending change.
    fn commit_edit(&mut self, cx: &App) {
        let Some(editor) = self.editing.take() else {
            return;
        };
        let text = editor.input.read(cx).value().to_string();
        match self.slot(editor.row) {
            Some(Slot::Existing(r)) => {
                let Some(original) = self.data.rows.get(r).and_then(|row| row.get(editor.col))
                else {
                    return;
                };
                // Typing the same text back (or leaving a NULL cell empty) is not a change.
                let unchanged = match original {
                    Value::Null => text.is_empty(),
                    v => v.to_string() == text,
                };
                let value = if unchanged {
                    original.clone()
                } else {
                    Value::Text(text)
                };
                self.pending.set_cell(r, editor.col, value, original);
            }
            Some(Slot::Inserted(i)) => {
                // An empty cell in a new row means "use the column default".
                let value = (!text.is_empty()).then_some(Value::Text(text));
                self.pending.set_inserted_cell(i, editor.col, value);
            }
            None => {}
        }
    }

    pub fn set_null(&mut self, row: usize, col: usize) {
        if !self.target().is_some_and(|t| t.is_column_editable(col)) {
            return;
        }
        match self.slot(row) {
            Some(Slot::Existing(r)) => {
                if let Some(original) = self.data.rows.get(r).and_then(|row| row.get(col)) {
                    self.pending.set_cell(r, col, Value::Null, original);
                }
            }
            Some(Slot::Inserted(i)) => self.pending.set_inserted_cell(i, col, Some(Value::Null)),
            None => {}
        }
    }

    /// Marks an existing row for deletion (or un-marks it), or drops an inserted row.
    pub fn toggle_delete(&mut self, row: usize) {
        if self.target().is_none() {
            return;
        }
        self.editing = None;
        match self.slot(row) {
            Some(Slot::Existing(r)) => self.pending.toggle_delete(r),
            Some(Slot::Inserted(i)) => self.pending.remove_inserted_row(i),
            None => {}
        }
    }

    /// Appends an empty row and returns its display index.
    pub fn add_row(&mut self) -> Option<usize> {
        self.target()?;
        let ix = self.pending.add_row(self.columns.len());
        Some(self.order.len() + ix)
    }

    pub fn discard(&mut self) {
        self.editing = None;
        self.pending.clear();
    }

    /// The SQL that saves every pending change.
    pub fn save_statements(&self, dialect: ovs_core::sql::Dialect) -> Vec<String> {
        self.target().map_or_else(Vec::new, |target| {
            self.pending.statements(target, &self.data, dialect)
        })
    }
}

#[allow(clippy::cast_precision_loss)]
fn initial_width(data: &ResultSet, col_ix: usize) -> f32 {
    let header = data.columns[col_ix].name.chars().count() + 3;
    let widest = data
        .rows
        .iter()
        .take(SAMPLE_ROWS)
        .filter_map(|r| r.get(col_ix))
        .map(|v| v.display().chars().count())
        .max()
        .unwrap_or(0);
    (header.max(widest) as f32 * CHAR_WIDTH + 16.).clamp(60., 420.)
}

/// Tab-separated text, the format spreadsheets paste cleanly.
fn tsv_row(values: &[Value]) -> String {
    values
        .iter()
        .map(|v| match v {
            Value::Null => "NULL".to_owned(),
            v => v.to_string().replace(['\t', '\n', '\r'], " "),
        })
        .collect::<Vec<_>>()
        .join("\t")
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len() + self.pending.inserted_rows().len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.editing = None;
        if let Some(server_sort) = self.server_sort.clone() {
            // Runs after the table's update; the callback reads this table.
            cx.defer(move |cx| server_sort(col_ix, sort, cx));
            return;
        }
        let rows = &self.data.rows;
        match sort {
            ColumnSort::Default => self.order = (0..rows.len()).collect(),
            ColumnSort::Ascending => self
                .order
                .sort_by(|&a, &b| rows[a][col_ix].sort_cmp(&rows[b][col_ix])),
            ColumnSort::Descending => self
                .order
                .sort_by(|&a, &b| rows[b][col_ix].sort_cmp(&rows[a][col_ix])),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if let Some(editor) = &self.editing
            && editor.row == row_ix
            && editor.col == col_ix
        {
            return div()
                .w_full()
                .child(Input::new(&editor.input).xsmall())
                .into_any_element();
        }
        let theme = cx.theme();
        let slot = self.slot(row_ix);
        let deleted = matches!(slot, Some(Slot::Existing(r)) if self.pending.is_deleted(r));
        let changed = matches!(
            slot,
            Some(Slot::Existing(r)) if self.pending.changed_value(r, col_ix).is_some()
        );
        let inserted = matches!(slot, Some(Slot::Inserted(_)));

        let cell = match self.cell(row_ix, col_ix) {
            None => div(),
            Some(Cell::Default) => div()
                .italic()
                .text_color(theme.muted_foreground)
                .child("(default)"),
            Some(Cell::Value(Value::Null)) => div()
                .italic()
                .text_color(theme.muted_foreground)
                .child("(NULL)"),
            Some(Cell::Value(v @ (Value::Int(_) | Value::UInt(_) | Value::Float(_)))) => {
                div().w_full().text_right().child(v.display())
            }
            Some(Cell::Value(v)) => div().child(v.display()),
        };
        let cell = if deleted {
            cell.line_through().text_color(theme.danger)
        } else if changed {
            cell.bg(theme.warning.opacity(0.25))
        } else if inserted {
            cell.bg(theme.success.opacity(0.15))
        } else {
            cell
        };
        cell.w_full().into_any_element()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        match self.cell(row_ix, col_ix) {
            Some(Cell::Value(v)) => v.to_string(),
            Some(Cell::Default) | None => String::new(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let row_text = match self.slot(row_ix) {
            Some(Slot::Existing(r)) => self
                .data
                .rows
                .get(r)
                .map(|r| tsv_row(r))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let data = self.data.clone();
        let order = self.order.clone();
        let menu = menu
            .item(
                PopupMenuItem::new("Copy row").on_click(move |_, _, cx: &mut App| {
                    cx.write_to_clipboard(ClipboardItem::new_string(row_text.clone()));
                }),
            )
            .item(PopupMenuItem::new("Copy all rows with headers").on_click({
                let (data, order) = (data.clone(), order.clone());
                move |_, _, cx: &mut App| {
                    let header = data
                        .columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join("\t");
                    let mut text = header;
                    for &r in &order {
                        text.push('\n');
                        text.push_str(&tsv_row(&data.rows[r]));
                    }
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }));
        // Copy every row, in the order shown, in another format.
        let ordered = Arc::new(ResultSet {
            columns: data.columns.clone(),
            rows: order.iter().map(|&r| data.rows[r].clone()).collect(),
            truncated: data.truncated,
        });
        let copy_as = |format: ExportFormat| {
            let ordered = ordered.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let options = ExportOptions {
                    format,
                    ..ExportOptions::default()
                };
                cx.write_to_clipboard(ClipboardItem::new_string(export_to_string(
                    &ordered, &options,
                )));
            }
        };
        let menu = menu
            .item(PopupMenuItem::new("Copy all as CSV").on_click(copy_as(ExportFormat::Csv)))
            .item(PopupMenuItem::new("Copy all as JSON").on_click(copy_as(ExportFormat::Json)))
            .item(
                PopupMenuItem::new("Copy all as Markdown")
                    .on_click(copy_as(ExportFormat::Markdown)),
            )
            .item(
                PopupMenuItem::new("Copy all as SQL INSERT")
                    .on_click(copy_as(ExportFormat::SqlInsert)),
            );
        // Context menus act on the right-clicked row and the column last clicked in it.
        let col = self
            .focused_cell
            .filter(|(r, _)| *r == row_ix)
            .map(|(_, c)| c);
        // Row actions need the row even when no cell in it was clicked; `usize::MAX` marks
        // "no column", which no column-level action accepts.
        self.focused_cell = Some((row_ix, col.unwrap_or(usize::MAX)));
        let menu = match (col, self.on_open_value.clone()) {
            (Some(col), Some(open)) => menu.separator().item(
                PopupMenuItem::new("View / edit value…")
                    .on_click(move |_, window, cx| open(row_ix, col, window, cx)),
            ),
            _ => menu,
        };
        let menu = match (self.on_kill.clone(), self.process_id(row_ix)) {
            (Some(kill), Some(id)) => {
                let kill_connection = kill.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new(format!("Kill query (process {id})"))
                            .on_click(move |_, window, cx| kill(id, false, window, cx)),
                    )
                    .item(
                        PopupMenuItem::new(format!("Kill connection (process {id})…"))
                            .on_click(move |_, window, cx| kill_connection(id, true, window, cx)),
                    )
            }
            _ => menu,
        };
        if self.target().is_none() {
            return menu;
        }

        let table = cx.entity().downgrade();
        let edit = move |f: fn(&mut ResultGrid)| {
            let table = table.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                table
                    .update(cx, |table, cx| {
                        f(table.delegate_mut());
                        table.refresh(cx);
                        cx.notify();
                    })
                    .ok();
            }
        };
        let deleted =
            matches!(self.slot(row_ix), Some(Slot::Existing(r)) if self.pending.is_deleted(r));
        let menu = menu.separator();
        let menu = match col {
            Some(col) if self.target().is_some_and(|t| t.is_column_editable(col)) => menu.item(
                PopupMenuItem::new("Set cell to NULL").on_click(edit(|grid| {
                    if let Some((row, col)) = grid.focused_cell {
                        grid.set_null(row, col);
                    }
                })),
            ),
            _ => menu,
        };
        menu.item(PopupMenuItem::new("Duplicate row").on_click(edit(|grid| {
            if let Some((row, _)) = grid.focused_cell {
                grid.duplicate_row(row);
            }
        })))
        .item(
            PopupMenuItem::new(if deleted {
                "Undo delete row"
            } else {
                "Delete row"
            })
            .on_click(edit(|grid| {
                if let Some((row, _)) = grid.focused_cell {
                    grid.toggle_delete(row);
                }
            })),
        )
        .item(PopupMenuItem::new("Add row").on_click(edit(|grid| {
            grid.add_row();
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_flattens_control_characters() {
        let row = [Value::Int(1), Value::Null, Value::Text("a\tb\nc".into())];
        assert_eq!(tsv_row(&row), "1\tNULL\ta b c");
    }
}
