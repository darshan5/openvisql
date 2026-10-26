//! The query builder's model: tables joined together, chosen columns, filters and sorting,
//! and further `SELECT`s combined with `UNION` and friends. `sql` turns it into a statement
//! for the connection's dialect.

use std::fmt::Write as _;

use super::dialect::Dialect;
use crate::schema::ForeignKeyInfo;

/// A table in a `SELECT`, under an alias that's unique within it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableRef {
    pub table: String,
    pub alias: String,
}

/// A column of one of the `SELECT`'s tables, by that table's alias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ColumnRef {
    pub alias: String,
    pub column: String,
}

impl ColumnRef {
    pub fn new(alias: &str, column: &str) -> Self {
        Self {
            alias: alias.to_owned(),
            column: column.to_owned(),
        }
    }

    fn sql(&self, d: Dialect) -> String {
        format!(
            "{}.{}",
            d.quote_ident(&self.alias),
            d.quote_ident(&self.column)
        )
    }

    /// `alias.column`, as menus show it.
    pub fn label(&self) -> String {
        format!("{}.{}", self.alias, self.column)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoinKind {
    #[default]
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

impl JoinKind {
    pub const ALL: [Self; 5] = [
        Self::Inner,
        Self::Left,
        Self::Right,
        Self::Full,
        Self::Cross,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::Right => "RIGHT JOIN",
            Self::Full => "FULL JOIN",
            Self::Cross => "CROSS JOIN",
        }
    }

    /// The kinds dialect `d` supports. MySQL and MariaDB have no `FULL JOIN`.
    pub fn available(d: Dialect) -> &'static [Self] {
        match d {
            Dialect::MySql => &[Self::Inner, Self::Left, Self::Right, Self::Cross],
            Dialect::Postgres | Dialect::Sqlite => &Self::ALL,
        }
    }
}

/// A table joined to the ones before it, matching columns pairwise (`ANDed`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    /// `(column of an earlier table, column of this one)`.
    pub on: Vec<(ColumnRef, ColumnRef)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

impl Aggregate {
    pub const ALL: [Self; 6] = [
        Self::Count,
        Self::CountDistinct,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::CountDistinct => "COUNT DISTINCT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
        }
    }

    fn apply(self, expr: &str) -> String {
        match self {
            Self::CountDistinct => format!("COUNT(DISTINCT {expr})"),
            other => format!("{}({expr})", other.label()),
        }
    }
}

/// An output column: a table column, optionally aggregated and renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub column: ColumnRef,
    pub aggregate: Option<Aggregate>,
    /// `AS` name; empty for none.
    pub alias: String,
}

impl SelectItem {
    pub fn column(column: ColumnRef) -> Self {
        Self {
            column,
            aggregate: None,
            alias: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Operator {
    #[default]
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    In,
    NotIn,
    IsNull,
    IsNotNull,
}

impl Operator {
    pub const ALL: [Self; 12] = [
        Self::Eq,
        Self::Ne,
        Self::Lt,
        Self::Le,
        Self::Gt,
        Self::Ge,
        Self::Like,
        Self::NotLike,
        Self::In,
        Self::NotIn,
        Self::IsNull,
        Self::IsNotNull,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Like => "LIKE",
            Self::NotLike => "NOT LIKE",
            Self::In => "IN",
            Self::NotIn => "NOT IN",
            Self::IsNull => "IS NULL",
            Self::IsNotNull => "IS NOT NULL",
        }
    }

    /// Whether the operator compares with a value.
    pub fn takes_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }
}

/// `column operator value`, `ANDed` with the others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Condition {
    pub column: ColumnRef,
    pub operator: Operator,
    /// As typed: numbers and `NULL` are used as they are, text is quoted, and anything
    /// starting with `=` is raw SQL (`=now()`). For `IN`, a comma-separated list.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderItem {
    pub column: ColumnRef,
    pub descending: bool,
}

/// One `SELECT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectPart {
    pub from: Option<TableRef>,
    pub joins: Vec<Join>,
    /// Empty selects every column (`*`).
    pub columns: Vec<SelectItem>,
    pub distinct: bool,
    pub conditions: Vec<Condition>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<u64>,
}

impl SelectPart {
    /// Every table in the `SELECT`, `FROM` first.
    pub fn tables(&self) -> Vec<&TableRef> {
        self.from
            .iter()
            .chain(self.joins.iter().map(|j| &j.table))
            .collect()
    }

    /// An alias for `table` not used yet: its initials (`order_items` → `oi`), numbered if
    /// taken.
    pub fn new_alias(&self, table: &str) -> String {
        let initials: String = table
            .split(['_', ' ', '-'])
            .filter_map(|word| word.chars().next())
            .flat_map(char::to_lowercase)
            .collect();
        let base = if initials
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && initials.starts_with(|c: char| c.is_ascii_alphabetic())
        {
            initials
        } else {
            "t".to_owned()
        };
        let taken = |a: &str| self.tables().iter().any(|t| t.alias == a);
        if !taken(&base) {
            return base;
        }
        // There are fewer tables than numbers, so one is free.
        (2..=self.tables().len() + 2)
            .map(|n| format!("{base}{n}"))
            .find(|a| !taken(a))
            .unwrap_or(base)
    }

    /// Adds `table` to the `SELECT`: as `FROM` when it's the first, otherwise joined. The join
    /// follows a foreign key between it and a table already there when there is one (`fks`
    /// maps each table to its keys); otherwise it's an `INNER JOIN` with no columns matched yet.
    pub fn add_table(&mut self, table: &str, fks: &dyn Fn(&str) -> Vec<ForeignKeyInfo>) {
        let alias = self.new_alias(table);
        let added = TableRef {
            table: table.to_owned(),
            alias,
        };
        if self.from.is_none() {
            self.from = Some(added);
            return;
        }
        let on = self.suggest_on(&added, fks);
        self.joins.push(Join {
            kind: JoinKind::Inner,
            table: added,
            on,
        });
    }

    /// Column pairs joining `new` to an existing table through a foreign key, either one of
    /// `new`'s keys pointing at an existing table, or an existing table's key pointing at
    /// `new`. The most recently added match wins.
    fn suggest_on(
        &self,
        new: &TableRef,
        fks: &dyn Fn(&str) -> Vec<ForeignKeyInfo>,
    ) -> Vec<(ColumnRef, ColumnRef)> {
        let pairs = |from: &TableRef, fk: &ForeignKeyInfo, to: &TableRef, reverse: bool| {
            fk.columns
                .iter()
                .zip(&fk.ref_columns)
                .map(|(col, ref_col)| {
                    let here = ColumnRef::new(&from.alias, col);
                    let there = ColumnRef::new(&to.alias, ref_col);
                    // Earlier table first.
                    if reverse {
                        (there, here)
                    } else {
                        (here, there)
                    }
                })
                .collect::<Vec<_>>()
        };
        for existing in self.tables().into_iter().rev() {
            if let Some(fk) = fks(&new.table)
                .iter()
                .find(|fk| fk.ref_table == existing.table)
            {
                return pairs(new, fk, existing, true);
            }
            if let Some(fk) = fks(&existing.table)
                .iter()
                .find(|fk| fk.ref_table == new.table)
            {
                return pairs(existing, fk, new, false);
            }
        }
        Vec::new()
    }

    /// Takes out the table under `alias` and everything that refers to it. Removing the `FROM`
    /// table moves the first join up.
    pub fn remove_table(&mut self, alias: &str) {
        if self.from.as_ref().is_some_and(|t| t.alias == alias) {
            self.from = if self.joins.is_empty() {
                None
            } else {
                Some(self.joins.remove(0).table)
            };
        } else {
            self.joins.retain(|j| j.table.alias != alias);
        }
        for join in &mut self.joins {
            join.on
                .retain(|(a, b)| a.alias != alias && b.alias != alias);
        }
        self.columns.retain(|c| c.column.alias != alias);
        self.conditions.retain(|c| c.column.alias != alias);
        self.order_by.retain(|o| o.column.alias != alias);
    }

    fn sql(&self, d: Dialect, db: &str, indent: &str) -> Result<String, String> {
        let from = self.from.as_ref().ok_or("Add a table to select from.")?;
        let mut sql = String::from("SELECT");
        if self.distinct {
            sql.push_str(" DISTINCT");
        }
        let aggregated = self.columns.iter().any(|c| c.aggregate.is_some());
        if self.columns.is_empty() {
            sql.push_str(" *");
        } else {
            let items: Vec<String> = self
                .columns
                .iter()
                .map(|item| {
                    let column = item.column.sql(d);
                    let mut out = item.aggregate.map_or(column.clone(), |a| a.apply(&column));
                    if !item.alias.trim().is_empty() {
                        out.push_str(" AS ");
                        out.push_str(&d.quote_ident(item.alias.trim()));
                    }
                    out
                })
                .collect();
            let _ = write!(sql, "\n{indent}  {}", items.join(&format!(",\n{indent}  ")));
        }
        let table = |t: &TableRef| {
            format!(
                "{} AS {}",
                d.qualified(db, &t.table),
                d.quote_ident(&t.alias)
            )
        };
        let _ = write!(sql, "\n{indent}FROM {}", table(from));
        for join in &self.joins {
            let _ = write!(
                sql,
                "\n{indent}  {} {}",
                join.kind.keyword(),
                table(&join.table)
            );
            if join.kind == JoinKind::Cross {
                continue;
            }
            if join.on.is_empty() {
                return Err(format!(
                    "Choose the columns that join {} ({}).",
                    join.table.table, join.table.alias
                ));
            }
            let on: Vec<String> = join
                .on
                .iter()
                .map(|(a, b)| format!("{} = {}", a.sql(d), b.sql(d)))
                .collect();
            let _ = write!(sql, " ON {}", on.join(" AND "));
        }
        if !self.conditions.is_empty() {
            let conditions = self
                .conditions
                .iter()
                .map(|c| condition_sql(d, c))
                .collect::<Result<Vec<_>, _>>()?;
            let _ = write!(
                sql,
                "\n{indent}WHERE {}",
                conditions.join(&format!("\n{indent}  AND "))
            );
        }
        // Columns shown next to aggregates are grouped by.
        if aggregated {
            let grouped: Vec<String> = self
                .columns
                .iter()
                .filter(|c| c.aggregate.is_none())
                .map(|c| c.column.sql(d))
                .collect();
            if !grouped.is_empty() {
                let _ = write!(sql, "\n{indent}GROUP BY {}", grouped.join(", "));
            }
        }
        sql.push_str(&order_and_limit(d, &self.order_by, self.limit, indent));
        Ok(sql)
    }
}

/// `ORDER BY …` and `LIMIT n`, each on its own line, or nothing.
fn order_and_limit(d: Dialect, order: &[OrderItem], limit: Option<u64>, indent: &str) -> String {
    let mut sql = String::new();
    if !order.is_empty() {
        let items: Vec<String> = order
            .iter()
            .map(|o| {
                format!(
                    "{}{}",
                    o.column.sql(d),
                    if o.descending { " DESC" } else { "" }
                )
            })
            .collect();
        let _ = write!(sql, "\n{indent}ORDER BY {}", items.join(", "));
    }
    if let Some(limit) = limit {
        let _ = write!(sql, "\n{indent}LIMIT {limit}");
    }
    sql
}

/// A value as SQL: see [`Condition::value`].
fn value_sql(d: Dialect, value: &str) -> String {
    let value = value.trim();
    if let Some(raw) = value.strip_prefix('=') {
        return raw.trim().to_owned();
    }
    if value.eq_ignore_ascii_case("null") || value.parse::<f64>().is_ok_and(f64::is_finite) {
        return value.to_owned();
    }
    // Already quoted by the user.
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value.to_owned();
    }
    d.quote_str(value)
}

fn condition_sql(d: Dialect, c: &Condition) -> Result<String, String> {
    let column = c.column.sql(d);
    let op = c.operator;
    if !op.takes_value() {
        return Ok(format!("{column} {}", op.label()));
    }
    if c.value.trim().is_empty() {
        return Err(format!(
            "Give a value to compare {} with.",
            c.column.label()
        ));
    }
    if matches!(op, Operator::In | Operator::NotIn) {
        let items: Vec<String> = c
            .value
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| value_sql(d, v))
            .collect();
        return Ok(format!("{column} {} ({})", op.label(), items.join(", ")));
    }
    Ok(format!(
        "{column} {} {}",
        op.label(),
        value_sql(d, &c.value)
    ))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SetOperator {
    #[default]
    Union,
    UnionAll,
    Intersect,
    Except,
}

impl SetOperator {
    pub const ALL: [Self; 4] = [Self::Union, Self::UnionAll, Self::Intersect, Self::Except];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Union => "UNION",
            Self::UnionAll => "UNION ALL",
            Self::Intersect => "INTERSECT",
            Self::Except => "EXCEPT",
        }
    }
}

/// A `SELECT`, or several combined with set operators, read left to right.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub first: SelectPart,
    pub rest: Vec<(SetOperator, SelectPart)>,
    /// For a combined query: sorting by output column position (1-based) and a row limit,
    /// applied to the combined result.
    pub order_by: Vec<(usize, bool)>,
    pub limit: Option<u64>,
}

impl Query {
    /// The statement for tables in `db`, or what's missing.
    pub fn sql(&self, d: Dialect, db: &str) -> Result<String, String> {
        if self.rest.is_empty() {
            return self.first.sql(d, db, "");
        }
        let parts: Vec<&SelectPart> = std::iter::once(&self.first)
            .chain(self.rest.iter().map(|(_, p)| p))
            .collect();
        // Per-part sorting and limits would need parentheses, which SQLite doesn't accept.
        if parts
            .iter()
            .any(|p| !p.order_by.is_empty() || p.limit.is_some())
        {
            return Err(
                "In a combined query, sort and limit the whole result, not each part.".into(),
            );
        }
        let widths: Vec<usize> = parts.iter().map(|p| p.columns.len()).collect();
        if widths.contains(&0) && widths.iter().any(|&w| w != 0) {
            return Err("Combined queries must all choose their columns, or all select *.".into());
        }
        if widths.windows(2).any(|w| w[0] != w[1]) {
            return Err(format!(
                "Combined queries need the same number of columns (they have {}).",
                widths
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut sql = self.first.sql(d, db, "")?;
        for (op, part) in &self.rest {
            let _ = write!(sql, "\n{}\n{}", op.keyword(), part.sql(d, db, "")?);
        }
        if !self.order_by.is_empty() {
            let width = widths[0];
            let items: Vec<String> = self
                .order_by
                .iter()
                .map(|&(position, descending)| {
                    if width > 0 && !(1..=width).contains(&position) {
                        return Err(format!("There's no column {position} to sort by."));
                    }
                    Ok(format!(
                        "{position}{}",
                        if descending { " DESC" } else { "" }
                    ))
                })
                .collect::<Result<_, _>>()?;
            let _ = write!(sql, "\nORDER BY {}", items.join(", "));
        }
        if let Some(limit) = self.limit {
            let _ = write!(sql, "\nLIMIT {limit}");
        }
        Ok(sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Dialect = Dialect::MySql;
    const P: Dialect = Dialect::Postgres;

    fn fk(columns: &[&str], ref_table: &str, ref_columns: &[&str]) -> ForeignKeyInfo {
        ForeignKeyInfo {
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            ref_table: ref_table.to_owned(),
            ref_columns: ref_columns.iter().map(|c| (*c).to_owned()).collect(),
            ..ForeignKeyInfo::default()
        }
    }

    /// `orders` → `customers`, `order_items` → `orders` and `products`.
    fn shop_fks(table: &str) -> Vec<ForeignKeyInfo> {
        match table {
            "orders" => vec![fk(&["customer_id"], "customers", &["id"])],
            "order_items" => vec![
                fk(&["order_id"], "orders", &["id"]),
                fk(&["product_id"], "products", &["id"]),
            ],
            _ => Vec::new(),
        }
    }

    #[test]
    fn aliases_are_initials_and_unique() {
        let mut part = SelectPart::default();
        assert_eq!(part.new_alias("order_items"), "oi");
        assert_eq!(part.new_alias("Customers"), "c");
        assert_eq!(part.new_alias("2024 sales"), "t");
        part.add_table("orders", &shop_fks);
        part.add_table("orders", &shop_fks);
        part.add_table("options", &shop_fks);
        let aliases: Vec<&str> = part.tables().iter().map(|t| t.alias.as_str()).collect();
        assert_eq!(aliases, ["o", "o2", "o3"]);
    }

    #[test]
    fn joins_follow_foreign_keys_both_ways() {
        let mut part = SelectPart::default();
        part.add_table("orders", &shop_fks);
        // customers is referenced by orders.
        part.add_table("customers", &shop_fks);
        // order_items references orders.
        part.add_table("order_items", &shop_fks);
        // products is referenced by order_items, the latest table.
        part.add_table("products", &shop_fks);
        // nothing relates notes to the others.
        part.add_table("notes", &shop_fks);
        let on: Vec<Vec<String>> = part
            .joins
            .iter()
            .map(|j| {
                j.on.iter()
                    .map(|(a, b)| format!("{} = {}", a.label(), b.label()))
                    .collect()
            })
            .collect();
        assert_eq!(
            on,
            [
                vec!["o.customer_id = c.id"],
                vec!["o.id = oi.order_id"],
                vec!["oi.product_id = p.id"],
                vec![],
            ]
        );
        assert!(part.sql(M, "shop", "").unwrap_err().contains("notes (n)"));
        part.joins[3].kind = JoinKind::Cross;
        let sql = part.sql(M, "shop", "").unwrap();
        assert_eq!(
            sql,
            "SELECT *\nFROM `shop`.`orders` AS `o`\
             \n  INNER JOIN `shop`.`customers` AS `c` ON `o`.`customer_id` = `c`.`id`\
             \n  INNER JOIN `shop`.`order_items` AS `oi` ON `o`.`id` = `oi`.`order_id`\
             \n  INNER JOIN `shop`.`products` AS `p` ON `oi`.`product_id` = `p`.`id`\
             \n  CROSS JOIN `shop`.`notes` AS `n`"
        );
    }

    #[test]
    fn columns_filters_grouping_sorting() {
        let mut part = SelectPart::default();
        part.add_table("orders", &shop_fks);
        part.add_table("customers", &shop_fks);
        part.joins[0].kind = JoinKind::Left;
        part.distinct = true;
        part.columns = vec![
            SelectItem {
                alias: "customer".into(),
                ..SelectItem::column(ColumnRef::new("c", "name"))
            },
            SelectItem {
                aggregate: Some(Aggregate::Sum),
                alias: "spent".into(),
                ..SelectItem::column(ColumnRef::new("o", "total"))
            },
            SelectItem {
                aggregate: Some(Aggregate::CountDistinct),
                ..SelectItem::column(ColumnRef::new("o", "id"))
            },
        ];
        part.conditions = vec![
            Condition {
                column: ColumnRef::new("o", "total"),
                operator: Operator::Gt,
                value: "100".into(),
            },
            Condition {
                column: ColumnRef::new("c", "name"),
                operator: Operator::Like,
                value: "O'Brien%".into(),
            },
            Condition {
                column: ColumnRef::new("o", "status"),
                operator: Operator::In,
                value: "paid, 'sent', 3".into(),
            },
            Condition {
                column: ColumnRef::new("o", "placed"),
                operator: Operator::Ge,
                value: "= now() - interval '1 day'".into(),
            },
            Condition {
                column: ColumnRef::new("c", "email"),
                operator: Operator::IsNotNull,
                value: "ignored".into(),
            },
        ];
        part.order_by = vec![OrderItem {
            column: ColumnRef::new("c", "name"),
            descending: true,
        }];
        part.limit = Some(50);
        assert_eq!(
            Query {
                first: part,
                ..Query::default()
            }
            .sql(P, "public")
            .unwrap(),
            "SELECT DISTINCT\
             \n  \"c\".\"name\" AS \"customer\",\
             \n  SUM(\"o\".\"total\") AS \"spent\",\
             \n  COUNT(DISTINCT \"o\".\"id\")\
             \nFROM \"public\".\"orders\" AS \"o\"\
             \n  LEFT JOIN \"public\".\"customers\" AS \"c\" ON \"o\".\"customer_id\" = \"c\".\"id\"\
             \nWHERE \"o\".\"total\" > 100\
             \n  AND \"c\".\"name\" LIKE 'O''Brien%'\
             \n  AND \"o\".\"status\" IN ('paid', 'sent', 3)\
             \n  AND \"o\".\"placed\" >= now() - interval '1 day'\
             \n  AND \"c\".\"email\" IS NOT NULL\
             \nGROUP BY \"c\".\"name\"\
             \nORDER BY \"c\".\"name\" DESC\
             \nLIMIT 50"
        );
    }

    #[test]
    fn values_are_quoted_unless_numbers_null_or_raw() {
        assert_eq!(value_sql(M, "42"), "42");
        assert_eq!(value_sql(M, "-1.5e3"), "-1.5e3");
        assert_eq!(value_sql(M, "NULL"), "NULL");
        assert_eq!(value_sql(M, "'kept'"), "'kept'");
        assert_eq!(value_sql(M, "it's"), "'it\\'s'");
        assert_eq!(value_sql(M, "=CURDATE()"), "CURDATE()");
        assert_eq!(value_sql(M, "inf"), "'inf'");
        let empty = Condition {
            column: ColumnRef::new("o", "x"),
            ..Condition::default()
        };
        assert!(condition_sql(M, &empty).unwrap_err().contains("o.x"));
    }

    #[test]
    fn removing_tables_cleans_up() {
        let mut part = SelectPart::default();
        part.add_table("orders", &shop_fks);
        part.add_table("customers", &shop_fks);
        part.add_table("order_items", &shop_fks);
        part.columns = vec![
            SelectItem::column(ColumnRef::new("c", "name")),
            SelectItem::column(ColumnRef::new("oi", "qty")),
        ];
        part.remove_table("o");
        // customers becomes the FROM table; the join that used orders loses its columns.
        assert_eq!(part.from.as_ref().unwrap().table, "customers");
        assert_eq!(part.joins.len(), 1);
        assert_eq!(part.joins[0].on.len(), 0);
        part.remove_table("oi");
        assert_eq!(part.columns.len(), 1);
        part.remove_table("c");
        assert!(part.from.is_none());
        assert_eq!(part.columns.len(), 0);
    }

    #[test]
    fn unions_check_their_parts() {
        let part = |table: &str, columns: &[&str]| {
            let mut p = SelectPart::default();
            p.add_table(table, &shop_fks);
            let alias = p.from.as_ref().unwrap().alias.clone();
            p.columns = columns
                .iter()
                .map(|c| SelectItem::column(ColumnRef::new(&alias, c)))
                .collect();
            p
        };
        let mut query = Query {
            first: part("customers", &["name", "email"]),
            rest: vec![(SetOperator::UnionAll, part("suppliers", &["name", "email"]))],
            order_by: vec![(1, false), (2, true)],
            limit: Some(10),
        };
        assert_eq!(
            query.sql(Dialect::Sqlite, "main").unwrap(),
            "SELECT\n  \"c\".\"name\",\n  \"c\".\"email\"\nFROM \"main\".\"customers\" AS \"c\"\
             \nUNION ALL\
             \nSELECT\n  \"s\".\"name\",\n  \"s\".\"email\"\nFROM \"main\".\"suppliers\" AS \"s\"\
             \nORDER BY 1, 2 DESC\nLIMIT 10"
        );
        query.order_by = vec![(3, false)];
        assert!(query.sql(M, "shop").unwrap_err().contains("column 3"));
        query.order_by.clear();
        query.rest[0].1.columns.pop();
        assert!(query.sql(M, "shop").unwrap_err().contains("2, 1"));
        query.rest[0].1.columns.clear();
        assert!(query.sql(M, "shop").unwrap_err().contains("all select *"));
        query.rest[0].1 = part("suppliers", &["name", "email"]);
        query.rest[0].1.limit = Some(5);
        assert!(query.sql(M, "shop").unwrap_err().contains("whole result"));
        assert!(
            Query::default()
                .sql(M, "shop")
                .unwrap_err()
                .contains("Add a table")
        );
    }

    #[test]
    fn mysql_has_no_full_join() {
        assert!(!JoinKind::available(M).contains(&JoinKind::Full));
        assert!(JoinKind::available(P).contains(&JoinKind::Full));
    }
}
