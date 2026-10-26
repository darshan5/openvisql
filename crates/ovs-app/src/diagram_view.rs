//! The ER diagram window: a database's tables as boxes joined by their foreign keys. Drag the
//! background to pan, drag a table to move it, scroll with ⌘/Ctrl held (or pinch) to zoom,
//! and export the picture as SVG.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement as _, PathBuilder, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, Styled as _, Window, canvas, div, point, px, size,
};
use ovs_core::diagram::{CHAR_WIDTH, Diagram, PADDING, ROW_HEIGHT, column_marker};

use crate::runtime::{self, SharedSession};

const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 3.;
/// The diagram's font size at zoom 1, matching [`CHAR_WIDTH`].
const FONT_SIZE: f32 = 12.;

enum Drag {
    /// Moving the view: where the mouse and the offset were when it started.
    Pan {
        mouse: Point<Pixels>,
        offset: Point<Pixels>,
    },
    /// Moving table `table`, from its diagram position when the drag started.
    Table {
        table: usize,
        mouse: Point<Pixels>,
        at: (f32, f32),
    },
}

pub struct DiagramView {
    db: String,
    diagram: Diagram,
    zoom: f32,
    /// Where the diagram's origin is drawn, relative to the canvas.
    offset: Point<Pixels>,
    drag: Option<Drag>,
    /// Set until the first frame, which fits the diagram to the window.
    fit_pending: bool,
    /// The canvas's place in the window, from the last frame.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl DiagramView {
    fn new(db: String, diagram: Diagram) -> Self {
        Self {
            db,
            diagram,
            zoom: 1.,
            offset: point(px(20.), px(20.)),
            drag: None,
            fit_pending: true,
            bounds: Rc::default(),
        }
    }

    /// Zooms to `zoom`, keeping the diagram point under `anchor` (canvas coordinates) still.
    fn zoom_to(&mut self, zoom: f32, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let scale = zoom / self.zoom;
        self.offset = point(
            anchor.x - (anchor.x - self.offset.x) * scale,
            anchor.y - (anchor.y - self.offset.y) * scale,
        );
        self.zoom = zoom;
        cx.notify();
    }

    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let centre = self.bounds.get().map_or(point(px(400.), px(300.)), |b| {
            point(b.size.width / 2., b.size.height / 2.)
        });
        self.zoom_to(self.zoom * factor, centre, cx);
    }

    /// Shows the whole diagram, no bigger than actual size.
    fn fit(&mut self, area: gpui_kit::Size<Pixels>, cx: &mut Context<Self>) {
        let (w, h) = self.diagram.size();
        let margin = 24.;
        let zoom = ((f32::from(area.width) - 2. * margin) / w.max(1.))
            .min((f32::from(area.height) - 2. * margin) / h.max(1.))
            .clamp(MIN_ZOOM, 1.);
        self.zoom = zoom;
        self.offset = point(px(margin), px(margin));
        cx.notify();
    }

    fn rearrange(&mut self, cx: &mut Context<Self>) {
        self.diagram.layout();
        self.fit_pending = true;
        cx.notify();
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let chosen = cx.prompt_for_new_path(&dir, Some(&format!("{}-diagram.svg", self.db)));
        let svg = self.diagram.to_svg();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(path))) = chosen.await else {
                    return;
                };
                let message = match std::fs::write(&path, svg) {
                    Ok(()) => format!("Saved the diagram to {}", path.display()),
                    Err(err) => format!("Could not save the diagram: {err}"),
                };
                cx.update(|window, cx| window.push_notification(message, cx))
                    .ok();
            })
            .detach();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        match &self.drag {
            Some(Drag::Pan { mouse, offset }) => {
                self.offset = *offset + (event.position - *mouse);
                cx.notify();
            }
            Some(Drag::Table { table, mouse, at }) => {
                let delta = event.position - *mouse;
                let t = &mut self.diagram.tables[*table];
                t.x = at.0 + f32::from(delta.x) / self.zoom;
                t.y = at.1 + f32::from(delta.y) / self.zoom;
                cx.notify();
            }
            None => {}
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(ROW_HEIGHT));
        if event.modifiers.secondary() {
            let origin = self.bounds.get().map_or(Point::default(), |b| b.origin);
            let factor = (1. + f32::from(delta.y) / 300.).clamp(0.5, 2.);
            self.zoom_to(self.zoom * factor, event.position - origin, cx);
        } else {
            self.offset += delta;
            cx.notify();
        }
    }

    /// The table boxes, positioned for the current zoom and offset.
    fn render_tables(&self, cx: &mut Context<Self>) -> Vec<gpui_kit::AnyElement> {
        let theme = cx.theme();
        let (border, header, muted, mono) = (
            theme.border,
            theme.secondary,
            theme.muted_foreground,
            theme.mono_font_family.clone(),
        );
        let bg = theme.background;
        let z = self.zoom;
        let row = px(ROW_HEIGHT * z);
        self.diagram
            .tables
            .iter()
            .enumerate()
            .map(|(ix, t)| {
                let at = (t.x, t.y);
                let rows = t.columns.iter().map(|c| {
                    h_flex()
                        .h(row)
                        .px(px(PADDING * z))
                        .justify_between()
                        .gap(px(2. * CHAR_WIDTH * z))
                        .child(div().when_bold(c.primary).child(format!(
                            "{}{}",
                            column_marker(c),
                            c.name
                        )))
                        .child(div().text_color(muted).child(c.column_type.clone()))
                });
                v_flex()
                    .id(("diagram-table", ix))
                    .absolute()
                    .left(self.offset.x + px(t.x * z))
                    .top(self.offset.y + px(t.y * z))
                    .w(px(t.width() * z))
                    .h(px(t.height() * z))
                    .overflow_hidden()
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .rounded(px(4. * z))
                    .font_family(mono.clone())
                    .text_size(px(FONT_SIZE * z))
                    .line_height(row)
                    .cursor_move()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.drag = Some(Drag::Table {
                                table: ix,
                                mouse: event.position,
                                at,
                            });
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .h(row)
                            .px(px(PADDING * z))
                            .bg(header)
                            .border_b_1()
                            .border_color(border)
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(t.name.clone()),
                    )
                    .children(rows)
                    .into_any_element()
            })
            .collect()
    }
}

/// Bold text for primary key columns.
trait BoldIf {
    fn when_bold(self, bold: bool) -> Self;
}

impl BoldIf for gpui_kit::Div {
    fn when_bold(self, bold: bool) -> Self {
        if bold {
            self.font_weight(gpui_kit::FontWeight::BOLD)
        } else {
            self
        }
    }
}

impl Render for DiagramView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.fit_pending {
            self.fit_pending = false;
            let viewport = window.viewport_size();
            // The toolbar takes the top of the window.
            self.fit(size(viewport.width, viewport.height - px(44.)), cx);
        }
        let theme = cx.theme();
        let line = theme.muted_foreground;
        let links: Vec<_> = self
            .diagram
            .relations
            .iter()
            .filter_map(|r| self.diagram.link(r))
            .collect();
        let (zoom, offset, bounds) = (self.zoom, self.offset, self.bounds.clone());
        let connectors = canvas(
            move |b, _, _| bounds.set(Some(b)),
            move |b, (), window, _| {
                let at = |p: (f32, f32)| {
                    point(
                        b.origin.x + offset.x + px(p.0 * zoom),
                        b.origin.y + offset.y + px(p.1 * zoom),
                    )
                };
                for link in &links {
                    let (start, end) = (at(link.start), at(link.end));
                    let bend = px(link.bend * zoom);
                    let end_bend = if link.looped { -bend } else { bend };
                    let mut path = PathBuilder::stroke(px((1.4 * zoom).max(1.)));
                    path.move_to(start);
                    path.cubic_bezier_to(
                        end,
                        point(start.x + bend, start.y),
                        point(end.x - end_bend, end.y),
                    );
                    if let Ok(path) = path.build() {
                        window.paint_path(path, line);
                    }
                    // A dot where the key points to.
                    let r = px((3. * zoom).max(2.));
                    window.paint_quad(
                        gpui_kit::fill(
                            Bounds::new(point(end.x - r, end.y - r), size(r * 2., r * 2.)),
                            line,
                        )
                        .corner_radii(r),
                    );
                }
            },
        )
        .absolute()
        .size_full();
        let tables = self.render_tables(cx);
        let (count, links) = (self.diagram.tables.len(), self.diagram.relations.len());
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .h(px(44.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child(SharedString::from(format!(
                        "{count} tables, {links} relationships"
                    ))))
                    .child(div().flex_1())
                    .child(
                        Button::new("zoom-out")
                            .ghost()
                            .small()
                            .icon(IconName::Minus)
                            .tooltip("Zoom out (⌘/Ctrl + scroll)")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1. / 1.25, cx))),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .text_sm()
                            .text_center()
                            .child(format!("{:.0}%", self.zoom * 100.)),
                    )
                    .child(
                        Button::new("zoom-in")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("Zoom in (⌘/Ctrl + scroll)")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1.25, cx))),
                    )
                    .child(
                        Button::new("fit")
                            .ghost()
                            .small()
                            .label("Fit")
                            .tooltip("Show the whole diagram")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.fit_pending = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("rearrange")
                            .ghost()
                            .small()
                            .label("Rearrange")
                            .tooltip("Lay the tables out again")
                            .on_click(cx.listener(|this, _, _, cx| this.rearrange(cx))),
                    )
                    .child(
                        Button::new("export")
                            .small()
                            .label("Export SVG…")
                            .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
                    ),
            )
            .child(
                div()
                    .id("diagram-canvas")
                    .relative()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .cursor_grab()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.drag = Some(Drag::Pan {
                                mouse: event.position,
                                offset: this.offset,
                            });
                            cx.notify();
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
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .child(connectors)
                    .children(tables),
            )
    }
}

/// Loads `db`'s diagram on `session` and opens it in a window of its own.
pub fn open(db: String, session: SharedSession, window: &mut Window, cx: &mut App) {
    window.push_notification(format!("Drawing the diagram of {db}…"), cx);
    window
        .spawn(cx, async move |cx| {
            let name = db.clone();
            let loaded = runtime::run(async move {
                let mut s = session.lock().await;
                Diagram::load(s.as_mut(), &name).await
            })
            .await;
            cx.update(|window, cx| match loaded {
                Ok(diagram) => {
                    let options = crate::window_options(format!("ER diagram: {db}"), cx);
                    if let Err(err) = gpui_kit::open_window(options, cx, move |_, cx| {
                        cx.new(|_| DiagramView::new(db, diagram))
                    }) {
                        window.push_notification(format!("Could not open the diagram: {err}"), cx);
                    }
                }
                Err(err) => {
                    window.push_notification(format!("Could not draw the diagram: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
}
