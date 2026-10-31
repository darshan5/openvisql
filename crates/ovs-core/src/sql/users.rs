//! MySQL and MariaDB accounts, roles and privileges: reading them from the `mysql` system
//! tables and the `CREATE USER` / `CREATE ROLE` / `GRANT` / `REVOKE` statements that change
//! them. The two servers keep roles and account locks differently, see [`Flavor`].

use std::collections::BTreeSet;

use super::dialect::Dialect;
use crate::result::ResultSet;
use crate::value::Value;

/// Which server: they store roles and account locks differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// MySQL 8: a role is an account (`'name'@'%'`), granted ones are in `mysql.role_edges`.
    MySql,
    /// MariaDB: a role has no host and `is_role = 'Y'`; granted ones are in
    /// `mysql.roles_mapping`; only one role is active at a time.
    MariaDb,
}

impl Flavor {
    /// From the server's version string.
    pub fn of(server_version: &str) -> Self {
        if server_version.to_ascii_lowercase().contains("mariadb") {
            Self::MariaDb
        } else {
            Self::MySql
        }
    }

    /// A role as `GRANT` names it.
    fn role(self, d: Dialect, role: &Account) -> String {
        match self {
            Self::MySql => role.sql(d),
            // Even one listed with a host, e.g. from roles_mapping.
            Self::MariaDb => d.quote_str(&role.user),
        }
    }
}

/// A `'user'@'host'` account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Account {
    pub user: String,
    pub host: String,
}

impl Account {
    /// `'user'@'host'`, or just `'name'` for a MariaDB role, which has no host.
    pub fn sql(&self, d: Dialect) -> String {
        if self.host.is_empty() {
            d.quote_str(&self.user)
        } else {
            format!("{}@{}", d.quote_str(&self.user), d.quote_str(&self.host))
        }
    }
}

impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.host)
    }
}

/// Where privileges apply.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Database(String),
    Table { database: String, table: String },
}

impl Scope {
    fn target(&self, d: Dialect) -> String {
        match self {
            Self::Global => "*.*".to_owned(),
            Self::Database(db) => format!("{}.*", d.quote_ident(db)),
            Self::Table { database, table } => {
                format!("{}.{}", d.quote_ident(database), d.quote_ident(table))
            }
        }
    }
}

/// (privilege, column in `mysql.user` / `mysql.db`, also grantable per database).
const PRIVILEGES: [(&str, &str, bool); 27] = [
    ("SELECT", "Select_priv", true),
    ("INSERT", "Insert_priv", true),
    ("UPDATE", "Update_priv", true),
    ("DELETE", "Delete_priv", true),
    ("CREATE", "Create_priv", true),
    ("DROP", "Drop_priv", true),
    ("ALTER", "Alter_priv", true),
    ("INDEX", "Index_priv", true),
    ("REFERENCES", "References_priv", true),
    ("CREATE TEMPORARY TABLES", "Create_tmp_table_priv", true),
    ("LOCK TABLES", "Lock_tables_priv", true),
    ("CREATE VIEW", "Create_view_priv", true),
    ("SHOW VIEW", "Show_view_priv", true),
    ("CREATE ROUTINE", "Create_routine_priv", true),
    ("ALTER ROUTINE", "Alter_routine_priv", true),
    ("EXECUTE", "Execute_priv", true),
    ("EVENT", "Event_priv", true),
    ("TRIGGER", "Trigger_priv", true),
    ("RELOAD", "Reload_priv", false),
    ("SHUTDOWN", "Shutdown_priv", false),
    ("PROCESS", "Process_priv", false),
    ("FILE", "File_priv", false),
    ("SHOW DATABASES", "Show_db_priv", false),
    ("SUPER", "Super_priv", false),
    ("REPLICATION SLAVE", "Repl_slave_priv", false),
    ("REPLICATION CLIENT", "Repl_client_priv", false),
    ("CREATE USER", "Create_user_priv", false),
];

/// Privileges on one table, as `GRANT` names them (`mysql.tables_priv` writes them in mixed
/// case, e.g. `Create View`).
const TABLE_PRIVILEGES: [&str; 12] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "CREATE VIEW",
    "SHOW VIEW",
    "TRIGGER",
];

/// The privileges that can be granted at `scope`, in display order.
pub fn privileges_for(scope: &Scope) -> Vec<&'static str> {
    if let Scope::Table { .. } = scope {
        return TABLE_PRIVILEGES.to_vec();
    }
    PRIVILEGES
        .iter()
        .filter(|(_, _, per_db)| *per_db || *scope == Scope::Global)
        .map(|(name, _, _)| *name)
        .collect()
}

/// An account's privileges at one scope.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grants {
    pub privileges: BTreeSet<String>,
    pub grant_option: bool,
}

/// Accounts and roles: user, host, is a role (1/0), locked (1/0).
pub fn list_accounts(flavor: Flavor) -> &'static str {
    match flavor {
        // CREATE ROLE makes a locked account without a password; anything granted to another
        // account is used as a role too.
        Flavor::MySql => {
            "SELECT u.User, u.Host, (u.account_locked = 'Y' AND u.authentication_string = '') \
             OR EXISTS (SELECT 1 FROM mysql.role_edges e WHERE e.FROM_USER = u.User \
             AND e.FROM_HOST = u.Host), u.account_locked = 'Y' FROM mysql.user u \
             ORDER BY u.User, u.Host"
        }
        Flavor::MariaDb => {
            "SELECT u.User, u.Host, u.is_role = 'Y', \
             IFNULL(JSON_EXTRACT(g.Priv, '$.account_locked'), 'false') = 'true' \
             FROM mysql.user u LEFT JOIN mysql.global_priv g \
             ON g.User = u.User AND g.Host = u.Host ORDER BY u.User, u.Host"
        }
    }
}

/// An account or role, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub account: Account,
    pub is_role: bool,
    pub locked: bool,
}

/// The databases an account has database-level privileges on.
pub fn list_database_grants(d: Dialect, account: &Account) -> String {
    format!(
        "SELECT Db FROM mysql.db WHERE User = {} AND Host = {} ORDER BY Db",
        d.quote_str(&account.user),
        d.quote_str(&account.host)
    )
}

/// The row of `mysql.user` or `mysql.db` holding the account's privileges at `scope`.
pub fn privileges_query(d: Dialect, account: &Account, scope: &Scope) -> String {
    let filter = format!(
        "User = {} AND Host = {}",
        d.quote_str(&account.user),
        d.quote_str(&account.host)
    );
    match scope {
        Scope::Global => format!("SELECT * FROM mysql.user WHERE {filter}"),
        Scope::Database(db) => {
            format!(
                "SELECT * FROM mysql.db WHERE {filter} AND Db = {}",
                d.quote_str(db)
            )
        }
        Scope::Table { database, table } => format!(
            "SELECT Table_priv FROM mysql.tables_priv WHERE {filter} AND Db = {} \
             AND Table_name = {}",
            d.quote_str(database),
            d.quote_str(table)
        ),
    }
}

fn is_yes(rs: &ResultSet, row: &[Value], column: &str) -> bool {
    rs.columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(column))
        .and_then(|i| row.get(i))
        .is_some_and(|v| v.to_string().eq_ignore_ascii_case("Y"))
}

/// Reads the result of [`privileges_query`]; no row means no privileges.
pub fn parse_privileges(rs: &ResultSet, scope: &Scope) -> Grants {
    let Some(row) = rs.rows.first() else {
        return Grants::default();
    };
    if let Scope::Table { .. } = scope {
        // A SET such as `Select,Insert,Grant,Create View`.
        let set = row.first().map(ToString::to_string).unwrap_or_default();
        let mut grants = Grants::default();
        for item in set.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let name = item.to_ascii_uppercase();
            if name == "GRANT" {
                grants.grant_option = true;
            } else if let Some(known) = TABLE_PRIVILEGES.iter().find(|p| **p == name) {
                grants.privileges.insert((*known).to_owned());
            }
        }
        return grants;
    }
    let allowed = privileges_for(scope);
    Grants {
        privileges: PRIVILEGES
            .iter()
            .filter(|(name, column, _)| allowed.contains(name) && is_yes(rs, row, column))
            .map(|(name, _, _)| (*name).to_owned())
            .collect(),
        grant_option: is_yes(rs, row, "Grant_priv"),
    }
}

/// Reads [`list_accounts`].
pub fn parse_accounts(rs: &ResultSet) -> Vec<AccountInfo> {
    let text = |r: &[Value], i: usize| r.get(i).map(ToString::to_string).unwrap_or_default();
    rs.rows
        .iter()
        .map(|r| AccountInfo {
            account: Account {
                user: text(r, 0),
                host: text(r, 1),
            },
            is_role: text(r, 2) == "1",
            locked: text(r, 3) == "1",
        })
        .collect()
}

/// The roles granted to `account`: (role user, role host), MariaDB roles having no host.
pub fn granted_roles_query(d: Dialect, flavor: Flavor, account: &Account) -> String {
    let (user, host) = (d.quote_str(&account.user), d.quote_str(&account.host));
    match flavor {
        Flavor::MySql => format!(
            "SELECT FROM_USER, FROM_HOST FROM mysql.role_edges \
             WHERE TO_USER = {user} AND TO_HOST = {host} ORDER BY 1, 2"
        ),
        Flavor::MariaDb => format!(
            "SELECT Role, '' FROM mysql.roles_mapping WHERE User = {user} AND Host = {host} \
             ORDER BY 1"
        ),
    }
}

/// Reads [`granted_roles_query`].
pub fn parse_granted_roles(rs: &ResultSet) -> BTreeSet<Account> {
    parse_accounts(rs).into_iter().map(|a| a.account).collect()
}

/// `GRANT` and `REVOKE` turning the roles granted to `account` from `before` into `after`,
/// then making them active when the account logs in (MySQL activates all of them; MariaDB
/// one, so only a single role is made the default there).
pub fn role_grant_statements(
    d: Dialect,
    flavor: Flavor,
    account: &Account,
    before: &BTreeSet<Account>,
    after: &BTreeSet<Account>,
) -> Vec<String> {
    if before == after {
        return Vec::new();
    }
    let who = account.sql(d);
    // One role per statement: MariaDB doesn't take a list.
    let mut out: Vec<String> = before
        .difference(after)
        .map(|r| format!("REVOKE {} FROM {who}", flavor.role(d, r)))
        .collect();
    out.extend(
        after
            .difference(before)
            .map(|r| format!("GRANT {} TO {who}", flavor.role(d, r))),
    );
    match flavor {
        Flavor::MySql if !after.is_empty() => out.push(format!("SET DEFAULT ROLE ALL TO {who}")),
        Flavor::MariaDb => match after.len() {
            0 => out.push(format!("SET DEFAULT ROLE NONE FOR {who}")),
            1 => {
                let role = after.first().expect("one role");
                out.push(format!(
                    "SET DEFAULT ROLE {} FOR {who}",
                    flavor.role(d, role)
                ));
            }
            _ => {}
        },
        Flavor::MySql => {}
    }
    out
}

pub fn create_role(d: Dialect, name: &str) -> String {
    format!("CREATE ROLE {}", d.quote_str(name))
}

pub fn drop_role(d: Dialect, flavor: Flavor, role: &Account) -> String {
    format!("DROP ROLE {}", flavor.role(d, role))
}

/// What the user manager applies to one account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountChange {
    /// `None` for a new account.
    pub original: Option<Account>,
    pub account: Account,
    /// A new password; `None` keeps the current one.
    pub password: Option<String>,
    /// Lock or unlock the account; `None` leaves it as it is.
    pub lock: Option<bool>,
}

/// `CREATE USER`, `RENAME USER`, `ALTER USER … IDENTIFIED BY` and `ACCOUNT LOCK` / `UNLOCK` as
/// needed. A password always ends its statement, so it can be hidden.
pub fn account_statements(d: Dialect, change: &AccountChange) -> Vec<String> {
    let mut out = Vec::new();
    let identified = |password: &str| format!(" IDENTIFIED BY {}", d.quote_str(password));
    match &change.original {
        None => out.push(format!(
            "CREATE USER {}{}",
            change.account.sql(d),
            change
                .password
                .as_deref()
                .map(identified)
                .unwrap_or_default()
        )),
        Some(original) => {
            if *original != change.account {
                out.push(format!(
                    "RENAME USER {} TO {}",
                    original.sql(d),
                    change.account.sql(d)
                ));
            }
            if let Some(password) = &change.password {
                out.push(format!(
                    "ALTER USER {}{}",
                    change.account.sql(d),
                    identified(password)
                ));
            }
        }
    }
    if let Some(lock) = change.lock {
        out.push(format!(
            "ALTER USER {} ACCOUNT {}",
            change.account.sql(d),
            if lock { "LOCK" } else { "UNLOCK" }
        ));
    }
    out
}

/// The `GRANT` and `REVOKE` statements turning `before` into `after` at `scope`.
pub fn grant_statements(
    d: Dialect,
    account: &Account,
    scope: &Scope,
    before: &Grants,
    after: &Grants,
) -> Vec<String> {
    let target = scope.target(d);
    let who = account.sql(d);
    let mut out = Vec::new();
    let removed: Vec<&str> = before
        .privileges
        .difference(&after.privileges)
        .map(String::as_str)
        .collect();
    if !removed.is_empty() {
        out.push(format!(
            "REVOKE {} ON {target} FROM {who}",
            removed.join(", ")
        ));
    }
    if before.grant_option && !after.grant_option {
        out.push(format!("REVOKE GRANT OPTION ON {target} FROM {who}"));
    }
    let added: Vec<&str> = after
        .privileges
        .difference(&before.privileges)
        .map(String::as_str)
        .collect();
    let add_option = after.grant_option && !before.grant_option;
    if !added.is_empty() || add_option {
        // `USAGE` grants nothing, so it carries a lone WITH GRANT OPTION.
        let list = if added.is_empty() {
            "USAGE".to_owned()
        } else {
            added.join(", ")
        };
        let option = if add_option { " WITH GRANT OPTION" } else { "" };
        out.push(format!("GRANT {list} ON {target} TO {who}{option}"));
    }
    out
}

pub fn drop_user(d: Dialect, account: &Account) -> String {
    format!("DROP USER {}", account.sql(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    const D: Dialect = Dialect::MySql;

    fn acc() -> Account {
        Account {
            user: "app".into(),
            host: "%".into(),
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn account_statements_cover_create_rename_and_password() {
        let created = account_statements(
            D,
            &AccountChange {
                original: None,
                account: acc(),
                password: Some("it's".into()),
                lock: Some(true),
            },
        );
        assert_eq!(
            created,
            [
                "CREATE USER 'app'@'%' IDENTIFIED BY 'it\\'s'",
                "ALTER USER 'app'@'%' ACCOUNT LOCK"
            ]
        );
        let renamed = account_statements(
            D,
            &AccountChange {
                original: Some(acc()),
                account: Account {
                    host: "localhost".into(),
                    ..acc()
                },
                password: Some("x".into()),
                lock: None,
            },
        );
        assert_eq!(
            renamed,
            [
                "RENAME USER 'app'@'%' TO 'app'@'localhost'",
                "ALTER USER 'app'@'localhost' IDENTIFIED BY 'x'",
            ]
        );
        let unchanged = AccountChange {
            original: Some(acc()),
            account: acc(),
            password: None,
            lock: None,
        };
        assert_eq!(account_statements(D, &unchanged).len(), 0);
    }

    #[test]
    fn grants_and_revokes_differences() {
        let scope = Scope::Database("shop".into());
        let before = Grants {
            privileges: set(&["SELECT", "DELETE"]),
            grant_option: true,
        };
        let after = Grants {
            privileges: set(&["SELECT", "INSERT", "UPDATE"]),
            grant_option: false,
        };
        assert_eq!(
            grant_statements(D, &acc(), &scope, &before, &after),
            [
                "REVOKE DELETE ON `shop`.* FROM 'app'@'%'",
                "REVOKE GRANT OPTION ON `shop`.* FROM 'app'@'%'",
                "GRANT INSERT, UPDATE ON `shop`.* TO 'app'@'%'",
            ]
        );
        let only_option = Grants {
            grant_option: true,
            ..after.clone()
        };
        assert_eq!(
            grant_statements(D, &acc(), &Scope::Global, &after, &only_option),
            ["GRANT USAGE ON *.* TO 'app'@'%' WITH GRANT OPTION"]
        );
        assert_eq!(grant_statements(D, &acc(), &scope, &after, &after).len(), 0);
    }

    #[test]
    fn parses_privilege_rows() {
        let rs = ResultSet {
            columns: [
                "Host",
                "Db",
                "User",
                "Select_priv",
                "Insert_priv",
                "Grant_priv",
                "Super_priv",
            ]
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                ..Column::default()
            })
            .collect(),
            rows: vec![
                ["%", "shop", "app", "Y", "N", "Y", "Y"]
                    .iter()
                    .map(|v| Value::Text((*v).into()))
                    .collect(),
            ],
            ..ResultSet::default()
        };
        let db = parse_privileges(&rs, &Scope::Database("shop".into()));
        // SUPER can't be granted per database, so it's ignored there.
        assert_eq!(db.privileges, set(&["SELECT"]));
        assert!(db.grant_option);
        let global = parse_privileges(&rs, &Scope::Global);
        assert_eq!(global.privileges, set(&["SELECT", "SUPER"]));
        assert_eq!(
            parse_privileges(&ResultSet::default(), &Scope::Global),
            Grants::default()
        );
        assert!(
            privileges_for(&Scope::Global).len()
                > privileges_for(&Scope::Database(String::new())).len()
        );
    }

    #[test]
    fn table_privileges_read_and_change() {
        let scope = Scope::Table {
            database: "shop".into(),
            table: "orders".into(),
        };
        let rs = ResultSet {
            columns: vec![Column {
                name: "Table_priv".into(),
                ..Column::default()
            }],
            rows: vec![vec![Value::Text(
                "Select,Insert,Grant,Create View,Show view".into(),
            )]],
            ..ResultSet::default()
        };
        let read = parse_privileges(&rs, &scope);
        assert_eq!(
            read.privileges,
            set(&["SELECT", "INSERT", "CREATE VIEW", "SHOW VIEW"])
        );
        assert!(read.grant_option);
        let after = Grants {
            privileges: set(&["SELECT", "UPDATE"]),
            grant_option: true,
        };
        assert_eq!(
            grant_statements(D, &acc(), &scope, &read, &after),
            [
                "REVOKE CREATE VIEW, INSERT, SHOW VIEW ON `shop`.`orders` FROM 'app'@'%'",
                "GRANT UPDATE ON `shop`.`orders` TO 'app'@'%'",
            ]
        );
    }

    #[test]
    fn roles_are_granted_and_made_active() {
        let role = |name: &str, host: &str| Account {
            user: name.into(),
            host: host.into(),
        };
        let readers = role("readers", "%");
        let writers = role("writers", "%");
        assert_eq!(
            role_grant_statements(
                D,
                Flavor::MySql,
                &acc(),
                &[readers.clone()].into(),
                &[writers.clone()].into()
            ),
            [
                "REVOKE 'readers'@'%' FROM 'app'@'%'",
                "GRANT 'writers'@'%' TO 'app'@'%'",
                "SET DEFAULT ROLE ALL TO 'app'@'%'",
            ]
        );
        let maria = role("readers", "");
        assert_eq!(
            role_grant_statements(
                D,
                Flavor::MariaDb,
                &acc(),
                &BTreeSet::new(),
                &[maria.clone()].into()
            ),
            [
                "GRANT 'readers' TO 'app'@'%'",
                "SET DEFAULT ROLE 'readers' FOR 'app'@'%'",
            ]
        );
        assert_eq!(
            role_grant_statements(
                D,
                Flavor::MariaDb,
                &acc(),
                &[maria.clone()].into(),
                &BTreeSet::new()
            ),
            [
                "REVOKE 'readers' FROM 'app'@'%'",
                "SET DEFAULT ROLE NONE FOR 'app'@'%'",
            ]
        );
        assert_eq!(
            role_grant_statements(
                D,
                Flavor::MySql,
                &acc(),
                &[readers.clone()].into(),
                &[readers].into()
            ),
            Vec::<String>::new()
        );
        assert_eq!(drop_role(D, Flavor::MariaDb, &maria), "DROP ROLE 'readers'");
        assert_eq!(Flavor::of("11.4.2-MariaDB-ubu2404"), Flavor::MariaDb);
        assert_eq!(Flavor::of("8.4.3"), Flavor::MySql);
    }
}
