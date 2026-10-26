//! The query builder dialog: pick tables (joined on their foreign keys), tick columns, add
//! conditions and sorting, and combine several `SELECT`s with `UNION` and friends. The SQL is
//! previewed live and opened in a query tab.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::sql::Dialect;
use ovs_core::sql::builder::{
    Aggregate, ColumnRef, Condition, JoinKind, Operator, OrderItem, Query, SelectItem, SelectPart,
    SetOperator,
};
use ovs_core::{ForeignKeyInfo, ObjectKind};

use crate::runtime::{self, SharedSession};

/// Opens the finished SQL in a query tab, running it when the flag is set.
pub type OpenSql = Rc<dyn Fn(String, bool, &mut Window, &mut App)>;

struct ColumnUi {
    column: ColumnRef,
    aggregate: Option<Aggregate>,
    alias: Entity<InputState>,
}

struct ConditionUi {
    column: ColumnRef,
    operator: Operator,
    value: Entity<InputState>,
}

/// One `SELECT` as edited. Tables, joins and sorting live in `part`; columns and conditions,
/// which carry text inputs, are kept here and copied into it when the SQL is built.
struct PartUi {
    operator: SetOperator,
    part: SelectPart,
    columns: Vec<ColumnUi>,
    conditions: Vec<ConditionUi>,
    limit: Entity<InputState>,
}

pub struct QueryBuilder {
    dialect: Dialect,
    db: String,
    session: SharedSession,
    /// Tables and views of `db`, for "Add table".
    tables: Vec<String>,
    columns: HashMap<String, Vec<String>>,
    foreign_keys: HashMap<String, Vec<ForeignKeyInfo>>,
    parts: Vec<PartUi>,
    active: usize,
    /// Sorting and limit of a combined query: output column position, descending.
    union_order: Vec<(usize, bool)>,
    union_limit: Entity<InputState>,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let union_limit = Self::number_input(window, cx);
        let mut builder = Self {
            dialect,
            db,
            session,
            tables,
            columns: HashMap::new(),
            foreign_keys: HashMap::new(),
            parts: Vec::new(),
            active: 0,
            union_order: Vec::new(),
            union_limit: union_limit.clone(),
            loading: false,
            error: None,
            subscriptions: Vec::new(),
        };
        builder.watch(&union_limit, cx);
        builder.add_part(SetOperator::Union, window, cx);
        builder
    }

    fn number_input(window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        cx.new(|cx| InputState::new(window, cx).placeholder("none"))
    }

    fn watch(&mut self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        self.subscriptions
            .push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
    }

    fn add_part(&mut self, operator: SetOperator, window: &mut Window, cx: &mut Context<Self>) {
        let limit = Self::number_input(window, cx);
        self.watch(&limit, cx);
        self.parts.push(PartUi {
            operator,
            part: SelectPart::default(),
            columns: Vec::new(),
            conditions: Vec::new(),
            limit,
        });
        self.active = self.parts.len() - 1;
        cx.notify();
    }

    fn part(&mut self) -> &mut PartUi {
        &mut self.parts[self.active]
    }

    /// Loads `table`'s columns and foreign keys if needed, then adds it to the active part.
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
                        this.columns
                            .insert(table.clone(), columns.into_iter().map(|c| c.name).collect());
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
        self.part()
            .part
            .add_table(table, &|t| fks.get(t).cloned().unwrap_or_default());
        self.error = None;
        cx.notify();
    }

    fn remove_table(&mut self, alias: &str, cx: &mut Context<Self>) {
        let part = self.part();
        part.part.remove_table(alias);
        part.columns.retain(|c| c.column.alias != alias);
        part.conditions.retain(|c| c.column.alias != alias);
        cx.notify();
    }

    /// Every column of the active part's tables, before the join at `limit` (all when `None`).
    fn columns_of(&self, ix: usize, before_join: Option<usize>) -> Vec<ColumnRef> {
        let part = &self.parts[ix].part;
        let tables = part.tables();
        let count = before_join.map_or(tables.len(), |j| j + 1);
        tables
            .into_iter()
            .take(count)
            .flat_map(|t| {
                self.columns
                    .get(&t.table)
                    .into_iter()
                    .flatten()
                    .map(|c| ColumnRef::new(&t.alias, c))
            })
            .collect()
    }

    fn toggle_column(&mut self, column: ColumnRef, window: &mut Window, cx: &mut Context<Self>) {
        let part = &mut self.parts[self.active];
        if let Some(at) = part.columns.iter().position(|c| c.column == column) {
            part.columns.remove(at);
        } else {
            let alias = cx.new(|cx| InputState::new(window, cx).placeholder("name"));
            self.watch(&alias, cx);
            self.parts[self.active].columns.push(ColumnUi {
                column,
                aggregate: None,
                alias,
            });
        }
        cx.notify();
    }

    fn add_condition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.columns_of(self.active, None).into_iter().next() else {
            return;
        };
        let value =
            cx.new(|cx| InputState::new(window, cx).placeholder("value: 42, text, or =SQL"));
        self.watch(&value, cx);
        self.part().conditions.push(ConditionUi {
            column,
            operator: Operator::Eq,
            value,
        });
        cx.notify();
    }

    /// The model with every input's current text.
    fn query(&self, cx: &App) -> Query {
        let limit = |input: &Entity<InputState>| input.read(cx).value().trim().parse().ok();
        let build = |ui: &PartUi| {
            let mut part = ui.part.clone();
            part.columns = ui
                .columns
                .iter()
                .map(|c| SelectItem {
                    column: c.column.clone(),
                    aggregate: c.aggregate,
                    alias: c.alias.read(cx).value().trim().to_owned(),
                })
                .collect();
            part.conditions = ui
                .conditions
                .iter()
                .map(|c| Condition {
                    column: c.column.clone(),
                    operator: c.operator,
                    value: c.value.read(cx).value().to_string(),
                })
                .collect();
            part.limit = limit(&ui.limit);
            part
        };
        Query {
            first: build(&self.parts[0]),
            rest: self.parts[1..]
                .iter()
                .map(|ui| (ui.operator, build(ui)))
                .collect(),
            order_by: self.union_order.clone(),
            limit: if self.parts.len() > 1 {
                limit(&self.union_limit)
            } else {
                None
            },
        }
    }

    pub fn sql(&self, cx: &App) -> Result<String, String> {
        self.query(cx).sql(self.dialect, &self.db)
    }
}

/// A small dropdown button showing `label`, listing `items`; picking one calls `pick` with its
/// index.
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

fn remove_button(
    id: impl Into<gpui_kit::ElementId>,
    tooltip: &'static str,
    on: impl Fn(&mut QueryBuilder, &mut Context<QueryBuilder>) + 'static,
    cx: &mut Context<QueryBuilder>,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(IconName::Close)
        .tooltip(tooltip)
        .on_click(cx.listener(move |this, _, _, cx| on(this, cx)))
}

fn section(title: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(title)
}

impl QueryBuilder {
    fn render_tables(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.active;
        let part = self.parts[ix].part.clone();
        let mut list = v_flex().gap_2().child(section("TABLES", cx));
        if let Some(from) = &part.from {
            let alias = from.alias.clone();
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child(format!("FROM {} ({})", from.table, from.alias)),
                    )
                    .child(div().flex_1())
                    .child(remove_button(
                        ("remove-from", ix),
                        "Remove table",
                        move |this, cx| this.remove_table(&alias, cx),
                        cx,
                    )),
            );
        }
        let kinds = JoinKind::available(self.dialect);
        for (j, join) in part.joins.iter().enumerate() {
            let alias = join.table.alias.clone();
            let mut block = v_flex()
                .gap_1()
                .pl_2()
                .border_l_2()
                .border_color(cx.theme().border);
            block = block.child(
                h_flex()
                    .gap_1()
                    .child(picker(
                        ("join-kind", ix * 1000 + j),
                        join.kind.keyword(),
                        110.,
                        kinds.iter().map(|k| k.keyword().to_owned()).collect(),
                        move |this, k, _, cx| {
                            let kind = JoinKind::available(this.dialect)[k];
                            if let Some(join) = this.part().part.joins.get_mut(j) {
                                join.kind = kind;
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} ({})", join.table.table, join.table.alias)),
                    )
                    .child(div().flex_1())
                    .child(remove_button(
                        ("remove-join", ix * 1000 + j),
                        "Remove table",
                        move |this, cx| this.remove_table(&alias, cx),
                        cx,
                    )),
            );
            if join.kind != JoinKind::Cross {
                let earlier: Vec<ColumnRef> = self.columns_of(ix, Some(j));
                let own: Vec<ColumnRef> = self
                    .columns
                    .get(&join.table.table)
                    .into_iter()
                    .flatten()
                    .map(|c| ColumnRef::new(&join.table.alias, c))
                    .collect();
                for (p, (left, right)) in join.on.iter().enumerate() {
                    let (earlier, own) = (earlier.clone(), own.clone());
                    let id = ix * 100_000 + j * 100 + p;
                    block = block.child(
                        h_flex()
                            .gap_1()
                            .child(div().text_xs().w(px(22.)).child(if p == 0 {
                                "ON"
                            } else {
                                "AND"
                            }))
                            .child(picker(
                                ("on-left", id),
                                left.label(),
                                130.,
                                earlier.iter().map(ColumnRef::label).collect(),
                                move |this, k, _, cx| {
                                    if let Some(pair) = this
                                        .part()
                                        .part
                                        .joins
                                        .get_mut(j)
                                        .and_then(|j| j.on.get_mut(p))
                                    {
                                        pair.0 = earlier[k].clone();
                                    }
                                    cx.notify();
                                },
                                cx,
                            ))
                            .child(div().text_xs().child("="))
                            .child(picker(
                                ("on-right", id),
                                right.label(),
                                130.,
                                own.iter().map(ColumnRef::label).collect(),
                                move |this, k, _, cx| {
                                    if let Some(pair) = this
                                        .part()
                                        .part
                                        .joins
                                        .get_mut(j)
                                        .and_then(|j| j.on.get_mut(p))
                                    {
                                        pair.1 = own[k].clone();
                                    }
                                    cx.notify();
                                },
                                cx,
                            ))
                            .child(remove_button(
                                ("remove-on", id),
                                "Remove this match",
                                move |this, cx| {
                                    if let Some(join) = this.part().part.joins.get_mut(j)
                                        && p < join.on.len()
                                    {
                                        join.on.remove(p);
                                    }
                                    cx.notify();
                                },
                                cx,
                            )),
                    );
                }
                let (first_left, first_right) = (earlier.first().cloned(), own.first().cloned());
                block = block.child(
                    h_flex().child(
                        Button::new(("add-on", ix * 1000 + j))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label(if join.on.is_empty() {
                                "Match columns"
                            } else {
                                "Match another column"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let (Some(l), Some(r)) =
                                    (first_left.clone(), first_right.clone())
                                    && let Some(join) = this.part().part.joins.get_mut(j)
                                {
                                    join.on.push((l, r));
                                }
                                cx.notify();
                            })),
                    ),
                );
            }
            list = list.child(block);
        }
        let tables = self.tables.clone();
        list.child(
            h_flex().child(
                Button::new(("add-table", ix))
                    .xsmall()
                    .icon(IconName::Plus)
                    .label(if part.from.is_some() {
                        "Join a table"
                    } else {
                        "Add a table"
                    })
                    .loading(self.loading)
                    .dropdown_menu({
                        let this = cx.entity().downgrade();
                        move |menu: PopupMenu, _, _| {
                            tables.iter().fold(menu.scrollable(true), |menu, table| {
                                let (this, table) = (this.clone(), table.clone());
                                menu.item(PopupMenuItem::new(table.clone()).on_click(
                                    move |_, _, cx| {
                                        this.update(cx, |this, cx| {
                                            this.add_table(table.clone(), cx);
                                        })
                                        .ok();
                                    },
                                ))
                            })
                        }
                    }),
            ),
        )
        .into_any_element()
    }

    fn render_columns(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.active;
        let part = self.parts[ix].part.clone();
        let mut list = v_flex().gap_2().child(
            h_flex().gap_3().child(section("COLUMNS", cx)).child(
                Checkbox::new(("distinct", ix))
                    .label("Distinct rows")
                    .checked(part.distinct)
                    .on_change(cx.listener(|this, value: &bool, _, cx| {
                        this.part().part.distinct = *value;
                        cx.notify();
                    })),
            ),
        );
        for table in part.tables() {
            let names = self.columns.get(&table.table).cloned().unwrap_or_default();
            let boxes = names.into_iter().enumerate().map(|(c, name)| {
                let column = ColumnRef::new(&table.alias, &name);
                let checked = self.parts[ix].columns.iter().any(|x| x.column == column);
                Checkbox::new(SharedString::from(format!("col-{ix}-{}-{c}", table.alias)))
                    .label(name)
                    .checked(checked)
                    .on_change(cx.listener(move |this, _: &bool, window, cx| {
                        this.toggle_column(column.clone(), window, cx);
                    }))
            });
            list = list.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} ({})", table.table, table.alias)),
                    )
                    .child(h_flex().flex_wrap().gap_x_3().gap_y_1().children(boxes)),
            );
        }
        if part.from.is_none() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Add a table on the left to choose its columns."),
            );
        } else if self.parts[ix].columns.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No columns ticked: every column is selected (*)."),
            );
        }
        let aggregates: Vec<String> = std::iter::once("—".to_owned())
            .chain(Aggregate::ALL.iter().map(|a| a.label().to_owned()))
            .collect();
        for (c, column) in self.parts[ix].columns.iter().enumerate() {
            let aggregates = aggregates.clone();
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(div().text_sm().w(px(160.)).child(column.column.label()))
                    .child(picker(
                        ("aggregate", ix * 1000 + c),
                        column.aggregate.map_or("—", Aggregate::label),
                        120.,
                        aggregates,
                        move |this, k, _, cx| {
                            if let Some(col) = this.part().columns.get_mut(c) {
                                col.aggregate = k.checked_sub(1).map(|k| Aggregate::ALL[k]);
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(div().text_xs().child("AS"))
                    .child(div().w(px(140.)).child(Input::new(&column.alias).xsmall())),
            );
        }
        list.into_any_element()
    }

    fn render_filters(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.active;
        let columns = self.columns_of(ix, None);
        let labels: Vec<String> = columns.iter().map(ColumnRef::label).collect();
        let mut list = v_flex()
            .gap_1()
            .child(section("WHERE (all must match)", cx));
        for (c, cond) in self.parts[ix].conditions.iter().enumerate() {
            let columns = columns.clone();
            let id = ix * 1000 + c;
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(picker(
                        ("cond-col", id),
                        cond.column.label(),
                        160.,
                        labels.clone(),
                        move |this, k, _, cx| {
                            if let Some(cond) = this.part().conditions.get_mut(c) {
                                cond.column = columns[k].clone();
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(picker(
                        ("cond-op", id),
                        cond.operator.label(),
                        100.,
                        Operator::ALL.iter().map(|o| o.label().to_owned()).collect(),
                        move |this, k, _, cx| {
                            if let Some(cond) = this.part().conditions.get_mut(c) {
                                cond.operator = Operator::ALL[k];
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(div().flex_1().when(cond.operator.takes_value(), |d| {
                        d.child(Input::new(&cond.value).xsmall())
                    }))
                    .child(remove_button(
                        ("remove-cond", id),
                        "Remove condition",
                        move |this, cx| {
                            if c < this.part().conditions.len() {
                                this.part().conditions.remove(c);
                            }
                            cx.notify();
                        },
                        cx,
                    )),
            );
        }
        list = list.child(
            h_flex().child(
                Button::new(("add-cond", ix))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .label("Condition")
                    .disabled(columns.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.add_condition(window, cx))),
            ),
        );
        // A combined query is sorted as a whole, below.
        if self.parts.len() == 1 {
            list = list
                .child(div().h(px(6.)))
                .child(section("SORT AND LIMIT", cx));
            for (o, order) in self.parts[ix].part.order_by.iter().enumerate() {
                let columns = columns.clone();
                let id = ix * 1000 + o;
                list = list.child(
                    h_flex()
                        .gap_1()
                        .child(picker(
                            ("order-col", id),
                            order.column.label(),
                            160.,
                            labels.clone(),
                            move |this, k, _, cx| {
                                if let Some(order) = this.part().part.order_by.get_mut(o) {
                                    order.column = columns[k].clone();
                                }
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(
                            Button::new(("order-dir", id))
                                .xsmall()
                                .outline()
                                .label(if order.descending {
                                    "Descending"
                                } else {
                                    "Ascending"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(order) = this.part().part.order_by.get_mut(o) {
                                        order.descending = !order.descending;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(remove_button(
                            ("remove-order", id),
                            "Remove sort",
                            move |this, cx| {
                                if o < this.part().part.order_by.len() {
                                    this.part().part.order_by.remove(o);
                                }
                                cx.notify();
                            },
                            cx,
                        )),
                );
            }
            let first = columns.first().cloned();
            list = list.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(("add-order", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("Sort")
                            .disabled(first.is_none())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(column) = first.clone() {
                                    this.part().part.order_by.push(OrderItem {
                                        column,
                                        descending: false,
                                    });
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().text_xs().child("Limit"))
                    .child(
                        div()
                            .w(px(90.))
                            .child(Input::new(&self.parts[ix].limit).xsmall()),
                    ),
            );
        }
        list.into_any_element()
    }

    /// Sorting and limit for a combined query, by output column position.
    fn render_union_order(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let width = self.parts[0].columns.len().max(1);
        let positions: Vec<String> = (1..=width).map(|p| format!("column {p}")).collect();
        let mut list = v_flex()
            .gap_1()
            .child(section("SORT AND LIMIT THE COMBINED RESULT", cx));
        for (o, (position, descending)) in self.union_order.iter().enumerate() {
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(picker(
                        ("union-order", o),
                        format!("column {position}"),
                        110.,
                        positions.clone(),
                        move |this, k, _, cx| {
                            if let Some(order) = this.union_order.get_mut(o) {
                                order.0 = k + 1;
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        Button::new(("union-dir", o))
                            .xsmall()
                            .outline()
                            .label(if *descending {
                                "Descending"
                            } else {
                                "Ascending"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(order) = this.union_order.get_mut(o) {
                                    order.1 = !order.1;
                                }
                                cx.notify();
                            })),
                    )
                    .child(remove_button(
                        ("remove-union-order", o),
                        "Remove sort",
                        move |this, cx| {
                            if o < this.union_order.len() {
                                this.union_order.remove(o);
                            }
                            cx.notify();
                        },
                        cx,
                    )),
            );
        }
        list.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("add-union-order")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("Sort")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.union_order.push((1, false));
                            cx.notify();
                        })),
                )
                .child(div().text_xs().child("Limit"))
                .child(
                    div()
                        .w(px(90.))
                        .child(Input::new(&self.union_limit).xsmall()),
                ),
        )
        .into_any_element()
    }
}

impl Render for QueryBuilder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let tabs: Vec<Tab> = self
            .parts
            .iter()
            .enumerate()
            .map(|(i, ui)| {
                Tab::new().label(if i == 0 {
                    "Query 1".to_owned()
                } else {
                    format!("{} · Query {}", ui.operator.keyword(), i + 1)
                })
            })
            .collect();
        let preview = match self.sql(cx) {
            Ok(sql) => div().child(sql),
            Err(err) => div().text_color(muted).child(err),
        };
        let active = self.active;
        let combine = (active > 0).then(|| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .child("Combined with the queries before it by"),
                )
                .child(picker(
                    ("set-op", active),
                    self.parts[active].operator.keyword(),
                    120.,
                    SetOperator::ALL
                        .iter()
                        .map(|o| o.keyword().to_owned())
                        .collect(),
                    move |this, k, _, cx| {
                        this.part().operator = SetOperator::ALL[k];
                        cx.notify();
                    },
                    cx,
                ))
                .child(div().flex_1())
                .child(
                    Button::new(("remove-part", active))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .label("Remove this query")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if active > 0 && active < this.parts.len() {
                                this.parts.remove(active);
                                this.active = active - 1;
                            }
                            cx.notify();
                        })),
                )
        });
        let tables = self.render_tables(cx);
        let columns = self.render_columns(cx);
        let filters = self.render_filters(cx);
        let union_order = (self.parts.len() > 1).then(|| self.render_union_order(cx));
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        TabBar::new("builder-parts")
                            .underline()
                            .small()
                            .selected_index(self.active)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.active = *ix;
                                cx.notify();
                            }))
                            .children(tabs),
                    )
                    .child(
                        Button::new("add-part")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("UNION another query")
                            .tooltip("Combine with another SELECT (UNION, INTERSECT, EXCEPT)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_part(SetOperator::Union, window, cx);
                            })),
                    ),
            )
            .children(combine)
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .h(px(400.))
                    .child(
                        div()
                            .id("builder-tables")
                            .w(px(400.))
                            .h_full()
                            .overflow_y_scroll()
                            .child(tables),
                    )
                    .child(
                        v_flex()
                            .id("builder-right")
                            .flex_1()
                            .h_full()
                            .gap_4()
                            .overflow_y_scroll()
                            .child(columns)
                            .child(filters)
                            .children(union_order),
                    ),
            )
            .children(
                self.error
                    .clone()
                    .map(|err| div().text_sm().text_color(cx.theme().danger).child(err)),
            )
            .child(
                div()
                    .id("builder-preview")
                    .p_2()
                    .h(px(150.))
                    .overflow_y_scroll()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family("monospace")
                    .text_xs()
                    .child(preview),
            )
    }
}

/// Opens the builder for `db`, starting with `table` when given.
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
                Ok(tables) => show(dialect, db, table, tables, session, open_sql, window, cx),
                Err(err) => {
                    window.push_notification(format!("Could not list the tables: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
}

#[allow(clippy::too_many_arguments)]
fn show(
    dialect: Dialect,
    db: String,
    table: Option<String>,
    tables: Vec<String>,
    session: SharedSession,
    open_sql: OpenSql,
    window: &mut Window,
    cx: &mut App,
) {
    let title = format!("Query builder: {db}");
    let builder = cx.new(|cx| QueryBuilder::new(dialect, db, session, tables, window, cx));
    if let Some(table) = table {
        builder.update(cx, |b, cx| b.add_table(table, cx));
    }
    window.open_dialog(cx, move |dialog, _, _| {
        let finish = |run: bool, builder: Entity<QueryBuilder>, open_sql: OpenSql| {
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                if let Ok(sql) = builder.read(cx).sql(cx) {
                    window.close_dialog(cx);
                    open_sql(sql, run, window, cx);
                }
            }
        };
        dialog
            .title(title.clone())
            .w(px(1120.))
            .child(builder.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("builder-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("builder-open")
                            .label("Open in editor")
                            .on_click(finish(false, builder.clone(), open_sql.clone())),
                    )
                    .child(
                        Button::new("builder-run")
                            .primary()
                            .label("Run")
                            .on_click(finish(true, builder.clone(), open_sql.clone())),
                    ),
            )
    });
}
