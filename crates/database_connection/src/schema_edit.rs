use super::*;
use crate::table_edit::{qualified_table, quote_identifier};
use dbflux_core::{ConstraintKind, IndexData};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaEntry {
    pub name: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct TableStructure {
    pub table: TableInfo,
    pub columns: Vec<ColumnMetadata>,
    pub keys: Vec<SchemaEntry>,
    pub foreign_keys: Vec<SchemaEntry>,
    pub indexes: Vec<SchemaEntry>,
    pub checks: Vec<SchemaEntry>,
    pub editability_error: Option<String>,
    connection: Weak<ConnectionState>,
    create_statement: Option<String>,
    mysql_sql_mode: Option<String>,
}

pub struct StructureOperation {
    operation: QueryOperation,
    table: TableInfo,
    kind: DatabaseKind,
}

impl StructureOperation {
    pub fn cancel_handle(&self) -> CancelQuery {
        self.operation.cancel_handle()
    }

    pub fn execute(self) -> Result<TableStructure> {
        let guard = self.operation.connection.lock()?;
        self.operation.activate()?;
        let connection = guard.as_ref().context("Database is disconnected")?;
        let structure = load_structure(
            connection.as_ref(),
            &self.operation.connection,
            self.kind,
            &self.table,
        )?;
        self.operation.ensure_not_cancelled()?;
        Ok(structure)
    }
}

pub struct ColumnChangeOperation {
    operation: QueryOperation,
    structure: TableStructure,
    kind: DatabaseKind,
    statements: Vec<String>,
    column_name: String,
}

impl ColumnChangeOperation {
    pub fn cancel_handle(&self) -> CancelQuery {
        self.operation.cancel_handle()
    }

    pub fn execute(self) -> Result<()> {
        let guard = self.operation.connection.lock()?;
        self.operation.activate()?;
        let connection = guard.as_ref().context("Database is disconnected")?;
        let current = load_structure(
            connection.as_ref(),
            &self.operation.connection,
            self.kind,
            &self.structure.table,
        )?;
        ensure!(
            current.columns == self.structure.columns
                && current.mysql_sql_mode == self.structure.mysql_sql_mode,
            "Table schema changed since loading; reopen the column editor before saving"
        );
        if self.kind == DatabaseKind::MySql {
            ensure!(
                mysql_structure_column(&current, &self.column_name)?
                    == mysql_structure_column(&self.structure, &self.column_name)?,
                "Column definition changed since loading; reopen the column editor before saving"
            );
        }
        self.operation.ensure_not_cancelled()?;

        if self.kind == DatabaseKind::MySql {
            // MySQL DDL commits an existing transaction implicitly. The
            // driver's read-only preflight rejects open transactions and locks.
            connection
                .execute(
                    &QueryRequest::new("SELECT 1")
                        .with_read_only(ReadOnlyEnforcement::Required),
                )
                .context("Finish the open SQL transaction and release table locks before editing columns")?;
            self.operation.ensure_not_cancelled()?;
            for statement in &self.statements {
                connection
                    .execute(&QueryRequest::new(statement))
                    .context("Could not change the column")?;
            }
            return Ok(());
        }

        if self.kind == DatabaseKind::Postgres {
            let transaction = connection.execute(&QueryRequest::new(
                "SELECT transaction_timestamp() = statement_timestamp()",
            ))?;
            ensure!(
                matches!(
                    transaction.rows.first().and_then(|row| row.first()),
                    Some(CellValue::Bool(true))
                ),
                "Finish the open SQL transaction before editing columns"
            );
        }
        self.operation.ensure_not_cancelled()?;
        connection
            .execute(&QueryRequest::new(if self.kind == DatabaseKind::Sqlite {
                "BEGIN IMMEDIATE"
            } else {
                "BEGIN"
            }))
            .context("Finish the open SQL transaction before editing columns")?;
        let outcome = (|| {
            if self.kind == DatabaseKind::Postgres {
                connection.execute(&QueryRequest::new(format!(
                    "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
                    qualified_table(self.kind, &self.structure.table)
                )))?;
            }
            let current = load_structure(
                connection.as_ref(),
                &self.operation.connection,
                self.kind,
                &self.structure.table,
            )?;
            ensure!(
                current.columns == self.structure.columns,
                "Table schema changed since loading; reopen the column editor before saving"
            );
            for statement in &self.statements {
                self.operation.ensure_not_cancelled()?;
                connection
                    .execute(&QueryRequest::new(statement))
                    .context("Could not change the column")?;
            }
            self.operation.ensure_not_cancelled()?;
            connection
                .execute(&QueryRequest::new("COMMIT"))
                .context("Could not commit the column change")?;
            Ok(())
        })();
        if let Err(error) = outcome {
            // Cancellation must not interrupt rollback and leave the session
            // inside a failed transaction for the next operation.
            self.operation
                .control
                .state
                .lock()
                .map_err(|_| anyhow!("Database cancellation state failed"))?
                .phase = OperationPhase::Pending;
            connection
                .execute(&QueryRequest::new("ROLLBACK"))
                .with_context(|| format!("{error:#}; rollback also failed"))?;
            return Err(error);
        }
        Ok(())
    }
}

impl DatabaseConnection {
    pub fn start_table_structure(&self, table: &TableInfo) -> Result<StructureOperation> {
        Ok(StructureOperation {
            operation: self.start_query("SELECT 1", 1)?,
            table: table.clone(),
            kind: self.kind,
        })
    }

    pub fn supports_column_type_changes(&self) -> bool {
        self.kind != DatabaseKind::Sqlite
    }

    pub fn column_change_sql(
        &self,
        structure: &TableStructure,
        old_name: &str,
        new_name: &str,
        new_type: &str,
    ) -> Result<Vec<String>> {
        ensure!(!self.is_read_only(), "This connection is read-only");
        ensure!(
            Weak::ptr_eq(&structure.connection, &Arc::downgrade(&self.inner)),
            "This table belongs to a different connection; reopen it first"
        );
        if let Some(error) = &structure.editability_error {
            bail!("{error}");
        }
        build_column_change(self.kind, structure, old_name, new_name, new_type)
    }

    pub fn start_column_change(
        &self,
        structure: &TableStructure,
        old_name: &str,
        new_name: &str,
        new_type: &str,
    ) -> Result<ColumnChangeOperation> {
        let statements = self.column_change_sql(structure, old_name, new_name, new_type)?;
        Ok(ColumnChangeOperation {
            operation: self.start_query("SELECT 1", 1)?,
            structure: structure.clone(),
            kind: self.kind,
            statements,
            column_name: old_name.to_owned(),
        })
    }
}

fn load_structure(
    connection: &dyn Connection,
    state: &Arc<ConnectionState>,
    kind: DatabaseKind,
    table: &TableInfo,
) -> Result<TableStructure> {
    let database = if kind == DatabaseKind::MySql {
        table.schema.as_deref().unwrap_or(&state.database)
    } else {
        &state.database
    };
    let details = connection
        .table_details(database, table.schema.as_deref(), &table.name)
        .context("Could not load table structure")?;
    let (create_statement, mysql_sql_mode) =
        if kind == DatabaseKind::MySql && !table.is_view && !state.read_only {
            // Read the definition on the execution session so quoted defaults and
            // comments retain the same SQL-mode interpretation when reapplied.
            let mode = connection.execute(
                &QueryRequest::new("SELECT @@SESSION.sql_mode")
                    .with_read_only(ReadOnlyEnforcement::Required),
            )?;
            let mode = match mode.rows.first().and_then(|row| row.first()) {
                Some(CellValue::Text(mode)) => mode.clone(),
                _ => bail!("Could not read the MySQL session SQL mode"),
            };
            let definition = connection
                .execute(
                    &QueryRequest::new(format!(
                        "SHOW CREATE TABLE {}",
                        qualified_table(kind, table)
                    ))
                    .with_read_only(ReadOnlyEnforcement::Required),
                )
                .context("Could not load the existing column definitions")?;
            let definition = match definition.rows.first().and_then(|row| row.get(1)) {
                Some(CellValue::Text(definition)) => definition.clone(),
                _ => bail!("Could not read the existing MySQL table definition"),
            };
            (Some(definition), Some(mode))
        } else {
            (None, None)
        };
    let columns: Vec<_> = details
        .columns
        .context("The database did not return column metadata")?
        .into_iter()
        .map(|column| ColumnMetadata {
            name: column.name,
            type_name: column.type_name,
            nullable: column.nullable,
            is_primary_key: column.is_primary_key,
            default_value: column.default_value,
        })
        .collect();
    let mut keys = Vec::new();
    let mut indexes = Vec::new();
    if let Some(IndexData::Relational(entries)) = details.indexes {
        for index in entries {
            let detail = format!("({})", index.columns.join(", "));
            if index.is_primary {
                keys.push(SchemaEntry {
                    name: index.name.clone(),
                    detail: detail.clone(),
                });
            }
            indexes.push(SchemaEntry {
                name: index.name,
                detail: if index.is_unique {
                    format!("{detail} UNIQUE")
                } else {
                    detail
                },
            });
        }
    }
    if keys.is_empty() {
        let primary: Vec<_> = columns
            .iter()
            .filter(|column| column.is_primary_key)
            .map(|column| column.name.as_str())
            .collect();
        if !primary.is_empty() {
            keys.push(SchemaEntry {
                name: "PRIMARY".into(),
                detail: format!("({})", primary.join(", ")),
            });
        }
    }
    let foreign_keys = details
        .foreign_keys
        .unwrap_or_default()
        .into_iter()
        .map(|foreign_key| {
            let target = match foreign_key.referenced_schema {
                Some(schema) => format!("{schema}.{}", foreign_key.referenced_table),
                None => foreign_key.referenced_table,
            };
            SchemaEntry {
                name: foreign_key.name,
                detail: format!(
                    "({}) → {target} ({})",
                    foreign_key.columns.join(", "),
                    foreign_key.referenced_columns.join(", ")
                ),
            }
        })
        .collect();
    let mut checks = Vec::new();
    for constraint in details.constraints.unwrap_or_default() {
        if constraint.kind == ConstraintKind::Check {
            checks.push(SchemaEntry {
                name: constraint.name,
                detail: constraint.check_clause.unwrap_or_default(),
            });
        } else {
            keys.push(SchemaEntry {
                name: constraint.name,
                detail: format!(
                    "({}) {}",
                    constraint.columns.join(", "),
                    if constraint.kind == ConstraintKind::Unique {
                        "UNIQUE"
                    } else {
                        "EXCLUDE"
                    }
                ),
            });
        }
    }
    Ok(TableStructure {
        table: table.clone(),
        columns,
        keys,
        foreign_keys,
        indexes,
        checks,
        editability_error: if state.read_only {
            Some("This connection is read-only".into())
        } else if table.is_view {
            Some("View columns cannot be edited here; edit the view definition with SQL".into())
        } else {
            None
        },
        connection: Arc::downgrade(state),
        create_statement,
        mysql_sql_mode,
    })
}

fn build_column_change(
    kind: DatabaseKind,
    structure: &TableStructure,
    old_name: &str,
    new_name: &str,
    new_type: &str,
) -> Result<Vec<String>> {
    ensure!(
        !structure.table.is_view,
        "View columns cannot be edited here"
    );
    ensure!(
        !new_name.trim().is_empty() && !new_name.chars().any(char::is_control),
        "Enter a column name without control characters"
    );
    let column = structure
        .columns
        .iter()
        .find(|column| column.name == old_name)
        .context("Column no longer exists; reopen the column editor")?;
    ensure!(
        !structure
            .columns
            .iter()
            .any(|column| column.name != old_name && column.name == new_name),
        "Another column already has this name"
    );
    let new_type = new_type.trim();
    let renamed = new_name != old_name;
    let type_changed = new_type != column.type_name;
    ensure!(
        renamed || type_changed,
        "There are no pending column changes"
    );
    if type_changed {
        ensure!(
            kind != DatabaseKind::Sqlite,
            "SQLite type changes require rebuilding the table; use a reviewed SQL migration"
        );
        validate_column_type(new_type)?;
    }
    let table = qualified_table(kind, &structure.table);
    let old_column = quote_identifier(kind, old_name);
    let new_column = quote_identifier(kind, new_name);
    if kind == DatabaseKind::MySql {
        let definition = mysql_structure_column(structure, old_name)?;
        let definition = if type_changed {
            let type_end = mysql_type_end(definition, mysql_backslash_escapes(structure))?;
            format!("{new_type}{}", &definition[type_end..])
        } else {
            definition.to_owned()
        };
        return Ok(vec![format!(
            "ALTER TABLE {table} CHANGE COLUMN {old_column} {new_column} {definition};"
        )]);
    }
    let mut statements = Vec::new();
    if type_changed {
        statements.push(format!(
            "ALTER TABLE {table} ALTER COLUMN {old_column} TYPE {new_type};"
        ));
    }
    if renamed {
        statements.push(format!(
            "ALTER TABLE {table} RENAME COLUMN {old_column} TO {new_column};"
        ));
    }
    Ok(statements)
}

fn validate_column_type(value: &str) -> Result<()> {
    let invalid = || {
        anyhow!(
            "Enter a type only, such as varchar(255), numeric(18, 6), or timestamp with time zone"
        )
    };
    ensure!(
        !value.is_empty()
            && !value.chars().any(char::is_control)
            && !value.contains([';', '\\', '#'])
            && !value.contains("--")
            && !value.contains("/*")
            && !value.contains("*/"),
        "Column type contains unsupported SQL syntax"
    );
    let mut remaining = value;
    let mut names = Vec::new();
    while !remaining.is_empty() {
        let (name, rest) = type_identifier(remaining).ok_or_else(invalid)?;
        names.push(name.to_ascii_lowercase());
        remaining = rest.trim_start();
        if let Some(rest) = remaining.strip_prefix('.') {
            remaining = rest.trim_start();
            ensure!(
                names.len() == 1,
                "A type can have only one schema qualifier"
            );
            let (_, rest) = type_identifier(remaining).ok_or_else(invalid)?;
            remaining = rest.trim_start();
            break;
        }
        if remaining.starts_with(['(', '[']) || remaining.is_empty() {
            break;
        }
    }
    if names.len() > 1 {
        ensure!(
            matches!(
                names.join(" ").as_str(),
                "double precision"
                    | "character varying"
                    | "char varying"
                    | "bit varying"
                    | "national character varying"
                    | "national varchar"
                    | "timestamp with time zone"
                    | "timestamp without time zone"
                    | "time with time zone"
                    | "time without time zone"
                    | "int unsigned"
                    | "integer unsigned"
                    | "bigint unsigned"
                    | "smallint unsigned"
                    | "mediumint unsigned"
                    | "tinyint unsigned"
                    | "float unsigned"
                    | "double unsigned"
                    | "decimal unsigned"
                    | "numeric unsigned"
            ),
            "Column type contains unsupported modifiers"
        );
    }
    if remaining.starts_with('(') {
        let end = parenthesized_end(remaining, false)?;
        let arguments = &remaining[1..end - 1];
        let mut arguments = arguments.trim();
        ensure!(!arguments.is_empty(), "Type parameters cannot be empty");
        while !arguments.is_empty() {
            if arguments.starts_with('\'') {
                ensure!(
                    names
                        .first()
                        .is_some_and(|name| name == "enum" || name == "set"),
                    "String type parameters are supported only for enum and set"
                );
                let end = quoted_end(arguments, '\'', false)?;
                arguments = arguments[end..].trim_start();
            } else {
                let end = arguments
                    .find(|character: char| !character.is_ascii_digit())
                    .unwrap_or(arguments.len());
                ensure!(end > 0, "Type parameters must be numbers or enum values");
                arguments = arguments[end..].trim_start();
            }
            if arguments.is_empty() {
                break;
            }
            arguments = arguments
                .strip_prefix(',')
                .ok_or_else(invalid)?
                .trim_start();
            ensure!(!arguments.is_empty(), "Missing type parameter after comma");
        }
        remaining = remaining[end..].trim_start();
    }
    if matches!(
        remaining,
        "with time zone" | "without time zone" | "unsigned" | "unsigned zerofill" | "zerofill"
    ) {
        remaining = "";
    }
    while let Some(rest) = remaining.strip_prefix("[]") {
        remaining = rest.trim_start();
    }
    ensure!(
        remaining.is_empty(),
        "Column type contains unsupported SQL syntax"
    );
    Ok(())
}

fn type_identifier(value: &str) -> Option<(&str, &str)> {
    if value.starts_with('"') {
        let end = quoted_end(value, '"', false).ok()?;
        return Some((&value[..end], &value[end..]));
    }
    if !value.starts_with(|character: char| character.is_alphabetic() || character == '_') {
        return None;
    }
    let end = value
        .find(|character: char| !character.is_alphanumeric() && character != '_')
        .unwrap_or(value.len());
    Some((&value[..end], &value[end..]))
}

fn quoted_end(value: &str, quote: char, backslash_escapes: bool) -> Result<usize> {
    let mut characters = value.char_indices().peekable();
    ensure!(
        characters
            .next()
            .is_some_and(|(_, character)| character == quote),
        "Expected quoted SQL value"
    );
    while let Some((index, character)) = characters.next() {
        if character == '\\' && backslash_escapes {
            characters.next().context("Unterminated SQL escape")?;
        } else if character == quote {
            if characters.peek().is_some_and(|(_, next)| *next == quote) {
                characters.next();
            } else {
                return Ok(index + quote.len_utf8());
            }
        }
    }
    bail!("Unterminated quoted SQL value")
}

fn parenthesized_end(value: &str, backslash_escapes: bool) -> Result<usize> {
    let mut depth = 0usize;
    let mut offset = 0;
    while let Some(character) = value[offset..].chars().next() {
        match character {
            '\'' | '"' | '`' => {
                offset += quoted_end(&value[offset..], character, backslash_escapes)?;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .context("Unbalanced type parentheses")?;
                if depth == 0 {
                    return Ok(offset + 1);
                }
            }
            _ => {}
        }
        offset += character.len_utf8();
    }
    bail!("Unbalanced type parentheses")
}

fn mysql_backslash_escapes(structure: &TableStructure) -> bool {
    !structure.mysql_sql_mode.as_deref().is_some_and(|mode| {
        mode.split(',')
            .any(|flag| flag.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
    })
}

fn mysql_structure_column<'a>(structure: &'a TableStructure, name: &str) -> Result<&'a str> {
    mysql_column_definition(
        structure
            .create_statement
            .as_deref()
            .context("Existing column definition unavailable; refresh before editing")?,
        name,
        mysql_backslash_escapes(structure),
    )
}

fn mysql_column_definition<'a>(
    create: &'a str,
    name: &str,
    backslash_escapes: bool,
) -> Result<&'a str> {
    let mut start = 0;
    loop {
        match create[start..]
            .chars()
            .next()
            .context("Unsupported CREATE TABLE definition")?
        {
            '(' => {
                start += 1;
                break;
            }
            quote @ ('`' | '"') => start += quoted_end(&create[start..], quote, false)?,
            character => start += character.len_utf8(),
        }
    }
    let mut offset = start;
    while let Some(character) = create[offset..].chars().next() {
        if matches!(character, '`' | '"') {
            let end = offset + quoted_end(&create[offset..], character, false)?;
            let identifier = create[offset + 1..end - 1]
                .replace(&format!("{character}{character}"), &character.to_string());
            let mut definition_end = end;
            while let Some(character) = create[definition_end..].chars().next() {
                match character {
                    '\'' | '"' | '`' => {
                        definition_end += quoted_end(
                            &create[definition_end..],
                            character,
                            backslash_escapes && character != '`',
                        )?;
                    }
                    '(' => {
                        definition_end +=
                            parenthesized_end(&create[definition_end..], backslash_escapes)?
                    }
                    ',' | ')' => break,
                    _ => definition_end += character.len_utf8(),
                }
            }
            if identifier == name {
                return Ok(create[end..definition_end].trim());
            }
            offset = definition_end;
        }
        offset += character.len_utf8();
        if offset >= create.len() {
            break;
        }
    }
    bail!("Column definition unavailable; use SQL to edit this column")
}

fn mysql_type_end(definition: &str, backslash_escapes: bool) -> Result<usize> {
    let (_, remaining) = type_identifier(definition).context("Unsupported MySQL column type")?;
    let mut end = definition.len() - remaining.len();
    if definition[end..].starts_with('(') {
        end += parenthesized_end(&definition[end..], backslash_escapes)?;
    }
    for modifier in ["unsigned", "zerofill"] {
        let remaining = definition[end..].trim_start();
        if let Some(after) = remaining.strip_prefix(modifier)
            && (after.is_empty() || after.starts_with(char::is_whitespace))
        {
            end = definition.len() - after.len();
        }
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, DatabaseConnection, TableInfo)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("columns.sqlite3");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY);
             CREATE TABLE items (
                 id INTEGER PRIMARY KEY,
                 parent_id INTEGER REFERENCES parents(id),
                 name TEXT NOT NULL DEFAULT 'untitled' CHECK (length(name) > 0),
                 UNIQUE(name)
             );
             CREATE INDEX items_parent ON items(parent_id);
             INSERT INTO items(id, name) VALUES (1, 'kept');",
        )?;
        let connection = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path),
                ..Default::default()
            },
            Secrets::default(),
        )?;
        let table = TableInfo {
            schema: None,
            name: "items".into(),
            is_view: false,
        };
        Ok((directory, connection, table))
    }

    #[test]
    fn structure_includes_columns_keys_foreign_keys_indexes_and_checks() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let structure = connection.start_table_structure(&table)?.execute()?;
        assert_eq!(structure.columns.len(), 3);
        assert!(structure.keys.iter().any(|key| key.detail == "(id)"));
        assert!(
            structure
                .keys
                .iter()
                .any(|key| key.detail == "(name) UNIQUE")
        );
        assert!(
            structure
                .foreign_keys
                .iter()
                .any(|key| key.detail.contains("parents (id)"))
        );
        assert!(
            structure
                .indexes
                .iter()
                .any(|index| index.name == "items_parent")
        );
        assert!(
            structure
                .checks
                .iter()
                .any(|check| check.detail == "length(name) > 0")
        );
        assert!(structure.editability_error.is_none());
        Ok(())
    }

    #[test]
    fn sqlite_rename_preserves_data_and_constraints() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let structure = connection.start_table_structure(&table)?.execute()?;
        let sql = connection.column_change_sql(&structure, "name", "display\"name", "TEXT")?;
        assert_eq!(
            sql,
            ["ALTER TABLE \"items\" RENAME COLUMN \"name\" TO \"display\"\"name\";"]
        );
        connection
            .start_column_change(&structure, "name", "display\"name", "TEXT")?
            .execute()?;
        let changed = connection.start_table_structure(&table)?.execute()?;
        let column = changed
            .columns
            .iter()
            .find(|column| column.name == "display\"name")
            .context("Renamed column missing")?;
        assert!(!column.nullable);
        assert_eq!(column.default_value.as_deref(), Some("'untitled'"));
        assert_eq!(
            connection
                .query("SELECT \"display\"\"name\" FROM items", 10)?
                .rows,
            [vec!["kept"]]
        );
        assert!(
            connection
                .query(
                    "INSERT INTO items(id, \"display\"\"name\") VALUES (2, '')",
                    10
                )
                .is_err()
        );
        assert!(
            connection
                .query(
                    "INSERT INTO items(id, \"display\"\"name\") VALUES (2, 'kept')",
                    10
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn sqlite_rejects_type_changes_and_stale_schema_without_mutation() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let structure = connection.start_table_structure(&table)?.execute()?;
        assert!(
            connection
                .column_change_sql(&structure, "name", "name", "BLOB")
                .is_err()
        );
        connection.query("ALTER TABLE items ADD COLUMN added INTEGER", 10)?;
        assert!(
            connection
                .start_column_change(&structure, "name", "renamed", "TEXT")?
                .execute()
                .is_err()
        );
        assert_eq!(
            connection.query("SELECT name FROM items", 10)?.rows,
            [vec!["kept"]]
        );
        Ok(())
    }

    #[test]
    fn cancelled_changes_and_open_transactions_preserve_data() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let structure = connection.start_table_structure(&table)?.execute()?;
        let operation = connection.start_column_change(&structure, "name", "renamed", "TEXT")?;
        operation.cancel_handle().cancel()?;
        assert!(operation.execute().is_err());
        connection.query("BEGIN", 10)?;
        connection.query("UPDATE items SET name = 'manual' WHERE id = 1", 10)?;
        assert!(
            connection
                .start_column_change(&structure, "name", "renamed", "TEXT")?
                .execute()
                .is_err()
        );
        assert_eq!(
            connection.query("SELECT name FROM items", 10)?.rows,
            [vec!["manual"]]
        );
        connection.query("ROLLBACK", 10)?;
        assert_eq!(
            connection.query("SELECT name FROM items", 10)?.rows,
            [vec!["kept"]]
        );
        Ok(())
    }

    #[test]
    fn read_only_views_and_other_connections_cannot_change_columns() -> Result<()> {
        let (directory, connection, table) = fixture()?;
        let structure = connection.start_table_structure(&table)?.execute()?;
        let read_only = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(directory.path().join("columns.sqlite3")),
                read_only: true,
                ..Default::default()
            },
            Secrets::default(),
        )?;
        assert!(
            read_only
                .start_column_change(&structure, "name", "renamed", "TEXT")
                .is_err()
        );
        let read_only_structure = read_only.start_table_structure(&table)?.execute()?;
        assert_eq!(
            read_only_structure.editability_error.as_deref(),
            Some("This connection is read-only")
        );
        assert!(
            connection
                .start_column_change(&read_only_structure, "name", "renamed", "TEXT")
                .is_err()
        );
        connection.query("CREATE VIEW item_view AS SELECT * FROM items", 10)?;
        let view = TableInfo {
            name: "item_view".into(),
            is_view: true,
            schema: None,
        };
        let structure = connection.start_table_structure(&view)?.execute()?;
        assert_eq!(structure.columns.len(), 3);
        assert!(
            connection
                .start_column_change(&structure, "name", "renamed", "TEXT")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn postgres_preview_quotes_names_and_keeps_type_and_rename_separate() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let mut structure = connection.start_table_structure(&table)?.execute()?;
        structure.table.schema = Some("tenant\"schema".into());
        let statements = build_column_change(
            DatabaseKind::Postgres,
            &structure,
            "name",
            "new\"name",
            "varchar(255)",
        )?;
        assert_eq!(
            statements,
            [
                "ALTER TABLE \"tenant\"\"schema\".\"items\" ALTER COLUMN \"name\" TYPE varchar(255);",
                "ALTER TABLE \"tenant\"\"schema\".\"items\" RENAME COLUMN \"name\" TO \"new\"\"name\";"
            ]
        );
        Ok(())
    }

    #[test]
    fn mysql_type_changes_preserve_full_column_attributes() -> Result<()> {
        let (_directory, connection, table) = fixture()?;
        let mut structure = connection.start_table_structure(&table)?.execute()?;
        structure.create_statement = Some(
            "CREATE TABLE `items` (\n  `id` int unsigned NOT NULL AUTO_INCREMENT COMMENT 'primary, key',\n  `parent_id` int DEFAULT NULL,\n  `name` varchar(50) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'untitled' COMMENT 'it\\'s a name',\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB;".into()
        );
        let statements = build_column_change(
            DatabaseKind::MySql,
            &structure,
            "name",
            "new`name",
            "varchar(255)",
        )?;
        assert_eq!(
            statements,
            [
                "ALTER TABLE `items` CHANGE COLUMN `name` `new``name` varchar(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'untitled' COMMENT 'it\\'s a name';"
            ]
        );
        let statements = build_column_change(
            DatabaseKind::MySql,
            &structure,
            "id",
            "identifier",
            "bigint unsigned",
        )?;
        assert_eq!(
            statements,
            [
                "ALTER TABLE `items` CHANGE COLUMN `id` `identifier` bigint unsigned NOT NULL AUTO_INCREMENT COMMENT 'primary, key';"
            ]
        );
        Ok(())
    }

    #[test]
    fn mysql_definition_handles_enum_values_generated_expressions_and_escaped_names() -> Result<()>
    {
        let create = "CREATE TABLE `items` (`v``alue` enum('a,b','c)') NOT NULL DEFAULT 'a,b', `generated` int GENERATED ALWAYS AS ((length(`v``alue`) + 1)) STORED, KEY (`v``alue`))";
        assert_eq!(
            mysql_column_definition(create, "v`alue", true)?,
            "enum('a,b','c)') NOT NULL DEFAULT 'a,b'"
        );
        assert_eq!(
            mysql_column_definition(create, "generated", true)?,
            "int GENERATED ALWAYS AS ((length(`v``alue`) + 1)) STORED"
        );
        assert_eq!(
            mysql_type_end("enum('a,b','c)') NOT NULL", true)?,
            "enum('a,b','c)')".len()
        );
        Ok(())
    }

    #[test]
    fn mysql_definition_preserves_sql_modes_and_ignores_table_counter() -> Result<()> {
        let create = "CREATE TABLE \"odd(table\" (\"name\" varchar(50) DEFAULT 'slash\\' COMMENT 'kept', \"id\" int NOT NULL AUTO_INCREMENT) ENGINE=InnoDB AUTO_INCREMENT=1";
        let changed = create.replace("AUTO_INCREMENT=1", "AUTO_INCREMENT=2");
        assert_eq!(
            mysql_column_definition(create, "name", false)?,
            "varchar(50) DEFAULT 'slash\\' COMMENT 'kept'"
        );
        assert_eq!(
            mysql_column_definition(create, "id", false)?,
            mysql_column_definition(&changed, "id", false)?
        );
        Ok(())
    }

    #[test]
    fn type_validation_accepts_types_and_rejects_sql_expressions() {
        for valid in [
            "integer",
            "varchar(255)",
            "numeric(18, 6)",
            "double precision",
            "timestamp with time zone",
            "timestamp(6) without time zone",
            "int unsigned",
            "int(10) unsigned",
            "enum('a','b''c')",
            "public.kind[]",
            "\"Odd Schema\".\"My Type\"",
            "text[][]",
        ] {
            assert!(validate_column_type(valid).is_ok(), "Rejected {valid}");
        }
        for invalid in [
            "",
            "text; DROP TABLE items",
            "text -- comment",
            "text /* comment */",
            "text DEFAULT NULL",
            "text USING random()",
            "varchar(2), DROP COLUMN id",
            "varchar((2))",
            "numeric(2,)",
            "text) RENAME TO owned",
            "enum('a\\')",
            "int CHECK(id > 0)",
            "text COLLATE public.other",
            "text\nDEFAULT NULL",
        ] {
            assert!(validate_column_type(invalid).is_err(), "Accepted {invalid}");
        }
    }
}
