//! The table designer dialog: edit a table's columns and apply the generated `CREATE TABLE`
//! or `ALTER TABLE`, which is shown as a live preview.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::sql::Dialect;
use ovs_core::sql::design::{
    CheckDef, ColumnDef, ForeignKeyDef, IndexDef, IndexKind, REFERENTIAL_ACTIONS, TableDesign,
    alter_sql, create_statements,
};
use ovs_core::{ObjectKind, TableOptions};

use crate::runtime::{self, SharedSession};

/// Runs the statements the designer produced, in order.
pub type ApplySql = Rc<dyn Fn(Vec<String>, &mut Window, &mut App)>;

struct Row {
    id: usize,
    name: Entity<InputState>,
    data_type: Entity<InputState>,
    default_sql: Entity<InputState>,
    comment: Entity<InputState>,
    nullable: bool,
    primary_key: bool,
    auto_increment: bool,
    /// Not edited here; kept so a changed column keeps its collation.
    collation: String,
    generated: bool,
    original_name: Option<String>,
}

struct IndexRow {
    id: usize,
    name: Entity<InputState>,
    kind: IndexKind,
    /// Comma-separated column names, e.g. `name(10), id`.
    columns: Entity<InputState>,
    original_name: Option<String>,
}

struct CheckRow {
    id: usize,
    name: Entity<InputState>,
    clause: Entity<InputState>,
    original_name: Option<String>,
}

struct ForeignKeyRow {
    id: usize,
    name: Entity<InputState>,
    columns: Entity<InputState>,
    /// `table` or `db.table`.
    ref_table: Entity<InputState>,
    ref_columns: Entity<InputState>,
    on_delete: String,
    on_update: String,
    original_name: Option<String>,
}

/// Splits a comma-separated list of names.
fn names(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Columns,
    Indexes,
    ForeignKeys,
    Checks,
}

impl Section {
    const ALL: [Self; 4] = [
        Self::Columns,
        Self::Indexes,
        Self::ForeignKeys,
        Self::Checks,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Columns => "Columns",
            Self::Indexes => "Indexes",
            Self::ForeignKeys => "Foreign keys",
            Self::Checks => "Checks",
        }
    }
}

pub struct TableDesigner {
    dialect: Dialect,
    db: String,
    /// The table as loaded; `None` when creating a new table.
    original: Option<TableDesign>,
    name: Entity<InputState>,
    rows: Vec<Row>,
    indexes: Vec<IndexRow>,
    foreign_keys: Vec<ForeignKeyRow>,
    checks: Vec<CheckRow>,
    section: Section,
    engine: Entity<InputState>,
    collation: Entity<InputState>,
    table_comment: Entity<InputState>,
    next_id: usize,
    subscriptions: Vec<Subscription>,
}

impl TableDesigner {
    fn new(
        dialect: Dialect,
        db: String,
        original: Option<TableDesign>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let start = original.clone().unwrap_or_else(|| TableDesign {
            name: "new_table".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: match dialect {
                    Dialect::MySql => "INT UNSIGNED",
                    Dialect::Sqlite => "INTEGER",
                    Dialect::Postgres => "bigint",
                }
                .into(),
                primary_key: true,
                auto_increment: true,
                ..ColumnDef::default()
            }],
            ..TableDesign::default()
        });
        let name = cx.new(|cx| InputState::new(window, cx).default_value(start.name.clone()));
        let option = |value: &str,
                      placeholder: &'static str,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let engine = option(&start.options.engine, "server default", window, cx);
        let collation = option(&start.options.collation, "server default", window, cx);
        let table_comment = option(&start.options.comment, "", window, cx);
        let mut designer = Self {
            dialect,
            db,
            original,
            name: name.clone(),
            rows: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            section: Section::Columns,
            engine: engine.clone(),
            collation: collation.clone(),
            table_comment: table_comment.clone(),
            next_id: 0,
            subscriptions: Vec::new(),
        };
        for input in [&name, &engine, &collation, &table_comment] {
            designer.watch(input, cx);
        }
        for column in start.columns {
            designer.push_row(column, window, cx);
        }
        for index in start.indexes {
            designer.push_index(index, window, cx);
        }
        for fk in start.foreign_keys {
            designer.push_foreign_key(fk, window, cx);
        }
        for check in start.checks {
            designer.push_check(check, window, cx);
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
            collation: column.collation,
            generated: column.generated,
            original_name: column.original_name,
        };
        self.next_id += 1;
        for input in [&row.name, &row.data_type, &row.default_sql, &row.comment] {
            self.watch(&input.clone(), cx);
        }
        self.rows.push(row);
        cx.notify();
    }

    fn push_index(&mut self, index: IndexDef, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("index name")
                .default_value(index.name)
        });
        let columns = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("col_a, col_b(10)")
                .default_value(index.columns.join(", "))
        });
        self.watch(&name, cx);
        self.watch(&columns, cx);
        self.indexes.push(IndexRow {
            id: self.next_id,
            name,
            kind: index.kind,
            columns,
            original_name: index.original_name,
        });
        self.next_id += 1;
        cx.notify();
    }

    fn push_foreign_key(&mut self, fk: ForeignKeyDef, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        let mut input = |value: String, placeholder: &'static str| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            });
            self.watch(&input, cx);
            input
        };
        let ref_table = if fk.ref_database.is_empty() {
            fk.ref_table
        } else {
            format!("{}.{}", fk.ref_database, fk.ref_table)
        };
        let row = ForeignKeyRow {
            id,
            name: input(fk.name, "constraint name"),
            columns: input(fk.columns.join(", "), "col_a, col_b"),
            ref_table: input(ref_table, "table or db.table"),
            ref_columns: input(fk.ref_columns.join(", "), "id"),
            on_delete: fk.on_delete,
            on_update: fk.on_update,
            original_name: fk.original_name,
        };
        self.next_id += 1;
        self.foreign_keys.push(row);
        cx.notify();
    }

    fn push_check(&mut self, check: CheckDef, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("constraint name")
                .default_value(check.name)
        });
        let clause = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("price >= 0")
                .default_value(check.clause)
        });
        self.watch(&name, cx);
        self.watch(&clause, cx);
        self.checks.push(CheckRow {
            id: self.next_id,
            name,
            clause,
            original_name: check.original_name,
        });
        self.next_id += 1;
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
                    collation: row.collation.clone(),
                    generated: row.generated,
                    original_name: row.original_name.clone(),
                })
                .collect(),
            indexes: self
                .indexes
                .iter()
                .map(|index| IndexDef {
                    name: text(&index.name),
                    kind: index.kind,
                    columns: names(&index.columns.read(cx).value()),
                    original_name: index.original_name.clone(),
                })
                .collect(),
            foreign_keys: self
                .foreign_keys
                .iter()
                .map(|fk| {
                    let target = text(&fk.ref_table);
                    let (ref_database, ref_table) = match target.split_once('.') {
                        Some((db, table)) => (db.trim().to_owned(), table.trim().to_owned()),
                        None => (String::new(), target),
                    };
                    ForeignKeyDef {
                        name: text(&fk.name),
                        columns: names(&fk.columns.read(cx).value()),
                        ref_database,
                        ref_table,
                        ref_columns: names(&fk.ref_columns.read(cx).value()),
                        on_update: fk.on_update.clone(),
                        on_delete: fk.on_delete.clone(),
                        original_name: fk.original_name.clone(),
                    }
                })
                .collect(),
            checks: self
                .checks
                .iter()
                .map(|check| CheckDef {
                    name: text(&check.name),
                    clause: text(&check.clause),
                    original_name: check.original_name.clone(),
                })
                .collect(),
            triggers: self
                .original
                .as_ref()
                .map(|o| o.triggers.clone())
                .unwrap_or_default(),
            suffix: self
                .original
                .as_ref()
                .map(|o| o.suffix.clone())
                .unwrap_or_default(),
            options: TableOptions {
                engine: text(&self.engine),
                collation: text(&self.collation),
                comment: self.table_comment.read(cx).value().to_string(),
            },
        }
    }

    /// The statements to run; none when an existing table is unchanged.
    fn sql(&self, cx: &App) -> Result<Vec<String>, String> {
        let mut design = self.design(cx);
        design.follow_renames();
        design.validate_for(self.dialect)?;
        Ok(match &self.original {
            None => create_statements(self.dialect, &self.db, &design),
            Some(original) => {
                design.validate_change(self.dialect, original)?;
                alter_sql(self.dialect, &self.db, original, &design)
            }
        })
    }

    fn move_row(&mut self, ix: usize, up: bool, cx: &mut Context<Self>) {
        let target = if up { ix.checked_sub(1) } else { Some(ix + 1) };
        if let Some(target) = target.filter(|t| *t < self.rows.len()) {
            self.rows.swap(ix, target);
            cx.notify();
        }
    }

    fn render_row(
        ix: usize,
        row: &Row,
        comments: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            .child(div().flex_1().when(comments, |cell| {
                cell.child(Input::new(&row.comment).xsmall())
            }))
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

impl TableDesigner {
    fn render_index(
        ix: usize,
        index: &IndexRow,
        kinds: &'static [IndexKind],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        h_flex()
            .gap_1()
            .child(div().w(px(180.)).child(Input::new(&index.name).xsmall()))
            .child(
                Button::new(("kind", index.id))
                    .xsmall()
                    .outline()
                    .w(px(96.))
                    .label(index.kind.label())
                    .dropdown_menu(move |menu, _, _| {
                        kinds.iter().copied().fold(menu, |menu, kind| {
                            let this = this.clone();
                            menu.item(PopupMenuItem::new(kind.label()).on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    if let Some(index) = this.indexes.get_mut(ix) {
                                        index.kind = kind;
                                    }
                                    cx.notify();
                                })
                                .ok();
                            }))
                        })
                    }),
            )
            .child(div().flex_1().child(Input::new(&index.columns).xsmall()))
            .child(
                Button::new(("remove-index", index.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove index")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if ix < this.indexes.len() {
                            this.indexes.remove(ix);
                        }
                        cx.notify();
                    })),
            )
    }
}

impl TableDesigner {
    fn action_menu(
        id: (&'static str, usize),
        current: &str,
        ix: usize,
        set: fn(&mut ForeignKeyRow, String),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let label = if current.is_empty() {
            "default".to_owned()
        } else {
            current.to_owned()
        };
        Button::new(id)
            .xsmall()
            .outline()
            .w(px(96.))
            .label(label)
            .dropdown_menu(move |menu, _, _| {
                REFERENTIAL_ACTIONS.into_iter().fold(menu, |menu, action| {
                    let this = this.clone();
                    menu.item(PopupMenuItem::new(action).on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(fk) = this.foreign_keys.get_mut(ix) {
                                set(fk, action.to_owned());
                            }
                            cx.notify();
                        })
                        .ok();
                    }))
                })
            })
    }

    fn render_foreign_key(
        ix: usize,
        fk: &ForeignKeyRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(div().w(px(150.)).child(Input::new(&fk.name).xsmall()))
            .child(div().w(px(150.)).child(Input::new(&fk.columns).xsmall()))
            .child(div().w(px(150.)).child(Input::new(&fk.ref_table).xsmall()))
            .child(div().flex_1().child(Input::new(&fk.ref_columns).xsmall()))
            .child(Self::action_menu(
                ("on-delete", fk.id),
                &fk.on_delete,
                ix,
                |fk, v| fk.on_delete = v,
                cx,
            ))
            .child(Self::action_menu(
                ("on-update", fk.id),
                &fk.on_update,
                ix,
                |fk, v| fk.on_update = v,
                cx,
            ))
            .child(
                Button::new(("remove-fk", fk.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove foreign key")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if ix < this.foreign_keys.len() {
                            this.foreign_keys.remove(ix);
                        }
                        cx.notify();
                    })),
            )
    }
}

impl TableDesigner {
    fn render_check(ix: usize, check: &CheckRow, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(div().w(px(150.)).child(Input::new(&check.name).xsmall()))
            .child(div().flex_1().child(Input::new(&check.clause).xsmall()))
            .child(
                Button::new(("remove-check", check.id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove check")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if ix < this.checks.len() {
                            this.checks.remove(ix);
                        }
                        cx.notify();
                    })),
            )
    }
}

impl Render for TableDesigner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mysql = self.dialect == Dialect::MySql;
        let kinds: &'static [IndexKind] = if mysql {
            &IndexKind::ALL
        } else {
            &IndexKind::ALL[..2]
        };
        let muted = cx.theme().muted_foreground;
        let header = |text: &'static str, width: Option<f32>| {
            let cell = div().text_xs().text_color(muted).child(text);
            match width {
                Some(w) => cell.w(px(w)),
                None => cell.flex_1(),
            }
        };
        let preview = match self.sql(cx) {
            Ok(sql) if sql.is_empty() => div().text_color(muted).child("No changes."),
            Ok(sql) => div().child(sql.join(";\n\n") + ";"),
            Err(err) => div().text_color(cx.theme().danger).child(err),
        };
        let add = |id: &'static str, label: &'static str| {
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .label(label)
        };
        let (headers, rows, add_button): (Vec<_>, Vec<_>, Button) = match self.section {
            Section::Columns => (
                vec![
                    header("Column", Some(150.)),
                    header("Type", Some(150.)),
                    header("NULL", Some(44.)),
                    header("PK", Some(36.)),
                    header("Auto", Some(36.)),
                    header("Default (SQL)", Some(140.)),
                    header(if mysql { "Comment" } else { "" }, None),
                    div().w(px(84.)),
                ],
                self.rows
                    .iter()
                    .enumerate()
                    .map(|(ix, row)| Self::render_row(ix, row, mysql, cx).into_any_element())
                    .collect(),
                add("add-column", "Column").on_click(cx.listener(|this, _, window, cx| {
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
            Section::Indexes => (
                vec![
                    header("Index", Some(180.)),
                    header("Kind", Some(96.)),
                    header("Columns (comma-separated, optional prefix length)", None),
                    div().w(px(28.)),
                ],
                self.indexes
                    .iter()
                    .enumerate()
                    .map(|(ix, index)| Self::render_index(ix, index, kinds, cx).into_any_element())
                    .collect(),
                add("add-index", "Index").on_click(cx.listener(|this, _, window, cx| {
                    this.push_index(
                        IndexDef {
                            name: format!("idx_{}", this.indexes.len() + 1),
                            ..IndexDef::default()
                        },
                        window,
                        cx,
                    );
                })),
            ),
            Section::ForeignKeys => (
                vec![
                    header("Foreign key", Some(150.)),
                    header("Columns", Some(150.)),
                    header("References table", Some(150.)),
                    header("Referenced columns", None),
                    header("On delete", Some(96.)),
                    header("On update", Some(96.)),
                    div().w(px(28.)),
                ],
                self.foreign_keys
                    .iter()
                    .enumerate()
                    .map(|(ix, fk)| Self::render_foreign_key(ix, fk, cx).into_any_element())
                    .collect(),
                add("add-fk", "Foreign key").on_click(cx.listener(|this, _, window, cx| {
                    let name = this.design(cx).name;
                    this.push_foreign_key(
                        ForeignKeyDef {
                            name: format!("fk_{name}_{}", this.foreign_keys.len() + 1),
                            ..ForeignKeyDef::default()
                        },
                        window,
                        cx,
                    );
                })),
            ),
            Section::Checks => (
                vec![
                    header("Check", Some(150.)),
                    header("Expression", None),
                    div().w(px(28.)),
                ],
                self.checks
                    .iter()
                    .enumerate()
                    .map(|(ix, check)| Self::render_check(ix, check, cx).into_any_element())
                    .collect(),
                add("add-check", "Check").on_click(cx.listener(|this, _, window, cx| {
                    let name = this.design(cx).name;
                    this.push_check(
                        CheckDef {
                            name: format!("{name}_chk_{}", this.checks.len() + 1),
                            ..CheckDef::default()
                        },
                        window,
                        cx,
                    );
                })),
            ),
        };
        let counts = [
            self.rows.len(),
            self.indexes.len(),
            self.foreign_keys.len(),
            self.checks.len(),
        ];
        let tabs = Section::ALL
            .iter()
            .zip(counts)
            .map(|(section, count)| Tab::new().label(format!("{} ({count})", section.label())));
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child("Table name"))
                    .child(div().w(px(220.)).child(Input::new(&self.name).small()))
                    .when(mysql, |row| {
                        row.child(div().text_sm().child("Engine"))
                            .child(div().w(px(110.)).child(Input::new(&self.engine).small()))
                            .child(div().text_sm().child("Collation"))
                            .child(div().w(px(170.)).child(Input::new(&self.collation).small()))
                            .child(div().text_sm().child("Comment"))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.table_comment).small()),
                            )
                    }),
            )
            .child(
                TabBar::new("designer-sections")
                    .underline()
                    .small()
                    .selected_index(self.section as usize)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.section = Section::ALL[*ix];
                        cx.notify();
                    }))
                    .children(tabs),
            )
            .child(h_flex().gap_1().children(headers))
            .child(
                v_flex()
                    .id("designer-rows")
                    .gap_1()
                    .h(px(300.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(h_flex().child(add_button))
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
    dialect: Dialect,
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
                        let foreign_keys = s.list_foreign_keys(&d, &t).await?;
                        let options = s.table_options(&d, &t).await?;
                        let checks = s.list_checks(&d, &t).await?;
                        let mut design = TableDesign::from_table(dialect, &t, &columns, &indexes)
                            .with_constraints(&d, &foreign_keys, options)
                            .with_checks(&checks);
                        // A SQLite rebuild re-creates the table's triggers and keeps its options.
                        if dialect == Dialect::Sqlite {
                            let create = s.object_ddl(&d, &t, ObjectKind::Table).await?;
                            let mut triggers = Vec::new();
                            for trigger in s.list_objects(&d, ObjectKind::Trigger).await? {
                                if trigger.detail == t {
                                    triggers.push(
                                        s.object_ddl(&d, &trigger.name, ObjectKind::Trigger)
                                            .await?,
                                    );
                                }
                            }
                            design = design.with_sqlite_parts(&create, triggers);
                        }
                        Ok::<_, ovs_core::Error>(design)
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
            cx.update(|window, cx| show(dialect, db, original, apply, window, cx))
                .ok();
        })
        .detach();
}

fn show(
    dialect: Dialect,
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
    let designer = cx.new(|cx| TableDesigner::new(dialect, db, original, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let (designer, apply) = (designer.clone(), apply.clone());
        dialog
            .title(title.clone())
            .w(px(1080.))
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
                                Ok(sql) if sql.is_empty() => window.close_dialog(cx),
                                Ok(sql) => {
                                    window.close_dialog(cx);
                                    apply(sql, window, cx);
                                }
                                // The preview already shows what's wrong.
                                Err(_) => {}
                            }),
                    ),
            )
    });
}
