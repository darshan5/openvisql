# Porting from DBeaver Community

DBeaver Community (https://github.com/dbeaver/dbeaver) is licensed under Apache-2.0, so its
code and data may be ported into OpenViSQL (MIT). This survey was made on 2026-10-25, against
upstream commit `811053f8`. Paths are relative to the DBeaver repo root.

## Rules for ported code

Apache-2.0 §4 applies to anything we port, whether it's a translation from Java or copied data
such as SQL text or keyword lists.

1. Put a header at the top of the Rust file, or above the ported item:
   ```rust
   // Portions ported from DBeaver (https://github.com/dbeaver/dbeaver),
   // <relative/path/File.java>, Copyright (C) 2010-2026 DBeaver Corp and others,
   // licensed under the Apache License, Version 2.0. Modified for OpenViSQL.
   ```
2. Keep any additional copyright line from the source file. For example, some PostgreSQL lock
   and plan files are "Copyright (C) Andrew Khitrin".
3. On the first port, add `LICENSES/Apache-2.0.txt` and a root `NOTICE` that lists the ported
   files. DBeaver itself ships no NOTICE file.
4. **Never port these EPL-licensed files:**
   - `model.sql/.../completion/hippie/*`
   - `ui.editors.sql/.../templates/SQLVariable*.java`, `SQLTemplateStore.java`, `SQLContextType*.java`
   - `ui.editors.sql/.../SQLMatchingCharacterPainter.java`
   - the EPL files in `org.jkiss.dbeaver.ui`
5. **Don't port third-party dependencies.** Use Rust crates instead of JSQLParser (LGPL/Apache),
   Apache POI (use `rust_xlsxwriter`), JSch/SSHJ (use `russh`) and DBeaver's CSV reader (use
   `csv`).
6. **SQL formatter:** the source carries DBeaver headers only, but it's rumoured to descend from
   blancoSqlFormatter (LGPL). Re-implement it from the algorithm on top of `ovs-core`'s lexer
   rather than translating it line by line.

Portability key:
- **P1:** pure SQL or data; copy almost directly.
- **P2:** an algorithm mixed into DBeaver's object model; rewrite it by following the original.
- **P3:** tied to JDBC, Eclipse or SWT; use as a spec only.

## Port plan, mapped to the roadmap

| # | Roadmap item | Source (P1/P2) | Notes |
|---|---|---|---|
| 1 | Richer MySQL metadata (FKs, checks, PK/UK, sizes, engines, charsets, collations, users) | `plugins/org.jkiss.dbeaver.ext.mysql/src/.../model/MySQLCatalog.java`, `MySQLTable.java` (FK query ~L483), `MySQLDataSource.java`, `MySQLConstants.java` (P1) | Separate check-constraint queries for MySQL and MariaDB. MySQL types come from JDBC, so write that type table ourselves. |
| 2 | Grid editing | `model/.../exec/DBExecUtils.java` `getBestIdentifier()` L584-684, `DBUtils.java` `getBestTableIdentifier()`, `model.jdbc/.../struct/JDBCTable.java`, `data/resultset/DBDResultSetDataUpdater.java` (P2) | Key order: primary key → unique index whose columns are all NOT NULL → PK/UK constraints whose columns are all in the result → virtual key. Extends `ovs_core::sql::edit`. |
| 3 | Table Data filter and paging | `model/.../data/DBDDataFilter.java`, `DBDAttributeConstraint.java`, `exec/DBCLogicalOperator.java`, `impl/sql/StandardSQLDialectQueryGenerator.java`, `QueryTransformerLimit.java` (P2) | |
| 4 | Export | `data.transfer/.../stream/exporter/DataExporter{CSV,JSON,SQL,HTML,MarkdownTable,TXT}.java`, `StreamConsumerSettings.java`, settings schema in `data.transfer/plugin.xml` L48-267 (P1/P2) | CSV has a formula-injection guard; SQL export can write multi-row INSERTs and upserts. |
| 5 | Dialect data | `model/.../sql/SQLConstants.java`, `ext.mysql/.../MySQLDialect.java`, `ext.postgresql/.../PostgreDialect.java`, `ext.sqlite/.../SQLiteSQLDialect.java` (P1) | Replaces or extends `ovs_core::sql::keywords`. |
| 6 | Processlist, kill, status | `ext.mysql/.../session/MySQLSessionManager.java`, dashboards in `ext.mysql/plugin.xml` L413+, `ext.postgresql/.../session/PostgreSessionManager.java` (P1) | The processlist query joins `sys.processlist` when it is available. |
| 7 | Table designer DDL | `ext.mysql/.../edit/MySQLTable{,Column}Manager.java`, `MySQL{Index,Constraint,ForeignKey}Manager.java`, `model/.../impl/sql/edit/struct/SQLTable{,Column}Manager.java`; SQLite table rebuild in `ext.sqlite/.../edit/SQLiteTableManager.java` (P2) | Column moves use `MODIFY`, `CHANGE`, `FIRST` and `AFTER`. |
| 8 | SSH tunnel | `net.ssh/.../SSHConstants.java`, `config/*.java`, `SSHUtils.java`, property schema in `net.ssh/plugin.xml` (P1 config model) | Transport through `russh`; supports jump-host chains. |
| 9 | Dump, restore, CSV import | `ext.mysql/.../tasks/MySQL{ExportSettings,DatabaseExportHandler,ScriptExecuteHandler,NativeToolHandler}.java`, `data.transfer/.../importer/DataImporterCSV.java`, `StreamDataImporterColumnInfo.java` (P1/P2) | The password goes through a temporary `--defaults-file`, never argv. |
| 10 | Formatter and alias-aware completion | Formatter: `model.sql/.../format/tokenized/*` (re-implement; see rule 6). Completion: `model.sql/.../analyzer/TableReferences*.java` and the alias/join parts of `completion/SQLCompletionAnalyzer.java` (P2) | Skip the new 22k-line semantic engine. |
| — | Splitter test cases | `model.sql/.../parser/SQLScriptParser.java`, `parser/rules/*` (P3) | Use as a spec for edge cases in `ovs_core::sql::split_statements`, such as `$$` quoting for PostgreSQL. |
| — | PostgreSQL (phase 5) | `ext.postgresql/.../model/PostgreSchema.java`, `PostgreDatabase.java`, `PostgreDataType.java`, `edit/PostgreTableManager*.java`, `lock/PostgreLockManager.java` | DDL has to be synthesized from `pg_catalog` (`pg_get_*def`). |
| — | EXPLAIN | `ext.mysql/.../model/plan/*` (P2) | Classic and JSON plans. |

## Not in DBeaver Community

These are Enterprise-edition features: schema/data compare, visual query builder,
XLSX/JSON/XML import, mock data, task scheduler. The SQLite plugin is thin, so write our own
`PRAGMA`-based metadata queries.
