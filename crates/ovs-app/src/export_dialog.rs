//! The options form shown in the Export dialog.

use gpui_kit::component::{
    checkbox::Checkbox,
    form::{Field, Form},
    input::{Input, InputState},
    radio::RadioGroup,
};
use gpui_kit::{
    App, AppContext as _, Axis, Context, Entity, IntoElement, ParentElement as _, Render, Window,
    prelude::FluentBuilder as _,
};
use ovs_core::export::{ExportFormat, ExportOptions};

const DELIMITERS: [(char, &str); 4] = [
    (',', "Comma"),
    (';', "Semicolon"),
    ('\t', "Tab"),
    ('|', "Pipe"),
];

pub struct ExportForm {
    format: usize,
    delimiter: usize,
    header: bool,
    escape_formulas: bool,
    null_text: Entity<InputState>,
    table: Entity<InputState>,
    /// Rows currently in the grid.
    shown_rows: usize,
    /// Whether "all rows" is offered: the result was cut short by a LIMIT.
    can_fetch_all: bool,
    all_rows: bool,
}

impl ExportForm {
    pub fn new(
        shown_rows: usize,
        can_fetch_all: bool,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            format: 0,
            delimiter: 0,
            header: true,
            escape_formulas: false,
            null_text: cx.new(|cx| InputState::new(window, cx).placeholder("(empty)")),
            table: cx.new(|cx| InputState::new(window, cx).default_value(table)),
            shown_rows,
            can_fetch_all,
            all_rows: false,
        }
    }

    pub fn format(&self) -> ExportFormat {
        ExportFormat::ALL[self.format]
    }

    /// True when the user asked for every row, not just the ones in the grid.
    pub fn all_rows(&self) -> bool {
        self.can_fetch_all && self.all_rows
    }

    pub fn options(&self, cx: &App) -> ExportOptions {
        let table = self.table.read(cx).value().trim().to_owned();
        ExportOptions {
            format: self.format(),
            delimiter: DELIMITERS[self.delimiter].0,
            header: self.header,
            null_text: self.null_text.read(cx).value().to_string(),
            escape_formulas: self.escape_formulas,
            table: if table.is_empty() {
                "result".into()
            } else {
                table
            },
            ..ExportOptions::default()
        }
    }
}

impl Render for ExportForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let format = self.format();
        Form::new()
            .label_layout(Axis::Horizontal)
            .child(
                Field::new().label("Format").child(
                    RadioGroup::horizontal("export-format")
                        .children(ExportFormat::ALL.map(ExportFormat::label))
                        .selected_index(Some(self.format))
                        .on_change(cx.listener(|this, ix: &usize, _, cx| {
                            this.format = *ix;
                            cx.notify();
                        })),
                ),
            )
            .when(format == ExportFormat::Csv, |form| {
                form.child(
                    Field::new().label("Separator").child(
                        RadioGroup::horizontal("export-delimiter")
                            .children(DELIMITERS.map(|(_, label)| label))
                            .selected_index(Some(self.delimiter))
                            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                this.delimiter = *ix;
                                cx.notify();
                            })),
                    ),
                )
                .child(
                    Field::new().label_indent(true).child(
                        Checkbox::new("export-header")
                            .label("Column names in the first line")
                            .checked(self.header)
                            .on_change(cx.listener(|this, value: &bool, _, cx| {
                                this.header = *value;
                                cx.notify();
                            })),
                    ),
                )
                .child(
                    Field::new().label_indent(true).child(
                        Checkbox::new("export-formulas")
                            .label(
                                "Protect against spreadsheet formulas (prefix =, +, -, @ with ')",
                            )
                            .checked(self.escape_formulas)
                            .on_change(cx.listener(|this, value: &bool, _, cx| {
                                this.escape_formulas = *value;
                                cx.notify();
                            })),
                    ),
                )
            })
            .when(
                matches!(format, ExportFormat::Csv | ExportFormat::Markdown),
                |form| {
                    form.child(
                        Field::new()
                            .label("NULL as")
                            .child(Input::new(&self.null_text)),
                    )
                },
            )
            .when(format == ExportFormat::SqlInsert, |form| {
                form.child(Field::new().label("Table").child(Input::new(&self.table)))
            })
            .when(self.can_fetch_all, |form| {
                form.child(
                    Field::new().label("Rows").child(
                        RadioGroup::horizontal("export-rows")
                            .children([
                                format!("The {} shown", self.shown_rows),
                                "All rows (runs the query again without LIMIT)".to_owned(),
                            ])
                            .selected_index(Some(usize::from(self.all_rows)))
                            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                this.all_rows = *ix == 1;
                                cx.notify();
                            })),
                    ),
                )
            })
    }
}
