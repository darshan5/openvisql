//! The visual query builder, in a window of its own. Tables are boxes on a canvas: tick
//! columns to select them, and drag from a column to a column of another table to join them
//! (foreign keys are joined for you). Each join line has a marker to change its kind. The grid
//! below sets aliases, sorting, aggregates, grouping and criteria per column. The SQL is shown
//! live.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _, PathBuilder,
    Pixels, Point, Render, ScrollWheelEvent, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, canvas, div, point, prelude::FluentBuilder as _, px,
};
use ovs_core::sql::Dialect;
use ovs_core::sql::builder::{
    Aggregate, ColumnRef, Criterion, JoinKind, OrderItem, Query, SelectItem, SelectPart,
};
use ovs_core::{ForeignKeyInfo, ObjectKind};

use crate::runtime::{self, SharedSession};

/// Opens the finished SQL in a query tab of the connection, running it when the flag is set.
pub type OpenSql = Rc<dyn Fn(String, bool, &mut App)>;

/// Box geometry on the canvas.
const HEADER: f32 = 26.;
const ROW: f32 = 22.;
const BOX_MIN_WIDTH: f32 = 170.;
const CHAR: f32 = 7.;

/// A table's columns: name and type.
type Columns = Vec<(String, String)>;

/// A column in the grid: shown in the output or only used for sorting, grouping or criteria.
struct RowUi {
    column: ColumnRef,
    visible: bool,
    alias: Entity<InputState>,
    /// `Some(descending)` when sorted.
    sort: Option<bool>,
    /// When sorting was turned on, to keep the sort order the user chose.
    sorted_at: usize,
    aggregate: Option<Aggregate>,
    group: bool,
    criteria: Entity<InputState>,
}

/// The `SELECT`: its tables and joins (in `part`), their places on the canvas, and its grid.
#[derive(Default)]
struct PartUi {
    part: SelectPart,
    boxes: HashMap<String, (f32, f32)>,
    rows: Vec<RowUi>,
}

enum Drag {
    Pan {
        mouse: Point<Pixels>,
        offset: Point<Pixels>,
    },
    Box {
        alias: String,
        mouse: Point<Pixels>,
        at: (f32, f32),
    },
    /// Drawing a join from a column; `to` follows the mouse.
    Link { from: ColumnRef, to: Point<Pixels> },
}

pub struct QueryBuilder {
    dialect: Dialect,
    db: String,
    session: SharedSession,
    open_sql: OpenSql,
    tables: Vec<String>,
    filter: Entity<InputState>,
    columns: HashMap<String, Columns>,
    foreign_keys: HashMap<String, Vec<ForeignKeyInfo>>,
    ui: PartUi,
    limit: Entity<InputState>,
    offset_rows: Entity<InputState>,
    /// Where the canvas origin is drawn, for panning.
    offset: Point<Pixels>,
    drag: Option<Drag>,
    canvas: Rc<Cell<Option<Bounds<Pixels>>>>,
    sort_counter: usize,
    loading: bool,
    error: Option<String>,
    subscriptions: Vec<Subscription>,
}

impl QueryBuilder {
    fn new(
        dialect: Dialect,
        db: String,
        session: SharedSession,
        tables: Vec<String>,
        open_sql: OpenSql,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let filter = input("Find a table", window, cx);
        let limit = input("none", window, cx);
        let offset_rows = input("0", window, cx);
        let mut builder = Self {
            dialect,
            db,
            session,
            open_sql,
            tables,
            filter: filter.clone(),
            columns: HashMap::new(),
            foreign_keys: HashMap::new(),
            ui: PartUi::default(),
            limit: limit.clone(),
            offset_rows: offset_rows.clone(),
            offset: point(px(24.), px(24.)),
            drag: None,
            canvas: Rc::default(),
            sort_counter: 0,
            loading: false,
            error: None,
            subscriptions: Vec::new(),
        };
        for input in [&filter, &limit, &offset_rows] {
            builder.watch(input, cx);
        }
        builder
    }

    fn watch(&mut self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        self.subscriptions
            .push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
    }

    fn part(&mut self) -> &mut PartUi {
        &mut self.ui
    }

    /// Loads `table`'s columns and foreign keys if needed, then adds it to the query.
    fn add_table(&mut self, table: String, cx: &mut Context<Self>) {
        if self.columns.contains_key(&table) {
            self.insert_table(&table, cx);
            return;
        }
        self.loading = true;
        cx.notify();
        let (session, db, name) = (self.session.clone(), self.db.clone(), table.clone());
        cx.spawn(async move |this, cx| {
            let loaded = runtime::run(async move {
                let mut s = session.lock().await;
                let columns = s.list_columns(&db, &name).await?;
                let fks = s.list_foreign_keys(&db, &name).await.unwrap_or_default();
                Ok::<_, ovs_core::Error>((columns, fks))
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match loaded {
                    Ok((columns, fks)) => {
                        let columns = columns
                            .into_iter()
                            .map(|c| (c.name, short_type(&c.column_type)))
                            .collect();
                        this.columns.insert(table.clone(), columns);
                        this.foreign_keys.insert(table.clone(), fks);
                        this.insert_table(&table, cx);
                    }
                    Err(err) => this.error = Some(format!("Could not load {table}: {err}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn insert_table(&mut self, table: &str, cx: &mut Context<Self>) {
        let fks = self.foreign_keys.clone();
        let part = self.part();
        part.part
            .add_table(table, &|t| fks.get(t).cloned().unwrap_or_default());
        let added = part.part.tables().last().map(|t| t.alias.clone());
        // The table it was joined to, if a foreign key joined it.
        let partner = part
            .part
            .joins
            .last()
            .filter(|j| Some(&j.table.alias) == added.as_ref())
            .and_then(|j| j.on.first())
            .map(|(earlier, _)| earlier.alias.clone());
        if let Some(alias) = added {
            let place = self.free_spot(table, partner.as_deref());
            self.part().boxes.insert(alias, place);
        }
        self.error = None;
        cx.notify();
    }

    /// Where a new box for `table` goes: right of the table it joins to (`partner`), or right
    /// of everything, moved down past any box already there.
    fn free_spot(&self, table: &str, partner: Option<&str>) -> (f32, f32) {
        const GAP: f32 = 110.;
        let ui = &self.ui;
        let placed: Vec<(f32, f32, f32, f32)> = ui
            .part
            .tables()
            .into_iter()
            .filter_map(|t| {
                let &(x, y) = ui.boxes.get(&t.alias)?;
                Some((x, y, self.box_width(&t.table), self.box_height(&t.table)))
            })
            .collect();
        if placed.is_empty() {
            return (0., 0.);
        }
        let partner_box = partner.and_then(|alias| {
            let t = ui.part.tables().into_iter().find(|t| t.alias == alias)?;
            let &(x, y) = ui.boxes.get(alias)?;
            Some((x, y, self.box_width(&t.table)))
        });
        let (x, mut y) = match partner_box {
            Some((px_, py, pw)) => (px_ + pw + GAP, py),
            None => (
                placed.iter().map(|b| b.0 + b.2).fold(0., f32::max) + GAP,
                0.,
            ),
        };
        let (w, h) = (self.box_width(table), self.box_height(table));
        // Step down until it overlaps nothing, with room between boxes.
        while let Some(&(_, by, _, bh)) = placed.iter().find(|b| {
            x < b.0 + b.2 + GAP / 2.
                && b.0 < x + w + GAP / 2.
                && y < b.1 + b.3 + 30.
                && b.1 < y + h + 30.
        }) {
            y = by + bh + 40.;
        }
        (x, y)
    }

    fn box_width(&self, table: &str) -> f32 {
        let widest = self
            .columns
            .get(table)
            .into_iter()
            .flatten()
            .map(|(name, ty)| name.chars().count() + ty.chars().count() + 6)
            .chain(std::iter::once(table.chars().count() + 10))
            .max()
            .unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let width = widest as f32 * CHAR + 24.;
        width.max(BOX_MIN_WIDTH)
    }

    fn box_height(&self, table: &str) -> f32 {
        let rows = self.columns.get(table).map_or(0, Vec::len) + 1;
        #[allow(clippy::cast_precision_loss)]
        let rows = rows as f32;
        HEADER + rows * ROW + 4.
    }

    fn remove_table(&mut self, alias: &str, cx: &mut Context<Self>) {
        let part = self.part();
        part.part.remove_table(alias);
        part.boxes.remove(alias);
        part.rows.retain(|r| r.column.alias != alias);
        cx.notify();
    }

    /// The point where a column's row meets the side of its box facing `toward_x`.
    fn anchor(&self, column: &ColumnRef, toward_x: f32) -> Option<(f32, f32)> {
        let ui = &self.ui;
        let table = ui
            .part
            .tables()
            .into_iter()
            .find(|t| t.alias == column.alias)?;
        let &(x, y) = ui.boxes.get(&column.alias)?;
        let row = if column.column == "*" {
            0
        } else {
            self.columns
                .get(&table.table)?
                .iter()
                .position(|(name, _)| *name == column.column)?
                + 1
        };
        let width = self.box_width(&table.table);
        #[allow(clippy::cast_precision_loss)]
        let row_y = y + HEADER + row as f32 * ROW + ROW / 2.;
        let side = if toward_x < x + width / 2. {
            x
        } else {
            x + width
        };
        Some((side, row_y))
    }

    /// `column`'s anchor on the side of its box facing `other`'s box.
    fn anchor_toward(&self, column: &ColumnRef, other: &ColumnRef) -> Option<(f32, f32)> {
        let ui = &self.ui;
        let &(ox, _) = ui.boxes.get(&other.alias)?;
        let other_table = ui
            .part
            .tables()
            .into_iter()
            .find(|t| t.alias == other.alias)?;
        let other_mid = ox + self.box_width(&other_table.table) / 2.;
        self.anchor(column, other_mid)
    }

    fn row_of(&self, column: &ColumnRef) -> Option<usize> {
        self.ui.rows.iter().position(|r| r.column == *column)
    }

    /// Ticking a column shows it; unticking hides it, keeping it in the grid if it still sorts,
    /// groups or filters.
    fn toggle_column(&mut self, column: ColumnRef, window: &mut Window, cx: &mut Context<Self>) {
        match self.row_of(&column) {
            Some(ix) => {
                let row = &mut self.ui.rows[ix];
                if row.visible {
                    let used = row.sort.is_some()
                        || row.group
                        || !row.criteria.read(cx).value().trim().is_empty();
                    if used {
                        row.visible = false;
                    } else {
                        self.ui.rows.remove(ix);
                    }
                } else {
                    row.visible = true;
                }
            }
            None => self.push_row(column, window, cx),
        }
        cx.notify();
    }

    fn push_row(&mut self, column: ColumnRef, window: &mut Window, cx: &mut Context<Self>) {
        let alias = cx.new(|cx| InputState::new(window, cx));
        let criteria =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. > 100 or LIKE 'a%'"));
        self.watch(&alias, cx);
        self.watch(&criteria, cx);
        self.ui.rows.push(RowUi {
            column,
            visible: true,
            alias,
            sort: None,
            sorted_at: 0,
            aggregate: None,
            group: false,
            criteria,
        });
    }

    /// Joins the tables of two columns on them: the later table's join gets the pair.
    fn link(&mut self, a: &ColumnRef, b: &ColumnRef, cx: &mut Context<Self>) {
        if a.alias == b.alias || a.column == "*" || b.column == "*" {
            return;
        }
        let part = &mut self.ui.part;
        let order: Vec<String> = part.tables().iter().map(|t| t.alias.clone()).collect();
        let (Some(ia), Some(ib)) = (
            order.iter().position(|x| *x == a.alias),
            order.iter().position(|x| *x == b.alias),
        ) else {
            return;
        };
        let (earlier, later, later_ix) = if ia < ib { (a, b, ib) } else { (b, a, ia) };
        let Some(join) = part.joins.get_mut(later_ix - 1) else {
            return;
        };
        let pair = (earlier.clone(), later.clone());
        if !join.on.contains(&pair) {
            join.on.push(pair);
        }
        if join.kind == JoinKind::Cross {
            join.kind = JoinKind::Inner;
        }
        cx.notify();
    }

    /// The model, with every input's current text.
    fn query(&self, cx: &App) -> Query {
        let number = |input: &Entity<InputState>| input.read(cx).value().trim().parse().ok();
        let ui = &self.ui;
        let mut part = ui.part.clone();
        part.columns = ui
            .rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| SelectItem {
                column: r.column.clone(),
                aggregate: r.aggregate,
                alias: r.alias.read(cx).value().trim().to_owned(),
            })
            .collect();
        part.criteria = ui
            .rows
            .iter()
            .map(|r| Criterion {
                column: r.column.clone(),
                text: r.criteria.read(cx).value().to_string(),
            })
            .collect();
        part.group_by = ui
            .rows
            .iter()
            .filter(|r| r.group)
            .map(|r| r.column.clone())
            .collect();
        let mut sorted: Vec<&RowUi> = ui.rows.iter().filter(|r| r.sort.is_some()).collect();
        sorted.sort_by_key(|r| r.sorted_at);
        part.order_by = sorted
            .iter()
            .map(|r| OrderItem {
                column: r.column.clone(),
                descending: r.sort == Some(true),
            })
            .collect();
        part.limit = number(&self.limit);
        part.offset = number(&self.offset_rows);
        Query {
            first: part,
            ..Query::default()
        }
    }

    pub fn sql(&self, cx: &App) -> Result<String, String> {
        self.query(cx).sql(self.dialect, &self.db)
    }

    fn finish(&mut self, run: bool, cx: &mut Context<Self>) {
        match self.sql(cx) {
            Ok(sql) => (self.open_sql)(sql, run, cx),
            Err(err) => self.error = Some(err),
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            if self.drag.take().is_some() {
                cx.notify();
            }
            return;
        }
        match &mut self.drag {
            Some(Drag::Pan { mouse, offset }) => {
                self.offset = *offset + (event.position - *mouse);
                cx.notify();
            }
            Some(Drag::Box { alias, mouse, at }) => {
                let delta = event.position - *mouse;
                let to = (at.0 + f32::from(delta.x), at.1 + f32::from(delta.y));
                let alias = alias.clone();
                self.part().boxes.insert(alias, to);
                cx.notify();
            }
            Some(Drag::Link { to, .. }) => {
                *to = event.position;
                cx.notify();
            }
            None => {}
        }
    }
}

/// Types as the boxes show them: without lengths or options that make them wide.
fn short_type(column_type: &str) -> String {
    let t = column_type.trim();
    let t = t.split('(').next().unwrap_or(t);
    t.split_whitespace().next().unwrap_or(t).to_owned()
}

/// A small dropdown button showing `label`; picking item `ix` calls `pick`.
fn picker(
    id: impl Into<gpui_kit::ElementId>,
    label: impl Into<SharedString>,
    width: f32,
    items: Vec<String>,
    pick: impl Fn(&mut QueryBuilder, usize, &mut Window, &mut Context<QueryBuilder>) + 'static,
    cx: &mut Context<QueryBuilder>,
) -> impl IntoElement {
    let this = cx.entity().downgrade();
    let pick = Rc::new(pick);
    Button::new(id)
        .xsmall()
        .outline()
        .w(px(width))
        .label(label)
        .dropdown_caret(true)
        .dropdown_menu(move |menu: PopupMenu, _, _| {
            items
                .iter()
                .enumerate()
                .fold(menu.scrollable(true), |menu, (ix, item)| {
                    let (this, pick) = (this.clone(), pick.clone());
                    menu.item(
                        PopupMenuItem::new(item.clone()).on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| pick(this, ix, window, cx)).ok();
                        }),
                    )
                })
        })
}

impl QueryBuilder {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        let hover = cx.theme().list_hover;
        let items: Vec<AnyElement> = self
            .tables
            .iter()
            .filter(|t| filter.is_empty() || t.to_lowercase().contains(&filter))
            .enumerate()
            .map(|(i, table)| {
                let name = table.clone();
                div()
                    .id(("builder-table", i))
                    .px_2()
                    .py_1()
                    .text_sm()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(table.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.add_table(name.clone(), cx)))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w(px(210.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_2()
            .gap_2()
            .child(Input::new(&self.filter).small())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.loading {
                        "Loading…"
                    } else {
                        "Click a table to add it"
                    }),
            )
            .child(
                v_flex()
                    .id("builder-table-list")
                    .flex_1()
                    .overflow_y_scrollbar()
                    .children(items),
            )
            .into_any_element()
    }

    fn render_boxes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ui = &self.ui;
        let theme = cx.theme();
        let (border, header, muted, bg, hover) = (
            theme.border,
            theme.secondary,
            theme.muted_foreground,
            theme.background,
            theme.list_hover,
        );
        let mut out = Vec::new();
        for (b, table) in ui.part.tables().into_iter().enumerate() {
            let Some(&(x, y)) = ui.boxes.get(&table.alias) else {
                continue;
            };
            let alias = table.alias.clone();
            let columns: Columns = std::iter::once(("*".to_owned(), String::new()))
                .chain(self.columns.get(&table.table).cloned().unwrap_or_default())
                .collect();
            let rows: Vec<AnyElement> = columns
                .into_iter()
                .enumerate()
                .map(|(c, (name, ty))| {
                    let column = ColumnRef::new(&alias, &name);
                    let checked = ui.rows.iter().any(|r| r.column == column && r.visible);
                    let (down, up, tick) = (column.clone(), column.clone(), column);
                    let star = name == "*";
                    h_flex()
                        .id(SharedString::from(format!("box-{alias}-{c}")))
                        .h(px(ROW))
                        .px_2()
                        .gap_2()
                        .hover(move |s| s.bg(hover))
                        .child(
                            Checkbox::new(SharedString::from(format!("tick-{alias}-{c}")))
                                .xsmall()
                                .checked(checked)
                                .on_change(cx.listener(move |this, _: &bool, window, cx| {
                                    this.toggle_column(tick.clone(), window, cx);
                                })),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .h_full()
                                .justify_between()
                                .gap_2()
                                .when(!star, |el| el.cursor(gpui_kit::CursorStyle::Crosshair))
                                .child(div().text_sm().child(if star {
                                    "* (all columns)".to_owned()
                                } else {
                                    name
                                }))
                                .child(div().text_xs().text_color(muted).child(ty))
                                // Drag from a column to another table's column to join on them.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                        if down.column != "*" {
                                            this.drag = Some(Drag::Link {
                                                from: down.clone(),
                                                to: event.position,
                                            });
                                        }
                                        cx.stop_propagation();
                                    }),
                                ),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if let Some(Drag::Link { from, .. }) = this.drag.take() {
                                    this.link(&from, &up, cx);
                                }
                            }),
                        )
                        .into_any_element()
                })
                .collect();
            let width = self.box_width(&table.table);
            let height = self.box_height(&table.table);
            let (close, moving) = (alias.clone(), alias.clone());
            out.push(
                v_flex()
                    .id(("builder-box", b))
                    .absolute()
                    .left(self.offset.x + px(x))
                    .top(self.offset.y + px(y))
                    .w(px(width))
                    .h(px(height))
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .shadow_sm()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .h(px(HEADER))
                            .pl_2()
                            .bg(header)
                            .border_b_1()
                            .border_color(border)
                            .cursor_move()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.drag = Some(Drag::Box {
                                        alias: moving.clone(),
                                        mouse: event.position,
                                        at: (x, y),
                                    });
                                    cx.stop_propagation();
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(format!("{} ({})", table.table, table.alias)),
                            )
                            .child(
                                Button::new(("close-box", b))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip("Remove table")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_table(&close, cx);
                                    })),
                            ),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }
        out
    }

    /// A marker on each join's lines, showing its kind; clicking it changes or removes it.
    fn render_markers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ui = &self.ui;
        let kinds: Vec<JoinKind> = JoinKind::available(self.dialect)
            .iter()
            .copied()
            .filter(|k| *k != JoinKind::Cross)
            .collect();
        let mut out = Vec::new();
        for (j, join) in ui.part.joins.iter().enumerate() {
            for (p, (a, b)) in join.on.iter().enumerate() {
                let (Some(pa), Some(pb)) = (self.anchor_toward(a, b), self.anchor_toward(b, a))
                else {
                    continue;
                };
                let mid = (f32::midpoint(pa.0, pb.0), f32::midpoint(pa.1, pb.1));
                let mut items: Vec<String> = kinds.iter().map(|k| k.keyword().to_owned()).collect();
                items.push("Remove this link".to_owned());
                let label = join
                    .kind
                    .keyword()
                    .split_whitespace()
                    .next()
                    .unwrap_or("JOIN")
                    .to_owned();
                let kinds = kinds.clone();
                out.push(
                    div()
                        .absolute()
                        .left(self.offset.x + px(mid.0 - 34.))
                        .top(self.offset.y + px(mid.1 - 11.))
                        .child(picker(
                            ("join-marker", j * 100 + p),
                            label,
                            68.,
                            items,
                            move |this, k, _, cx| {
                                if let Some(join) = this.part().part.joins.get_mut(j) {
                                    match kinds.get(k) {
                                        Some(kind) => join.kind = *kind,
                                        None if p < join.on.len() => {
                                            join.on.remove(p);
                                        }
                                        None => {}
                                    }
                                }
                                cx.notify();
                            },
                            cx,
                        ))
                        .into_any_element(),
                );
            }
        }
        out
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let head = |text: &'static str, width: Option<f32>| {
            let cell = div().text_xs().text_color(muted).child(text);
            match width {
                Some(w) => cell.w(px(w)),
                None => cell.flex_1(),
            }
        };
        let mut sorted: Vec<(usize, usize)> = self
            .ui
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.sort.is_some())
            .map(|(i, r)| (r.sorted_at, i))
            .collect();
        sorted.sort_unstable();
        let rank = |i: usize| sorted.iter().position(|&(_, r)| r == i).map(|p| p + 1);
        let aggregates: Vec<String> = std::iter::once("—".to_owned())
            .chain(Aggregate::ALL.iter().map(|a| a.label().to_owned()))
            .collect();
        let rows: Vec<AnyElement> = self
            .ui
            .rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                let id = r;
                h_flex()
                    .gap_2()
                    .h(px(30.))
                    .child(
                        Button::new(("grid-remove", id))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Remove from the grid")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if r < this.part().rows.len() {
                                    this.part().rows.remove(r);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div().w(px(54.)).child(
                            Checkbox::new(("grid-visible", id))
                                .xsmall()
                                .checked(row.visible)
                                .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                    if let Some(row) = this.part().rows.get_mut(r) {
                                        row.visible = *value;
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(div().w(px(180.)).text_sm().child(row.column.label()))
                    .child(div().w(px(130.)).child(Input::new(&row.alias).xsmall()))
                    .child(picker(
                        ("grid-sort", id),
                        match row.sort {
                            None => "—",
                            Some(false) => "Ascending",
                            Some(true) => "Descending",
                        },
                        110.,
                        vec!["—".into(), "Ascending".into(), "Descending".into()],
                        move |this, k, _, cx| {
                            this.sort_counter += 1;
                            let counter = this.sort_counter;
                            if let Some(row) = this.part().rows.get_mut(r) {
                                let was = row.sort;
                                row.sort = match k {
                                    1 => Some(false),
                                    2 => Some(true),
                                    _ => None,
                                };
                                if was.is_none() && row.sort.is_some() {
                                    row.sorted_at = counter;
                                }
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .w(px(70.))
                            .text_sm()
                            .text_center()
                            .child(rank(r).map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(picker(
                        ("grid-aggregate", id),
                        row.aggregate.map_or("—", Aggregate::label),
                        120.,
                        aggregates.clone(),
                        move |this, k, _, cx| {
                            if let Some(row) = this.part().rows.get_mut(r) {
                                row.aggregate = k.checked_sub(1).map(|k| Aggregate::ALL[k]);
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        div().w(px(64.)).flex().justify_center().child(
                            Checkbox::new(("grid-group", id))
                                .xsmall()
                                .checked(row.group)
                                .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                    if let Some(row) = this.part().rows.get_mut(r) {
                                        row.group = *value;
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(div().flex_1().child(Input::new(&row.criteria).xsmall()))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        // The column headings stay put while the rows scroll.
        v_flex()
            .h(px(200.))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_2()
                    .child(div().w(px(24.)))
                    .child(head("Output", Some(54.)))
                    .child(head("Expression", Some(180.)))
                    .child(head("Alias", Some(130.)))
                    .child(head("Sort type", Some(110.)))
                    .child(head("Sort order", Some(70.)))
                    .child(head("Aggregate", Some(120.)))
                    .child(head("Grouping", Some(64.)))
                    .child(head("Criteria", None)),
            )
            .child(
                v_flex()
                    .id("builder-grid")
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .pt_2()
                                .child("Tick columns in the tables above to add them here."),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let distinct = self.ui.part.distinct;
        h_flex()
            .h(px(44.))
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1())
            .child(
                Checkbox::new("distinct")
                    .label("Distinct")
                    .checked(distinct)
                    .on_change(cx.listener(|this, value: &bool, _, cx| {
                        this.part().part.distinct = *value;
                        cx.notify();
                    })),
            )
            .child(div().text_sm().child("Limit"))
            .child(div().w(px(70.)).child(Input::new(&self.limit).xsmall()))
            .child(div().text_sm().child("Offset"))
            .child(
                div()
                    .w(px(60.))
                    .child(Input::new(&self.offset_rows).xsmall()),
            )
            .child(
                Button::new("builder-open")
                    .small()
                    .label("Open in editor")
                    .on_click(cx.listener(|this, _, _, cx| this.finish(false, cx))),
            )
            .child(
                Button::new("builder-run")
                    .small()
                    .primary()
                    .icon(IconName::Play)
                    .label("Run")
                    .on_click(cx.listener(|this, _, _, cx| this.finish(true, cx))),
            )
            .into_any_element()
    }
}

impl Render for QueryBuilder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (line, muted, bg) = (
            theme.muted_foreground,
            theme.muted_foreground,
            theme.background,
        );
        // Every join line, plus the one being drawn.
        let mut links: Vec<((f32, f32), (f32, f32))> = Vec::new();
        for join in &self.ui.part.joins {
            for (a, b) in &join.on {
                if let (Some(pa), Some(pb)) = (self.anchor_toward(a, b), self.anchor_toward(b, a)) {
                    links.push((pa, pb));
                }
            }
        }
        if let Some(Drag::Link { from, to }) = &self.drag {
            let origin = self.canvas.get().map_or(Point::default(), |b| b.origin);
            let target = *to - origin - self.offset;
            let target = (f32::from(target.x), f32::from(target.y));
            if let Some(start) = self.anchor(from, target.0) {
                links.push((start, target));
            }
        }
        let (offset, bounds) = (self.offset, self.canvas.clone());
        let connectors = canvas(
            move |b, _, _| bounds.set(Some(b)),
            move |b, (), window, _| {
                let at = |p: (f32, f32)| {
                    point(
                        b.origin.x + offset.x + px(p.0),
                        b.origin.y + offset.y + px(p.1),
                    )
                };
                for (from, to) in &links {
                    let (start, end) = (at(*from), at(*to));
                    let reach = (f32::from(end.x - start.x).abs() / 2.).max(30.);
                    let (sb, eb) = if start.x < end.x {
                        (reach, -reach)
                    } else {
                        (-reach, reach)
                    };
                    let mut path = PathBuilder::stroke(px(1.5));
                    path.move_to(start);
                    path.cubic_bezier_to(
                        end,
                        point(start.x + px(sb), start.y),
                        point(end.x + px(eb), end.y),
                    );
                    if let Ok(path) = path.build() {
                        window.paint_path(path, line);
                    }
                }
            },
        )
        .absolute()
        .size_full();
        let empty = self.ui.part.from.is_none();
        let sidebar = self.render_sidebar(cx);
        let boxes = self.render_boxes(cx);
        let markers = self.render_markers(cx);
        let grid = self.render_grid(cx);
        let toolbar = self.render_toolbar(cx);
        let preview = match self.sql(cx) {
            Ok(sql) => div().child(sql),
            Err(err) => div().text_color(muted).child(err),
        };
        v_flex()
            .size_full()
            .bg(bg)
            .text_color(cx.theme().foreground)
            .child(toolbar)
            .child(
                h_flex().flex_1().min_h_0().child(sidebar).child(
                    div()
                        .id("builder-canvas")
                        .relative()
                        .flex_1()
                        .h_full()
                        .overflow_hidden()
                        .bg(cx.theme().muted.opacity(0.4))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, _| {
                                this.drag = Some(Drag::Pan {
                                    mouse: event.position,
                                    offset: this.offset,
                                });
                            }),
                        )
                        .on_mouse_move(cx.listener(Self::on_mouse_move))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.drag = None;
                                cx.notify();
                            }),
                        )
                        .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                            this.offset += event.delta.pixel_delta(px(ROW));
                            cx.notify();
                        }))
                        .child(connectors)
                        .children(boxes)
                        .children(markers)
                        .when(empty, |el| {
                            el.child(
                                div()
                                    .absolute()
                                    .top(px(24.))
                                    .left(px(24.))
                                    .text_sm()
                                    .text_color(muted)
                                    .child(
                                        "Add tables from the list on the left. Tick columns \
                                             to select them; drag from a column to another \
                                             table's column to join on it.",
                                    ),
                            )
                        }),
                ),
            )
            .child(grid)
            .children(self.error.clone().map(|err| {
                div()
                    .px_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(err)
            }))
            .child(
                div()
                    .id("builder-sql")
                    .h(px(150.))
                    .p_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .overflow_y_scrollbar()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .child(preview),
            )
    }
}

/// Opens the builder for `db` in a window of its own, starting with `table` when given.
pub fn open(
    dialect: Dialect,
    db: String,
    table: Option<String>,
    session: SharedSession,
    open_sql: OpenSql,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let (s, d) = (session.clone(), db.clone());
            let listed = runtime::run(async move {
                let mut s = s.lock().await;
                let mut names: Vec<String> = Vec::new();
                for kind in [ObjectKind::Table, ObjectKind::View] {
                    names.extend(s.list_objects(&d, kind).await?.into_iter().map(|o| o.name));
                }
                Ok::<_, ovs_core::Error>(names)
            })
            .await;
            cx.update(|window, cx| match listed {
                Ok(tables) => {
                    let options = crate::window_options(format!("Query builder: {db}"), cx);
                    let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                        let builder = cx.new(|cx| {
                            QueryBuilder::new(dialect, db, session, tables, open_sql, window, cx)
                        });
                        if let Some(table) = table {
                            builder.update(cx, |b, cx| b.add_table(table, cx));
                        }
                        builder
                    });
                    if let Err(err) = opened {
                        window.push_notification(format!("Could not open the builder: {err}"), cx);
                    }
                }
                Err(err) => {
                    window.push_notification(format!("Could not list the tables: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
}
