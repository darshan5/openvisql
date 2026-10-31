//! PostgreSQL roles and privileges: reading them from the catalogs, and the `CREATE ROLE`,
//! `ALTER ROLE`, `GRANT`, `REVOKE` and `ALTER DEFAULT PRIVILEGES` statements that change them.
//!
//! Roles and memberships belong to the whole server. A connection is to one database, so
//! privileges on schemas and tables are those of that database.

use std::collections::{BTreeMap, BTreeSet};

use super::dialect::Dialect;
use super::users::Grants;
use crate::result::ResultSet;
use crate::value::Value;

const D: Dialect = Dialect::Postgres;

/// A role and its attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub login: bool,
    pub superuser: bool,
    pub create_db: bool,
    pub create_role: bool,
    /// Uses the privileges of the roles it's a member of without `SET ROLE`.
    pub inherit: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    /// `None` for no limit.
    pub connection_limit: Option<u32>,
    /// When the password stops working, as PostgreSQL writes it; `None` for never.
    pub valid_until: Option<String>,
    /// The roles this one is a member of.
    pub member_of: BTreeSet<String>,
}

impl Default for Role {
    /// What `CREATE ROLE` gives without options.
    fn default() -> Self {
        Self {
            name: String::new(),
            login: false,
            superuser: false,
            create_db: false,
            create_role: false,
            inherit: true,
            replication: false,
            bypass_rls: false,
            connection_limit: None,
            valid_until: None,
            member_of: BTreeSet::new(),
        }
    }
}

/// PostgreSQL's built-in roles (`pg_read_all_data`, `pg_monitor`, …). They can be granted but
/// not changed.
pub fn is_predefined(name: &str) -> bool {
    name.starts_with("pg_")
}

pub const LIST_ROLES: &str = "SELECT rolname, rolcanlogin::text, rolsuper::text, \
    rolcreatedb::text, rolcreaterole::text, rolinherit::text, rolreplication::text, \
    rolbypassrls::text, rolconnlimit::text, rolvaliduntil::text FROM pg_roles ORDER BY rolname";

/// (member, role) pairs.
pub const LIST_MEMBERSHIPS: &str = "SELECT m.rolname, r.rolname FROM pg_auth_members a \
    JOIN pg_roles m ON m.oid = a.member JOIN pg_roles r ON r.oid = a.roleid ORDER BY 1, 2";

fn text(row: &[Value], i: usize) -> Option<String> {
    row.get(i).filter(|v| !v.is_null()).map(ToString::to_string)
}

fn flag(row: &[Value], i: usize) -> bool {
    text(row, i).is_some_and(|t| t == "true" || t == "t")
}

/// Reads [`LIST_ROLES`] and [`LIST_MEMBERSHIPS`].
pub fn parse_roles(roles: &ResultSet, memberships: &ResultSet) -> Vec<Role> {
    let mut member_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &memberships.rows {
        if let (Some(member), Some(role)) = (text(row, 0), text(row, 1)) {
            member_of.entry(member).or_default().insert(role);
        }
    }
    roles
        .rows
        .iter()
        .map(|row| {
            let name = text(row, 0).unwrap_or_default();
            Role {
                login: flag(row, 1),
                superuser: flag(row, 2),
                create_db: flag(row, 3),
                create_role: flag(row, 4),
                inherit: flag(row, 5),
                replication: flag(row, 6),
                bypass_rls: flag(row, 7),
                connection_limit: text(row, 8).and_then(|t| t.parse().ok()),
                valid_until: text(row, 9).filter(|t| t != "infinity"),
                member_of: member_of.remove(&name).unwrap_or_default(),
                name,
            }
        })
        .collect()
}

/// The attribute options that turn `from` into `to`, as `CREATE ROLE` / `ALTER ROLE` take
/// them. The password is added separately.
fn attribute_options(from: &Role, to: &Role) -> Vec<String> {
    let mut out = Vec::new();
    let mut toggle = |before: bool, after: bool, on: &str| {
        if before != after {
            out.push(if after {
                on.to_owned()
            } else {
                format!("NO{on}")
            });
        }
    };
    toggle(from.login, to.login, "LOGIN");
    toggle(from.superuser, to.superuser, "SUPERUSER");
    toggle(from.create_db, to.create_db, "CREATEDB");
    toggle(from.create_role, to.create_role, "CREATEROLE");
    toggle(from.inherit, to.inherit, "INHERIT");
    toggle(from.replication, to.replication, "REPLICATION");
    toggle(from.bypass_rls, to.bypass_rls, "BYPASSRLS");
    if from.connection_limit != to.connection_limit {
        let limit = to.connection_limit.map_or(-1, i64::from);
        out.push(format!("CONNECTION LIMIT {limit}"));
    }
    if from.valid_until != to.valid_until {
        let until = to.valid_until.as_deref().unwrap_or("infinity");
        out.push(format!("VALID UNTIL {}", D.quote_str(until)));
    }
    out
}

/// `CREATE ROLE` (for a new role) or `ALTER ROLE … RENAME` and `ALTER ROLE … WITH`, then the
/// membership changes. A password always comes last in its statement, so it can be hidden.
pub fn role_statements(
    original: Option<&Role>,
    role: &Role,
    password: Option<&str>,
) -> Vec<String> {
    let name = D.quote_ident(&role.name);
    let mut out = Vec::new();
    let base = Role::default();
    let mut options = attribute_options(original.unwrap_or(&base), role);
    if let Some(password) = password {
        options.push(format!("PASSWORD {}", D.quote_str(password)));
    }
    let with = |options: &[String]| format!(" WITH {}", options.join(" "));
    match original {
        None => out.push(format!(
            "CREATE ROLE {name}{}",
            if options.is_empty() {
                String::new()
            } else {
                with(&options)
            }
        )),
        Some(original) => {
            if original.name != role.name {
                out.push(format!(
                    "ALTER ROLE {} RENAME TO {name}",
                    D.quote_ident(&original.name)
                ));
            }
            if !options.is_empty() {
                out.push(format!("ALTER ROLE {name}{}", with(&options)));
            }
        }
    }
    let empty = BTreeSet::new();
    let before = original.map_or(&empty, |r| &r.member_of);
    let list = |names: Vec<&String>| {
        names
            .into_iter()
            .map(|n| D.quote_ident(n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let removed: Vec<&String> = before.difference(&role.member_of).collect();
    if !removed.is_empty() {
        out.push(format!("REVOKE {} FROM {name}", list(removed)));
    }
    let added: Vec<&String> = role.member_of.difference(before).collect();
    if !added.is_empty() {
        out.push(format!("GRANT {} TO {name}", list(added)));
    }
    out
}

pub fn drop_role(name: &str) -> String {
    format!("DROP ROLE {}", D.quote_ident(name))
}

/// Drops a role that may still own objects or hold privileges: what it owns in this database
/// goes to the current user, its privileges here are revoked, then the role is dropped. That
/// fails if it still owns or holds something in another database.
pub fn drop_role_statements(name: &str) -> Vec<String> {
    let role = D.quote_ident(name);
    vec![
        format!("REASSIGN OWNED BY {role} TO CURRENT_USER"),
        format!("DROP OWNED BY {role}"),
        drop_role(name),
    ]
}

/// Who privileges are granted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grantee {
    /// Every role, including ones created later.
    Public,
    Role(String),
}

impl Grantee {
    fn sql(&self) -> String {
        match self {
            Self::Public => "PUBLIC".to_owned(),
            Self::Role(name) => D.quote_ident(name),
        }
    }

    /// The grantee's oid, as `aclexplode` reports it (0 for PUBLIC).
    fn oid(&self) -> String {
        match self {
            Self::Public => "0".to_owned(),
            Self::Role(name) => format!(
                "(SELECT oid FROM pg_roles WHERE rolname = {})",
                D.quote_str(name)
            ),
        }
    }
}

/// What privileges are granted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The connected database.
    Database(String),
    Schema(String),
    /// A table, view, materialized view or foreign table.
    Table {
        schema: String,
        name: String,
    },
    /// Every table (and view) in a schema.
    AllTables(String),
    /// Every sequence in a schema.
    AllSequences(String),
}

impl Target {
    /// Every object in the schema rather than one object: shown as "on all", with default
    /// privileges for objects created later.
    pub fn is_schema_wide(&self) -> bool {
        matches!(self, Self::AllTables(_) | Self::AllSequences(_))
    }

    fn sql(&self) -> String {
        match self {
            Self::Database(db) => format!("DATABASE {}", D.quote_ident(db)),
            Self::Schema(schema) => format!("SCHEMA {}", D.quote_ident(schema)),
            Self::Table { schema, name } => {
                format!("TABLE {}.{}", D.quote_ident(schema), D.quote_ident(name))
            }
            Self::AllTables(schema) => format!("ALL TABLES IN SCHEMA {}", D.quote_ident(schema)),
            Self::AllSequences(schema) => {
                format!("ALL SEQUENCES IN SCHEMA {}", D.quote_ident(schema))
            }
        }
    }

    /// (catalog rows, ACL expression) for the objects this covers.
    fn objects(&self) -> (String, &'static str) {
        let relations = |filter: String, kind: char| {
            (
                format!("pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE {filter}"),
                if kind == 's' {
                    "coalesce(c.relacl, acldefault('s', c.relowner))"
                } else {
                    "coalesce(c.relacl, acldefault('r', c.relowner))"
                },
            )
        };
        let in_schema = |schema: &str| format!("n.nspname = {}", D.quote_str(schema));
        match self {
            Self::Database(db) => (
                format!("pg_database d WHERE d.datname = {}", D.quote_str(db)),
                "coalesce(d.datacl, acldefault('d', d.datdba))",
            ),
            Self::Schema(schema) => (
                format!("pg_namespace n WHERE {}", in_schema(schema)),
                "coalesce(n.nspacl, acldefault('n', n.nspowner))",
            ),
            Self::Table { schema, name } => relations(
                format!(
                    "{} AND c.relname = {} AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
                    in_schema(schema),
                    D.quote_str(name)
                ),
                'r',
            ),
            Self::AllTables(schema) => relations(
                format!(
                    "{} AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
                    in_schema(schema)
                ),
                'r',
            ),
            Self::AllSequences(schema) => {
                relations(format!("{} AND c.relkind = 'S'", in_schema(schema)), 's')
            }
        }
    }

    /// `ALTER DEFAULT PRIVILEGES` object type and catalog code, for schema-wide targets.
    fn default_kind(&self) -> Option<(&'static str, char)> {
        match self {
            Self::AllTables(_) => Some(("TABLES", 'r')),
            Self::AllSequences(_) => Some(("SEQUENCES", 'S')),
            _ => None,
        }
    }
}

const DATABASE_PRIVILEGES: [&str; 3] = ["CONNECT", "CREATE", "TEMPORARY"];
const SCHEMA_PRIVILEGES: [&str; 2] = ["USAGE", "CREATE"];
const TABLE_PRIVILEGES: [&str; 7] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];
const SEQUENCE_PRIVILEGES: [&str; 3] = ["USAGE", "SELECT", "UPDATE"];

/// The privileges that can be granted on `target`, in display order.
pub fn privileges_for(target: &Target) -> &'static [&'static str] {
    match target {
        Target::Database(_) => &DATABASE_PRIVILEGES,
        Target::Schema(_) => &SCHEMA_PRIVILEGES,
        Target::Table { .. } | Target::AllTables(_) => &TABLE_PRIVILEGES,
        Target::AllSequences(_) => &SEQUENCE_PRIVILEGES,
    }
}

/// One row per privilege `grantee` holds on any of the target's objects: the privilege, on
/// how many objects, whether it's grantable on all of them, and how many objects there are.
pub fn privileges_query(grantee: &Grantee, target: &Target) -> String {
    let (objects, acl) = target.objects();
    let (from, filter) = objects
        .split_once(" WHERE ")
        .expect("object queries have a filter");
    format!(
        "SELECT a.privilege_type, count(*)::text, bool_and(a.is_grantable)::text, \
         (SELECT count(*) FROM {objects})::text \
         FROM {from}, aclexplode({acl}) a WHERE {filter} AND a.grantee = {} GROUP BY 1",
        grantee.oid()
    )
}

/// Privileges on a target, as read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Privileges {
    /// Held on every object of the target (for a single object, simply held).
    pub grants: Grants,
    /// Held on some objects of a schema-wide target but not all: privilege → how many.
    pub partial: BTreeMap<String, usize>,
    /// How many objects the target covers.
    pub objects: usize,
}

/// Reads [`privileges_query`].
pub fn parse_privileges(rs: &ResultSet, target: &Target) -> Privileges {
    let allowed = privileges_for(target);
    let mut out = Privileges::default();
    let mut grantable = true;
    for row in &rs.rows {
        let Some(name) = text(row, 0) else { continue };
        let count: usize = text(row, 1).and_then(|t| t.parse().ok()).unwrap_or(0);
        out.objects = text(row, 3).and_then(|t| t.parse().ok()).unwrap_or(0);
        // MAINTAIN (PostgreSQL 17) and the like aren't offered.
        if !allowed.contains(&name.as_str()) {
            continue;
        }
        if count >= out.objects {
            grantable &= flag(row, 2);
            out.grants.privileges.insert(name);
        } else {
            out.partial.insert(name, count);
        }
    }
    out.grants.grant_option = grantable && !out.grants.privileges.is_empty();
    out
}

/// The default privileges `grantee` gets on objects the current user creates later in a
/// schema-wide target's schema. `None` for other targets.
pub fn default_privileges_query(grantee: &Grantee, target: &Target) -> Option<String> {
    let (Target::AllTables(schema) | Target::AllSequences(schema)) = target else {
        return None;
    };
    let (_, code) = target.default_kind()?;
    Some(format!(
        "SELECT a.privilege_type, '1', a.is_grantable::text, '1' FROM pg_default_acl d \
         JOIN pg_namespace n ON n.oid = d.defaclnamespace, aclexplode(d.defaclacl) a \
         WHERE n.nspname = {} AND d.defaclobjtype = '{code}' \
         AND d.defaclrole = (SELECT oid FROM pg_roles WHERE rolname = current_user) \
         AND a.grantee = {}",
        D.quote_str(schema),
        grantee.oid()
    ))
}

/// Reads [`default_privileges_query`].
pub fn parse_default_privileges(rs: &ResultSet, target: &Target) -> Grants {
    parse_privileges(rs, target).grants
}

/// `REVOKE` and `GRANT` turning `before` into `after`, with `on` naming the objects and
/// `prefix` put in front of each statement.
fn changes(
    prefix: &str,
    on: &str,
    grantee: &Grantee,
    before: &Grants,
    after: &Grants,
) -> Vec<String> {
    let who = grantee.sql();
    let mut out = Vec::new();
    let removed: Vec<&str> = before
        .privileges
        .difference(&after.privileges)
        .map(String::as_str)
        .collect();
    if !removed.is_empty() {
        out.push(format!(
            "{prefix}REVOKE {} ON {on} FROM {who}",
            removed.join(", ")
        ));
    }
    let kept: Vec<&str> = before
        .privileges
        .intersection(&after.privileges)
        .map(String::as_str)
        .collect();
    if before.grant_option && !after.grant_option && !kept.is_empty() {
        out.push(format!(
            "{prefix}REVOKE GRANT OPTION FOR {} ON {on} FROM {who}",
            kept.join(", ")
        ));
    }
    let option = after.grant_option && *grantee != Grantee::Public;
    // Turning the grant option on re-grants everything with it.
    let to_grant: Vec<&str> = if option && !before.grant_option {
        after.privileges.iter().map(String::as_str).collect()
    } else {
        after
            .privileges
            .difference(&before.privileges)
            .map(String::as_str)
            .collect()
    };
    if !to_grant.is_empty() {
        out.push(format!(
            "{prefix}GRANT {} ON {on} TO {who}{}",
            to_grant.join(", "),
            if option { " WITH GRANT OPTION" } else { "" }
        ));
    }
    out
}

/// The statements turning `before` into `after` on `target`.
pub fn grant_statements(
    grantee: &Grantee,
    target: &Target,
    before: &Grants,
    after: &Grants,
) -> Vec<String> {
    changes("", &target.sql(), grantee, before, after)
}

/// The `ALTER DEFAULT PRIVILEGES` statements turning `before` into `after` for objects the
/// current user creates later in a schema-wide target's schema.
pub fn default_privilege_statements(
    grantee: &Grantee,
    target: &Target,
    before: &Grants,
    after: &Grants,
) -> Vec<String> {
    let (Target::AllTables(schema) | Target::AllSequences(schema)) = target else {
        return Vec::new();
    };
    let Some((kind, _)) = target.default_kind() else {
        return Vec::new();
    };
    let prefix = format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA {} ",
        D.quote_ident(schema)
    );
    changes(&prefix, kind, grantee, before, after)
}

/// A statement with any password in it hidden, for showing.
pub fn mask_password(sql: &str) -> String {
    match sql.find(" PASSWORD '") {
        Some(at) => format!("{} PASSWORD '…'", &sql[..at]),
        None => sql.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn grants(items: &[&str], grant_option: bool) -> Grants {
        Grants {
            privileges: set(items),
            grant_option,
        }
    }

    fn rs(columns: usize, rows: &[&[Option<&str>]]) -> ResultSet {
        ResultSet {
            columns: (0..columns)
                .map(|i| Column {
                    name: format!("c{i}"),
                    ..Column::default()
                })
                .collect(),
            rows: rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| v.map_or(Value::Null, |t| Value::Text(t.to_owned())))
                        .collect()
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn creates_a_role_with_only_the_options_that_differ() {
        let role = Role {
            name: "App User".into(),
            login: true,
            connection_limit: Some(5),
            member_of: set(&["readers"]),
            ..Role::default()
        };
        assert_eq!(
            role_statements(None, &role, Some("it's")),
            [
                "CREATE ROLE \"App User\" WITH LOGIN CONNECTION LIMIT 5 PASSWORD 'it''s'",
                "GRANT \"readers\" TO \"App User\"",
            ]
        );
        let bare = Role {
            name: "readers".into(),
            ..Role::default()
        };
        assert_eq!(
            role_statements(None, &bare, None),
            ["CREATE ROLE \"readers\""]
        );
    }

    #[test]
    fn alters_renames_and_changes_memberships() {
        let before = Role {
            name: "app".into(),
            login: true,
            valid_until: Some("2027-01-01 00:00:00+00".into()),
            member_of: set(&["readers", "writers"]),
            ..Role::default()
        };
        let after = Role {
            name: "service".into(),
            create_db: true,
            inherit: false,
            valid_until: None,
            member_of: set(&["writers", "pg_monitor"]),
            ..before.clone()
        };
        assert_eq!(
            role_statements(Some(&before), &after, Some("new")),
            [
                "ALTER ROLE \"app\" RENAME TO \"service\"",
                "ALTER ROLE \"service\" WITH CREATEDB NOINHERIT VALID UNTIL 'infinity' PASSWORD 'new'",
                "REVOKE \"readers\" FROM \"service\"",
                "GRANT \"pg_monitor\" TO \"service\"",
            ]
        );
        assert_eq!(
            role_statements(Some(&before), &before, None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn parses_roles_and_memberships() {
        let roles = rs(
            10,
            &[
                &[
                    Some("app"),
                    Some("true"),
                    Some("false"),
                    Some("true"),
                    Some("false"),
                    Some("true"),
                    Some("false"),
                    Some("false"),
                    Some("-1"),
                    None,
                ],
                &[
                    Some("old"),
                    Some("false"),
                    Some("false"),
                    Some("false"),
                    Some("false"),
                    Some("false"),
                    Some("false"),
                    Some("false"),
                    Some("3"),
                    Some("2027-01-01 00:00:00+00"),
                ],
            ],
        );
        let members = rs(2, &[&[Some("app"), Some("readers")]]);
        let parsed = parse_roles(&roles, &members);
        assert_eq!(parsed[0].name, "app");
        assert!(parsed[0].login && parsed[0].create_db && parsed[0].inherit);
        assert_eq!(parsed[0].connection_limit, None);
        assert_eq!(parsed[0].member_of, set(&["readers"]));
        assert!(!parsed[1].inherit);
        assert_eq!(parsed[1].connection_limit, Some(3));
        assert_eq!(
            parsed[1].valid_until.as_deref(),
            Some("2027-01-01 00:00:00+00")
        );
    }

    #[test]
    fn grants_revokes_and_grant_options() {
        let role = Grantee::Role("app".into());
        let table = Target::Table {
            schema: "s".into(),
            name: "t".into(),
        };
        assert_eq!(
            grant_statements(
                &role,
                &table,
                &grants(&["SELECT", "DELETE"], false),
                &grants(&["SELECT", "INSERT"], false)
            ),
            [
                "REVOKE DELETE ON TABLE \"s\".\"t\" FROM \"app\"",
                "GRANT INSERT ON TABLE \"s\".\"t\" TO \"app\""
            ]
        );
        assert_eq!(
            grant_statements(
                &role,
                &Target::Schema("s".into()),
                &grants(&["USAGE"], false),
                &grants(&["USAGE", "CREATE"], true)
            ),
            ["GRANT CREATE, USAGE ON SCHEMA \"s\" TO \"app\" WITH GRANT OPTION"]
        );
        assert_eq!(
            grant_statements(
                &role,
                &Target::Database("main db".into()),
                &grants(&["CONNECT"], true),
                &grants(&["CONNECT"], false)
            ),
            ["REVOKE GRANT OPTION FOR CONNECT ON DATABASE \"main db\" FROM \"app\""]
        );
        // PUBLIC can't hold a grant option.
        assert_eq!(
            grant_statements(
                &Grantee::Public,
                &Target::AllTables("s".into()),
                &grants(&[], false),
                &grants(&["SELECT"], true)
            ),
            ["GRANT SELECT ON ALL TABLES IN SCHEMA \"s\" TO PUBLIC"]
        );
        assert_eq!(
            default_privilege_statements(
                &role,
                &Target::AllSequences("s".into()),
                &grants(&["UPDATE"], false),
                &grants(&["USAGE"], false)
            ),
            [
                "ALTER DEFAULT PRIVILEGES IN SCHEMA \"s\" REVOKE UPDATE ON SEQUENCES FROM \"app\"",
                "ALTER DEFAULT PRIVILEGES IN SCHEMA \"s\" GRANT USAGE ON SEQUENCES TO \"app\"",
            ]
        );
        assert_eq!(
            default_privilege_statements(
                &role,
                &table,
                &grants(&[], false),
                &grants(&["SELECT"], false)
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn privileges_on_some_objects_are_partial() {
        let rows = rs(
            4,
            &[
                &[Some("SELECT"), Some("3"), Some("true"), Some("3")],
                &[Some("INSERT"), Some("1"), Some("false"), Some("3")],
                &[Some("MAINTAIN"), Some("3"), Some("false"), Some("3")],
            ],
        );
        let read = parse_privileges(&rows, &Target::AllTables("s".into()));
        assert_eq!(read.grants, grants(&["SELECT"], true));
        assert_eq!(read.partial, [("INSERT".to_owned(), 1)].into());
        assert_eq!(read.objects, 3);
        assert_eq!(
            parse_privileges(&rs(4, &[]), &Target::Schema("s".into())),
            Privileges::default()
        );
    }

    #[test]
    fn passwords_are_hidden() {
        assert_eq!(
            mask_password("ALTER ROLE a WITH LOGIN PASSWORD 'secret'"),
            "ALTER ROLE a WITH LOGIN PASSWORD '…'"
        );
        assert_eq!(mask_password("DROP ROLE a"), "DROP ROLE a");
    }
}
