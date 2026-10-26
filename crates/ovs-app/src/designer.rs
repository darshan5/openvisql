//! The table designer dialog: edit a table's columns and apply the generated `CREATE TABLE`
//! or `ALTER TABLE`, which is shown as a live preview.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};
use ovs_core::sql::Dialect;
use ovs_core::sql::design::{ColumnDef, TableDesign, alter_sql, create_sql};

use crate::runtime::{self, SharedSession};

/// Runs the SQL the designer produced.
pub type ApplySql = Rc<dyn Fn(String, &mut Window, &mut App)>;

struct Row {
    id: usize,
    name: Entity<InputState>,
    data_type: Entity<InputState>,
    default_sql: Entity<InputState>,
    comment: Entity<InputState>,
    nullable: bool,
    primary_key: bool,
    auto_increment: bool,
    original_name: Option<String>,
}

pub struct TableDesigner {
    db: String,
    /// The table as loaded; `None` when creating a new table.
    original: Option<TableDesign>,
    name: Entity<InputState>,
    rows: Vec<Row>,
    next_id: usize,
    subscriptions: Vec<Subscription>,
}

impl TableDesigner {
    fn new(
        db: String,
        original: Option<TableDesign>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let start = original.clone().unwrap_or_else(|| TableDesign {
            name: "new_table".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: "INT UNSIGNED".into(),
                primary_key: true,
                auto_increment: true,
                ..ColumnDef::default()
            }],
        });
        let name = cx.new(|cx| InputState::new(window, cx).default_value(start.name.clone()));
        let mut designer = Self {
            db,
            original,
            name: name.clone(),
            rows: Vec::new(),
            next_id: 0,
            subscriptions: Vec::new(),
        };
        designer.watch(&name, cx);
        for column in start.columns {
            designer.push_row(column, window, cx);
        }
        designer
    }

    /// Re-renders the SQL preview whenever `input` changes.
    fn watch(&mut self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        self.subscriptions
            .push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
    }

    fn push_row(&mut self, column: ColumnDef, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            |value: String, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                let placeholder = SharedString::from(placeholder.to_owned());
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(value)
                })
            };
        let row = Row {
            id: self.next_id,
            name: input(column.name, "name", window, cx),
            data_type: input(column.data_type, "VARCHAR(100)", window, cx),
            default_sql: input(column.default_sql, "none", window, cx),
            comment: input(column.comment, "", window, cx),
            nullable: column.nullable,
            primary_key: column.primary_key,
            auto_increment: column.auto_increment,
            original_name: column.original_name,
        };
        self.next_id += 1;
        for input in [&row.name, &row.data_type, &row.default_sql, &row.comment] {
            self.watch(&input.clone(), cx);
        }
        self.rows.push(row);
        cx.notify();
    }

    fn design(&self, cx: &App) -> TableDesign {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        TableDesign {
            name: text(&self.name),
            columns: self
                .rows
                .iter()
                .map(|row| ColumnDef {
                    name: text(&row.name),
                    data_type: text(&row.data_type),
                    nullable: row.nullable,
                    default_sql: text(&row.default_sql),
                    auto_increment: row.auto_increment,
                    primary_key: row.primary_key,
                    comment: row.comment.read(cx).value().to_string(),
                    original_name: row.original_name.clone(),
                })
                .collect(),
        }
    }

    /// The statement to run: `Ok(None)` when an existing table is unchanged.
    fn sql(&self, cx: &App) -> Result<Option<String>, String> {
        let design = self.design(cx);
        design.validate()?;
        Ok(match &self.original {
            None => Some(create_sql(Dialect::MySql, &self.db, &design)),
            Some(original) => alter_sql(Dialect::MySql, &self.db, original, &design),
        })
    }

    fn move_row(&mut self, ix: usize, up: bool, cx: &mut Context<Self>) {
        let target = if up { ix.checked_sub(1) } else { Some(ix + 1) };
        if let Some(target) = target.filter(|t| *t < self.rows.len()) {
            self.rows.swap(ix, target);
            cx.notify();
        }
    }

    fn render_row(ix: usize, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let flag =
            |id: &'static str, checked: bool, set: fn(&mut Row, bool), cx: &mut Context<Self>| {
                Checkbox::new((id, row.id))
                    .checked(checked)
                    .on_change(cx.listener(move |this, value: &bool, _, cx| {
                        if let Some(row) = this.rows.get_mut(ix) {
                            set(row, *value);
                        }
                        cx.notify();
                    }))
            };
        h_flex()
            .gap_1()
            .child(div().w(px(150.)).child(Input::new(&row.name).xsmall()))
            .child(div().w(px(150.)).child(Input::new(&row.data_type).xsmall()))
            .child(div().w(px(44.)).flex().justify_center().child(flag(
                "null",
                row.nullable,
                |r, v| r.nullable = v,
                cx,
            )))
            .child(div().w(px(36.)).flex().justify_center().child(flag(
                "pk",
                row.primary_key,
                |r, v| r.primary_key = v,
                cx,
            )))
            .child(div().w(px(36.)).flex().justify_center().child(flag(
                "ai",
                row.auto_increment,
                |r, v| r.auto_increment = v,
                cx,
            )))
            .child(
                div()
                    .w(px(140.))
                    .child(Input::new(&row.default_sql).xsmall()),
            )
            .child(div().flex_1().child(Input::new(&row.comment).xsmall()))
            .child(
                Button::new(("up", row.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .on_click(cx.listener(move |this, _, _, cx| this.move_row(ix, true, cx))),
            )
            .child(
                Button::new(("down", row.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowDown)
                    .on_click(cx.listener(move |this, _, _, cx| this.move_row(ix, false, cx))),
            )
            .child(
                Button::new(("remove", row.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove column")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if ix < this.rows.len() {
                            this.rows.remove(ix);
                        }
                        cx.notify();
                    })),
            )
    }
}

impl Render for TableDesigner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let header = |text: &'static str, width: Option<f32>| {
            let cell = div().text_xs().text_color(muted).child(text);
            match width {
                Some(w) => cell.w(px(w)),
                None => cell.flex_1(),
            }
        };
        let preview = match self.sql(cx) {
            Ok(Some(sql)) => div().child(sql),
            Ok(None) => div().text_color(muted).child("No changes."),
            Err(err) => div().text_color(cx.theme().danger).child(err),
        };
        let rows: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| Self::render_row(ix, row, cx).into_any_element())
            .collect();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child("Table name"))
                    .child(div().w(px(260.)).child(Input::new(&self.name).small())),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(header("Column", Some(150.)))
                    .child(header("Type", Some(150.)))
                    .child(header("NULL", Some(44.)))
                    .child(header("PK", Some(36.)))
                    .child(header("Auto", Some(36.)))
                    .child(header("Default (SQL)", Some(140.)))
                    .child(header("Comment", None))
                    .child(div().w(px(84.))),
            )
            .child(
                v_flex()
                    .id("designer-rows")
                    .gap_1()
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                h_flex().child(
                    Button::new("add-column")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("Column")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.push_row(
                                ColumnDef {
                                    name: format!("column_{}", this.rows.len() + 1),
                                    data_type: "VARCHAR(100)".into(),
                                    nullable: true,
                                    ..ColumnDef::default()
                                },
                                window,
                                cx,
                            );
                        })),
                ),
            )
            .child(
                div()
                    .id("designer-preview")
                    .p_2()
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family("monospace")
                    .text_xs()
                    .child(preview),
            )
    }
}

/// Opens the designer for `table` in `db` (loading it on `session`), or for a new table.
pub fn open(
    db: String,
    table: Option<String>,
    session: SharedSession,
    apply: ApplySql,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let original = match table {
                Some(table) => {
                    let (d, t) = (db.clone(), table.clone());
                    let loaded = runtime::run(async move {
                        let mut s = session.lock().await;
                        let columns = s.list_columns(&d, &t).await?;
                        let indexes = s.list_indexes(&d, &t).await?;
                        Ok::<_, ovs_core::Error>(TableDesign::from_table(&t, &columns, &indexes))
                    })
                    .await;
                    match loaded {
                        Ok(design) => Some(design),
                        Err(err) => {
                            cx.update(|window, cx| {
                                window.push_notification(
                                    format!("Could not load {table}: {err}"),
                                    cx,
                                );
                            })
                            .ok();
                            return;
                        }
                    }
                }
                None => None,
            };
            cx.update(|window, cx| show(db, original, apply, window, cx))
                .ok();
        })
        .detach();
}

fn show(
    db: String,
    original: Option<TableDesign>,
    apply: ApplySql,
    window: &mut Window,
    cx: &mut App,
) {
    let title = match &original {
        Some(t) => format!("Design table {}", t.name),
        None => format!("New table in {db}"),
    };
    let designer = cx.new(|cx| TableDesigner::new(db, original, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let (designer, apply) = (designer.clone(), apply.clone());
        dialog
            .title(title.clone())
            .w(px(980.))
            .child(designer.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("designer-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("designer-apply")
                            .primary()
                            .label("Apply")
                            .on_click(move |_, window, cx| match designer.read(cx).sql(cx) {
                                Ok(Some(sql)) => {
                                    window.close_dialog(cx);
                                    apply(sql, window, cx);
                                }
                                Ok(None) => window.close_dialog(cx),
                                // The preview already shows what's wrong.
                                Err(_) => {}
                            }),
                    ),
            )
    });
}
