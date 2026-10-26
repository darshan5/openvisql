//! Entity-relationship diagrams: a database's tables as boxes listing their columns, joined by
//! their foreign keys, laid out automatically. The layout is in abstract units based on a
//! monospace character cell, so the app and the SVG export draw the same picture.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use crate::driver::Session;
use crate::schema::{ForeignKeyInfo, ObjectKind};
use crate::{Error, Result};

/// Width of one character of the diagram's monospace font.
pub const CHAR_WIDTH: f32 = 7.2;
/// Height of a column row; the table name row is the same.
pub const ROW_HEIGHT: f32 = 20.;
/// Space inside a box, left and right of the text.
pub const PADDING: f32 = 10.;
/// Space between layers (left to right) and between boxes in a layer.
pub const H_GAP: f32 = 90.;
pub const V_GAP: f32 = 30.;

#[derive(Debug, Clone, PartialEq)]
pub struct DiagramColumn {
    pub name: String,
    pub column_type: String,
    pub primary: bool,
    /// Part of a foreign key.
    pub foreign: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiagramTable {
    pub name: String,
    pub columns: Vec<DiagramColumn>,
    /// Top-left corner, set by [`Diagram::layout`].
    pub x: f32,
    pub y: f32,
}

impl DiagramTable {
    pub fn width(&self) -> f32 {
        let widest = self
            .columns
            .iter()
            .map(|c| c.name.chars().count() + c.column_type.chars().count() + 4)
            .chain(std::iter::once(self.name.chars().count()))
            .max()
            .unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let text = widest as f32 * CHAR_WIDTH;
        (text + 2. * PADDING).max(140.)
    }

    pub fn height(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let rows = (self.columns.len() + 1) as f32;
        rows * ROW_HEIGHT + 6.
    }

    /// The vertical middle of column `name`'s row, or of the title when there's no such column.
    pub fn row_y(&self, name: &str) -> f32 {
        let row = self
            .columns
            .iter()
            .position(|c| c.name == name)
            .map_or(0, |i| i + 1);
        #[allow(clippy::cast_precision_loss)]
        let row = row as f32;
        self.y + row * ROW_HEIGHT + ROW_HEIGHT / 2.
    }
}

/// A foreign key: `from_table`'s `from_columns` refer to `to_table`'s `to_columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub name: String,
    pub from_table: String,
    pub from_columns: Vec<String>,
    pub to_table: String,
    pub to_columns: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diagram {
    pub tables: Vec<DiagramTable>,
    pub relations: Vec<Relation>,
}

impl Diagram {
    /// The diagram of `db`'s tables, laid out. Foreign keys to other databases are left out.
    pub async fn load(session: &mut dyn Session, db: &str) -> Result<Self> {
        let mut diagram = Diagram::default();
        for table in session.list_objects(db, ObjectKind::Table).await? {
            let fks = session.list_foreign_keys(db, &table.name).await?;
            let foreign: HashSet<&String> = fks.iter().flat_map(|fk| &fk.columns).collect();
            let columns = session
                .list_columns(db, &table.name)
                .await?
                .into_iter()
                .map(|c| DiagramColumn {
                    primary: c.is_primary(),
                    foreign: foreign.contains(&c.name),
                    name: c.name,
                    column_type: c.column_type,
                })
                .collect();
            diagram
                .relations
                .extend(fks.iter().filter(|fk| same_db(fk, db)).map(|fk| Relation {
                    name: fk.name.clone(),
                    from_table: table.name.clone(),
                    from_columns: fk.columns.clone(),
                    to_table: fk.ref_table.clone(),
                    to_columns: fk.ref_columns.clone(),
                }));
            diagram.tables.push(DiagramTable {
                name: table.name,
                columns,
                x: 0.,
                y: 0.,
            });
        }
        // Keys to tables that aren't listed (another schema, say) have nothing to point at.
        let names: HashSet<String> = diagram.tables.iter().map(|t| t.name.clone()).collect();
        diagram.relations.retain(|r| names.contains(&r.to_table));
        if diagram.tables.is_empty() {
            return Err(Error::Other(format!("{db} has no tables to draw")));
        }
        diagram.layout();
        Ok(diagram)
    }

    /// Places the tables: those referred to sit left of those referring to them, each layer is
    /// ordered to keep lines from crossing, and tables with no relations go in a grid after.
    pub fn layout(&mut self) {
        let index: HashMap<&str, usize> = self
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.as_str(), i))
            .collect();
        let n = self.tables.len();
        // Edges from a referring table to the table it refers to, without self-references.
        let mut refers: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut referred_by: Vec<Vec<usize>> = vec![Vec::new(); n];
        for r in &self.relations {
            if let (Some(&a), Some(&b)) = (
                index.get(r.from_table.as_str()),
                index.get(r.to_table.as_str()),
            ) && a != b
                && !refers[a].contains(&b)
            {
                refers[a].push(b);
                referred_by[b].push(a);
            }
        }
        let related: Vec<bool> = (0..n)
            .map(|i| !refers[i].is_empty() || !referred_by[i].is_empty())
            .collect();

        // Layer = longest chain of references below a table (cycles are cut by the visit set).
        let mut layer: Vec<Option<usize>> = vec![None; n];
        let mut visiting = HashSet::new();
        let mut layers: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in (0..n).filter(|&i| related[i]) {
            let d = depth(i, &refers, &mut layer, &mut visiting);
            layers.entry(d).or_default().push(i);
        }

        // Order each layer to keep lines from crossing: a few sweeps back and forth, each
        // sorting a layer by the average position of its tables' neighbours.
        let neighbours: Vec<Vec<usize>> = (0..n)
            .map(|i| refers[i].iter().chain(&referred_by[i]).copied().collect())
            .collect();
        let mut ordered: Vec<Vec<usize>> = layers.into_values().collect();
        let mut position: Vec<f32> = vec![0.; n];
        let number = |members: &[usize], position: &mut Vec<f32>| {
            for (p, &i) in members.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let p = p as f32;
                position[i] = p;
            }
        };
        for members in &mut ordered {
            members.sort_by(|&a, &b| self.tables[a].name.cmp(&self.tables[b].name));
            number(members, &mut position);
        }
        for sweep in 0..4 {
            let layers: Vec<usize> = if sweep % 2 == 0 {
                (0..ordered.len()).collect()
            } else {
                (0..ordered.len()).rev().collect()
            };
            for k in layers {
                let key = |i: usize| {
                    average(neighbours[i].iter().map(|&j| position[j])).unwrap_or(position[i])
                };
                let keys: HashMap<usize, f32> = ordered[k].iter().map(|&i| (i, key(i))).collect();
                ordered[k].sort_by(|a, b| keys[a].total_cmp(&keys[b]));
                number(&ordered[k], &mut position);
            }
        }

        // Columns left to right; in each, boxes sit level with the tables they connect to where
        // there's room. Twice: the second pass also sees neighbours further right.
        let mut x = 0.;
        for members in &ordered {
            let width = members
                .iter()
                .map(|&i| self.tables[i].width())
                .fold(0., f32::max);
            for &i in members {
                self.tables[i].x = x;
            }
            x += width + H_GAP;
        }
        let mut placed = vec![false; n];
        for _ in 0..2 {
            for members in &ordered {
                let mut top: f32 = 0.;
                for &i in members {
                    let centre = average(
                        neighbours[i]
                            .iter()
                            .filter(|&&j| placed[j])
                            .map(|&j| self.tables[j].y + self.tables[j].height() / 2.),
                    );
                    let height = self.tables[i].height();
                    let y = centre.map_or(top, |c| (c - height / 2.).max(top));
                    self.tables[i].y = y;
                    top = y + height + V_GAP;
                }
                for &i in members {
                    placed[i] = true;
                }
            }
        }
        let bottom = ordered
            .iter()
            .flatten()
            .map(|&i| self.tables[i].y + self.tables[i].height())
            .fold(0., f32::max);

        // Unrelated tables in rows below, wrapped to roughly the width used so far.
        let wrap = x.max(900.);
        let (mut x, mut y, mut row_height) = (
            0.,
            if ordered.is_empty() {
                0.
            } else {
                bottom + V_GAP
            },
            0f32,
        );
        let mut loose: Vec<usize> = (0..n).filter(|&i| !related[i]).collect();
        loose.sort_by(|&a, &b| self.tables[a].name.cmp(&self.tables[b].name));
        for i in loose {
            let width = self.tables[i].width();
            if x > 0. && x + width > wrap {
                x = 0.;
                y += row_height + V_GAP;
                row_height = 0.;
            }
            self.tables[i].x = x;
            self.tables[i].y = y;
            x += width + V_GAP;
            row_height = row_height.max(self.tables[i].height());
        }
    }

    /// The width and height everything fits in.
    pub fn size(&self) -> (f32, f32) {
        self.tables.iter().fold((0., 0.), |(w, h), t| {
            (w.max(t.x + t.width()), h.max(t.y + t.height()))
        })
    }

    pub fn table(&self, name: &str) -> Option<&DiagramTable> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The line for `relation`: from the side of the referring table nearest the other table,
    /// at its first key column, to the referred-to table's matching column. `None` when either
    /// table is missing. A self-reference loops out of the right side.
    pub fn link(&self, relation: &Relation) -> Option<Link> {
        let from = self.table(&relation.from_table)?;
        let to = self.table(&relation.to_table)?;
        let from_y = from.row_y(relation.from_columns.first().map_or("", String::as_str));
        let to_y = to.row_y(relation.to_columns.first().map_or("", String::as_str));
        if from.name == to.name {
            let x = from.x + from.width();
            return Some(Link {
                start: (x, from_y),
                end: (x, to_y),
                bend: 40.,
                looped: true,
            });
        }
        let (from_mid, to_mid) = (from.x + from.width() / 2., to.x + to.width() / 2.);
        let (start_x, end_x, bend) = if to_mid < from_mid {
            (from.x, to.x + to.width(), -1.)
        } else {
            (from.x + from.width(), to.x, 1.)
        };
        let reach = ((end_x - start_x).abs() / 2.).max(30.);
        Some(Link {
            start: (start_x, from_y),
            end: (end_x, to_y),
            bend: bend * reach,
            looped: false,
        })
    }

    /// The diagram as a standalone SVG image.
    pub fn to_svg(&self) -> String {
        let (width, height) = self.size();
        let margin = 20.;
        let mut svg = String::new();
        let _ = write!(
            svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" \
             viewBox=\"{vx} {vy} {w} {h}\" font-family=\"Menlo, Consolas, monospace\" \
             font-size=\"12\">\n<rect x=\"{vx}\" y=\"{vy}\" width=\"{w}\" height=\"{h}\" \
             fill=\"#ffffff\"/>\n",
            w = width + 2. * margin,
            h = height + 2. * margin,
            vx = -margin,
            vy = -margin,
        );
        for relation in &self.relations {
            if let Some(link) = self.link(relation) {
                let ((x1, y1), (x2, y2)) = (link.start, link.end);
                let _ = writeln!(
                    svg,
                    "<path d=\"M {x1} {y1} C {c1} {y1}, {c2} {y2}, {x2} {y2}\" fill=\"none\" \
                     stroke=\"#7a8699\" stroke-width=\"1.4\"/>\
                     <circle cx=\"{x2}\" cy=\"{y2}\" r=\"3\" fill=\"#7a8699\"/>",
                    c1 = x1 + link.bend,
                    c2 = x2 - if link.looped { -link.bend } else { link.bend },
                );
            }
        }
        for t in &self.tables {
            let (x, y, w, h) = (t.x, t.y, t.width(), t.height());
            let _ = writeln!(
                svg,
                "<g><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"4\" \
                 fill=\"#ffffff\" stroke=\"#9aa5b5\"/>\
                 <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{ROW_HEIGHT}\" rx=\"4\" \
                 fill=\"#e8edf4\" stroke=\"#9aa5b5\"/>\
                 <text x=\"{tx}\" y=\"{ty}\" font-weight=\"bold\">{name}</text>",
                tx = x + PADDING,
                ty = y + ROW_HEIGHT - 6.,
                name = escape(&t.name),
            );
            for (i, c) in t.columns.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let row_y = y + (i + 2) as f32 * ROW_HEIGHT - 6.;
                let marker = column_marker(c);
                let _ = writeln!(
                    svg,
                    "<text x=\"{tx}\" y=\"{row_y}\"{weight}>{marker}{name}</text>\
                     <text x=\"{rx}\" y=\"{row_y}\" text-anchor=\"end\" fill=\"#6b7280\">{ty}</text>",
                    tx = x + PADDING,
                    rx = x + w - PADDING,
                    weight = if c.primary {
                        " font-weight=\"bold\""
                    } else {
                        ""
                    },
                    name = escape(&c.name),
                    ty = escape(&c.column_type),
                );
            }
            svg.push_str("</g>\n");
        }
        svg.push_str("</svg>\n");
        svg
    }
}

/// The longest chain of references below table `i`; a cycle is cut where it closes.
fn depth(
    i: usize,
    refers: &[Vec<usize>],
    layer: &mut Vec<Option<usize>>,
    visiting: &mut HashSet<usize>,
) -> usize {
    if let Some(d) = layer[i] {
        return d;
    }
    if !visiting.insert(i) {
        return 0;
    }
    let d = refers[i]
        .iter()
        .map(|&j| depth(j, refers, layer, visiting) + 1)
        .max()
        .unwrap_or(0);
    visiting.remove(&i);
    layer[i] = Some(d);
    d
}

/// A relation's line, a curve from `start` to `end` whose control points sit `bend` to the
/// side (negative to the left) of each end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Link {
    pub start: (f32, f32),
    pub end: (f32, f32),
    pub bend: f32,
    /// A table referring to itself: both ends are on its right side.
    pub looped: bool,
}

/// `# ` before primary key columns, `→ ` before foreign key columns.
pub fn column_marker(c: &DiagramColumn) -> &'static str {
    if c.primary {
        "# "
    } else if c.foreign {
        "→ "
    } else {
        "  "
    }
}

/// The mean of `values`, or `None` when there are none.
fn average(values: impl Iterator<Item = f32>) -> Option<f32> {
    let (sum, count) = values.fold((0., 0u32), |(s, c), v| (s + v, c + 1));
    #[allow(clippy::cast_precision_loss)]
    (count > 0).then(|| sum / count as f32)
}

fn same_db(fk: &ForeignKeyInfo, db: &str) -> bool {
    fk.ref_database.is_empty() || fk.ref_database == db
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, columns: &[&str]) -> DiagramTable {
        DiagramTable {
            name: name.into(),
            columns: columns
                .iter()
                .map(|c| DiagramColumn {
                    name: (*c).into(),
                    column_type: "int".into(),
                    primary: *c == "id",
                    foreign: c.ends_with("_id"),
                })
                .collect(),
            x: 0.,
            y: 0.,
        }
    }

    fn relation(from: &str, column: &str, to: &str) -> Relation {
        Relation {
            name: format!("{from}_{column}"),
            from_table: from.into(),
            from_columns: vec![column.into()],
            to_table: to.into(),
            to_columns: vec!["id".into()],
        }
    }

    fn shop() -> Diagram {
        let mut d = Diagram {
            tables: vec![
                table("order_items", &["id", "order_id", "product_id", "qty"]),
                table("orders", &["id", "customer_id", "total"]),
                table("customers", &["id", "name"]),
                table("products", &["id", "name"]),
                table("notes", &["id", "text"]),
                table("employees", &["id", "manager_id"]),
            ],
            relations: vec![
                relation("order_items", "order_id", "orders"),
                relation("order_items", "product_id", "products"),
                relation("orders", "customer_id", "customers"),
                relation("employees", "manager_id", "employees"),
            ],
        };
        d.layout();
        d
    }

    #[test]
    fn referenced_tables_are_left_of_their_referrers() {
        let d = shop();
        let x = |name: &str| d.table(name).unwrap().x;
        assert!(x("customers") < x("orders"));
        assert!(x("orders") < x("order_items"));
        assert!(x("products") < x("order_items"));
        assert_eq!(x("customers"), 0.);
        assert_eq!(x("products"), 0.);
    }

    #[test]
    fn nothing_overlaps() {
        let d = shop();
        for (i, a) in d.tables.iter().enumerate() {
            for b in &d.tables[i + 1..] {
                let apart = a.x + a.width() <= b.x
                    || b.x + b.width() <= a.x
                    || a.y + a.height() <= b.y
                    || b.y + b.height() <= a.y;
                assert!(apart, "{} overlaps {}", a.name, b.name);
            }
        }
    }

    #[test]
    fn unrelated_and_self_referencing_tables_go_below() {
        let d = shop();
        let related_bottom = ["customers", "products", "orders", "order_items"]
            .iter()
            .map(|n| {
                let t = d.table(n).unwrap();
                t.y + t.height()
            })
            .fold(0., f32::max);
        // A table that only refers to itself has no neighbours to sit between.
        for name in ["notes", "employees"] {
            assert!(d.table(name).unwrap().y > related_bottom, "{name}");
        }
    }

    #[test]
    fn links_join_the_key_columns_on_facing_sides() {
        let d = shop();
        let link = d.link(&d.relations[2]).unwrap();
        let (orders, customers) = (d.table("orders").unwrap(), d.table("customers").unwrap());
        // From the left side of orders (customers is to its left) to the right of customers.
        assert_eq!(link.start, (orders.x, orders.row_y("customer_id")));
        assert_eq!(
            link.end,
            (customers.x + customers.width(), customers.row_y("id"))
        );
        assert!(link.bend < 0.);
        let looped = d.link(&d.relations[3]).unwrap();
        assert!(looped.looped && (looped.start.0 - looped.end.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cycles_still_lay_out() {
        let mut d = Diagram {
            tables: vec![table("a", &["id", "b_id"]), table("b", &["id", "a_id"])],
            relations: vec![relation("a", "b_id", "b"), relation("b", "a_id", "a")],
        };
        d.layout();
        assert_ne!(d.table("a").unwrap().x, d.table("b").unwrap().x);
    }

    #[test]
    fn svg_has_every_table_and_link_and_escapes_text() {
        let mut d = shop();
        d.tables[4].name = "a<b&c".into();
        let svg = d.to_svg();
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains(">a&lt;b&amp;c</text>"));
        assert_eq!(svg.matches("<g>").count(), 6);
        assert_eq!(svg.matches("<path ").count(), 4);
        assert!(svg.trim_end().ends_with("</svg>"));
    }
}
