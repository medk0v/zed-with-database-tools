use super::*;
use std::collections::HashSet;

pub use dbflux_core::Value as CellValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMetadata {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
}

#[derive(Clone)]
pub struct TableSnapshot {
    pub table: TableInfo,
    pub columns: Vec<ColumnMetadata>,
    pub primary_key: Vec<usize>,
    pub result: QueryResult,
    pub total_rows: u64,
    pub offset: u64,
    pub editability_error: Option<String>,
    connection: Weak<ConnectionState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellInput {
    Text(String),
    Null,
    Default,
}

#[derive(Clone, Debug)]
pub enum RowChange {
    Update {
        original: Vec<CellValue>,
        values: Vec<(usize, CellInput)>,
    },
    Delete {
        original: Vec<CellValue>,
    },
    Insert {
        values: Vec<(usize, CellInput)>,
    },
}

#[derive(Clone, Debug)]
pub struct ChangeResult {
    pub affected_rows: u64,
}

pub struct BrowseOperation {
    operation: QueryOperation,
    table: TableInfo,
    kind: DatabaseKind,
    offset: u64,
}

impl BrowseOperation {
    pub fn cancel_handle(&self) -> CancelQuery {
        self.operation.cancel_handle()
    }

    pub fn execute(self) -> Result<TableSnapshot> {
        let guard = self.operation.connection.lock()?;
        self.operation.activate()?;
        let connection = guard.as_ref().context("Database is disconnected")?;
        let details = if self.table.is_view {
            None
        } else {
            Some(
                connection
                    .table_details(
                        &self.operation.connection.database,
                        self.table.schema.as_deref(),
                        &self.table.name,
                    )
                    .context("Could not load table columns")?,
            )
        };
        let columns: Vec<_> = details
            .and_then(|details| details.columns)
            .unwrap_or_default()
            .into_iter()
            .map(|column| ColumnMetadata {
                name: column.name,
                type_name: column.type_name,
                nullable: column.nullable,
                is_primary_key: column.is_primary_key,
                default_value: column.default_value,
            })
            .collect();
        let primary_key: Vec<_> = columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| column.is_primary_key.then_some(index))
            .collect();
        self.operation.ensure_not_cancelled()?;
        let count_sql = format!(
            "SELECT COUNT(*) FROM ({}) AS counted_rows",
            self.operation.sql
        );
        let count = connection
            .execute(
                &QueryRequest::new(&count_sql)
                    .with_limit(1)
                    .with_read_only(ReadOnlyEnforcement::Required),
            )
            .context("Could not count table rows")?;
        self.operation.ensure_not_cancelled()?;
        let total_rows = match count.rows.first().and_then(|row| row.first()) {
            Some(CellValue::Int(count)) => {
                u64::try_from(*count).context("Table row count must not be negative")?
            }
            Some(CellValue::Decimal(count) | CellValue::Text(count)) => count
                .parse::<u64>()
                .context("Table row count must be an unsigned integer")?,
            _ => bail!("Database returned an invalid table row count"),
        };
        let limit = u64::from(self.operation.limit);
        let last_page_offset = total_rows.saturating_sub(1) / limit * limit;
        let offset = self.offset.min(last_page_offset);
        let mut sql = self.operation.sql.clone();
        let ordering: Vec<_> = columns
            .iter()
            .filter(|column| column.is_primary_key)
            .map(|column| quote_identifier(self.kind, &column.name))
            .collect();
        if !ordering.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", ordering.join(", ")));
        }
        sql.push_str(&format!(
            " LIMIT {} OFFSET {}",
            self.operation.limit, offset
        ));
        let result = connection
            .execute(
                &QueryRequest::new(&sql)
                    .with_limit(self.operation.limit)
                    .with_read_only(ReadOnlyEnforcement::Required),
            )
            .context("Could not load table rows")?;
        self.operation.ensure_not_cancelled()?;
        let result = QueryResult::from_driver(result)?;
        let editability_error = if self.operation.connection.read_only {
            Some("This connection is read-only".into())
        } else if self.table.is_view {
            Some("Views are not editable in the grid".into())
        } else if primary_key.is_empty() {
            Some("Grid editing requires a primary key; use SQL for this table".into())
        } else if columns
            .iter()
            .map(|column| &column.name)
            .ne(result.columns.iter())
        {
            Some("Table has generated or unsupported columns; use SQL to edit it".into())
        } else if result.values.iter().flatten().any(|value| {
            matches!(
                value,
                CellValue::Unsupported(_) | CellValue::Array(_) | CellValue::Document(_)
            )
        }) {
            Some(
                "This table contains values that cannot be round-tripped safely in the grid".into(),
            )
        } else {
            None
        };
        Ok(TableSnapshot {
            table: self.table,
            columns,
            primary_key,
            result,
            total_rows,
            offset,
            editability_error,
            connection: Arc::downgrade(&self.operation.connection),
        })
    }
}

pub struct ChangeOperation {
    operation: QueryOperation,
    snapshot: TableSnapshot,
    statements: Vec<String>,
}

impl ChangeOperation {
    pub fn cancel_handle(&self) -> CancelQuery {
        self.operation.cancel_handle()
    }

    pub fn execute(self) -> Result<ChangeResult> {
        let guard = self.operation.connection.lock()?;
        self.operation.activate()?;
        let connection = guard.as_ref().context("Database is disconnected")?;
        let details = connection.table_details(
            &self.operation.connection.database,
            self.snapshot.table.schema.as_deref(),
            &self.snapshot.table.name,
        )?;
        let current: Vec<_> = details
            .columns
            .context("Table columns unavailable")?
            .into_iter()
            .map(|column| ColumnMetadata {
                name: column.name,
                type_name: column.type_name,
                nullable: column.nullable,
                is_primary_key: column.is_primary_key,
                default_value: column.default_value,
            })
            .collect();
        ensure!(
            current == self.snapshot.columns,
            "Table schema changed since loading; refresh before saving"
        );
        self.operation.ensure_not_cancelled()?;
        let affected_rows = connection
            .execute_atomic_row_changes(
                self.snapshot.table.schema.as_deref(),
                &self.snapshot.table.name,
                &self.statements,
            )
            .context("Could not save table changes")?;
        Ok(ChangeResult { affected_rows })
    }
}

impl DatabaseConnection {
    pub fn start_drop_table(&self, table: &TableInfo) -> Result<QueryOperation> {
        ensure!(!self.is_read_only(), "This connection is read-only");
        let object = if table.is_view { "VIEW" } else { "TABLE" };
        self.start_query(
            &format!("DROP {object} {}", qualified_table(self.kind, table)),
            1,
        )
    }

    pub fn is_read_only(&self) -> bool {
        self.inner.read_only
    }

    pub fn is_busy(&self) -> bool {
        self.inner.busy.load(Ordering::Acquire)
    }

    pub fn start_browse(
        &self,
        table: &TableInfo,
        limit: u32,
        offset: u64,
    ) -> Result<BrowseOperation> {
        self.start_browse_filtered(table, None, limit, offset)
    }

    pub fn start_browse_filtered(
        &self,
        table: &TableInfo,
        filter: Option<&str>,
        limit: u32,
        offset: u64,
    ) -> Result<BrowseOperation> {
        let mut sql = format!("SELECT * FROM {}", qualified_table(self.kind, table));
        if let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) {
            validate_filter(filter)?;
            sql.push_str(&format!(" WHERE ({filter})"));
        }
        Ok(BrowseOperation {
            operation: self.start_query(&sql, limit)?,
            table: table.clone(),
            kind: self.kind,
            offset,
        })
    }

    pub fn start_changes(
        &self,
        snapshot: &TableSnapshot,
        changes: Vec<RowChange>,
    ) -> Result<ChangeOperation> {
        ensure!(!self.is_read_only(), "This connection is read-only");
        ensure!(
            Weak::ptr_eq(&snapshot.connection, &Arc::downgrade(&self.inner)),
            "This table belongs to a different connection; refresh it first"
        );
        if let Some(reason) = &snapshot.editability_error {
            bail!("{reason}");
        }
        ensure!(!changes.is_empty(), "There are no pending changes");
        let statements = build_statements(self.kind, snapshot, &changes)?;
        let operation = self.start_query("SELECT 1", 1)?;
        Ok(ChangeOperation {
            operation,
            snapshot: snapshot.clone(),
            statements,
        })
    }
}

impl QueryOperation {
    pub(super) fn activate(&self) -> Result<()> {
        ensure!(
            !self.connection.disconnected.load(Ordering::Acquire),
            "Database is disconnected"
        );
        let mut state = self
            .control
            .state
            .lock()
            .map_err(|_| anyhow!("Database cancellation state failed"))?;
        ensure!(!state.cancelled, "Operation cancelled");
        state.phase = OperationPhase::Running;
        Ok(())
    }

    pub(super) fn ensure_not_cancelled(&self) -> Result<()> {
        let state = self
            .control
            .state
            .lock()
            .map_err(|_| anyhow!("Database cancellation state failed"))?;
        ensure!(!state.cancelled, "Operation cancelled");
        Ok(())
    }
}

pub fn cell_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bytes(bytes) => format!("0x{}", hex(bytes)),
        value => display_value(value.clone()),
    }
}

pub(super) fn quote_identifier(kind: DatabaseKind, value: &str) -> String {
    let quote = if kind == DatabaseKind::MySql {
        '`'
    } else {
        '"'
    };
    format!(
        "{quote}{}{quote}",
        value.replace(quote, &format!("{quote}{quote}"))
    )
}

pub(super) fn qualified_table(kind: DatabaseKind, table: &TableInfo) -> String {
    match table.schema.as_deref().filter(|schema| !schema.is_empty()) {
        Some(schema) => format!(
            "{}.{}",
            quote_identifier(kind, schema),
            quote_identifier(kind, &table.name)
        ),
        None => quote_identifier(kind, &table.name),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn text_literal(kind: DatabaseKind, text: &str) -> Result<String> {
    Ok(match kind {
        DatabaseKind::Postgres => {
            ensure!(
                !text.contains('\0'),
                "PostgreSQL text cannot contain a NUL byte"
            );
            format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
        }
        DatabaseKind::MySql => format!("CONVERT(X'{}' USING utf8mb4)", hex(text.as_bytes())),
        DatabaseKind::Sqlite => format!("CAST(X'{}' AS TEXT)", hex(text.as_bytes())),
    })
}

fn value_literal(kind: DatabaseKind, value: &CellValue) -> Result<String> {
    Ok(match value {
        CellValue::Null => "NULL".into(),
        CellValue::Int(value) => value.to_string(),
        CellValue::Bool(value) => if *value { "TRUE" } else { "FALSE" }.into(),
        CellValue::Float(value) => {
            ensure!(
                value.is_finite(),
                "Non-finite numbers cannot be edited in the grid"
            );
            value.to_string()
        }
        CellValue::Bytes(bytes) => match kind {
            DatabaseKind::Postgres => format!("decode('{}', 'hex')", hex(bytes)),
            _ => format!("X'{}'", hex(bytes)),
        },
        CellValue::Unsupported(_) | CellValue::Array(_) | CellValue::Document(_) => {
            bail!("This value cannot be edited losslessly in the grid")
        }
        value => text_literal(kind, &cell_text(value))?,
    })
}

fn input_literal(kind: DatabaseKind, column: &ColumnMetadata, input: &CellInput) -> Result<String> {
    match input {
        CellInput::Null => {
            ensure!(column.nullable, "Column {} cannot be NULL", column.name);
            Ok("NULL".into())
        }
        CellInput::Default => {
            if kind == DatabaseKind::Sqlite {
                Ok(column
                    .default_value
                    .clone()
                    .unwrap_or_else(|| "NULL".into()))
            } else {
                Ok("DEFAULT".into())
            }
        }
        CellInput::Text(value) => {
            let data_type = column.type_name.to_ascii_lowercase();
            if data_type.contains("blob") || data_type.contains("binary") || data_type == "bytea" {
                let value = value.strip_prefix("0x").unwrap_or(value);
                ensure!(
                    value.len() % 2 == 0 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "Binary value must be hexadecimal (for example 0x00ff)"
                );
                return Ok(if kind == DatabaseKind::Postgres {
                    format!("decode('{value}', 'hex')")
                } else {
                    format!("X'{value}'")
                });
            }
            text_literal(kind, value)
        }
    }
}

fn original_predicate(
    kind: DatabaseKind,
    snapshot: &TableSnapshot,
    original: &[CellValue],
) -> Result<String> {
    ensure!(
        original.len() == snapshot.columns.len(),
        "Row shape changed; refresh the table"
    );
    ensure!(
        snapshot.result.values.iter().any(|row| row == original),
        "Row is not part of this table snapshot"
    );
    for index in &snapshot.primary_key {
        ensure!(
            !original
                .get(*index)
                .context("Primary key column missing")?
                .is_null(),
            "Rows with NULL primary-key values cannot be edited in the grid"
        );
    }
    let mut clauses = Vec::with_capacity(original.len());
    for (column, value) in snapshot.columns.iter().zip(original) {
        let name = quote_identifier(kind, &column.name);
        if value.is_null() {
            clauses.push(format!("{name} IS NULL"));
            continue;
        }
        let value_sql = value_literal(kind, value)?;
        let clause = match (kind, value) {
            // Text comparisons must observe case and trailing spaces even on
            // columns with case-insensitive collations.
            (DatabaseKind::Sqlite, CellValue::Text(_)) => {
                format!("{name} COLLATE BINARY = {value_sql}")
            }
            (DatabaseKind::MySql, CellValue::Text(_) | CellValue::Json(_)) => {
                format!("CAST({name} AS BINARY) = CAST({value_sql} AS BINARY)")
            }
            (DatabaseKind::Postgres, CellValue::Text(text)) => format!(
                "convert_to({name}::text, 'UTF8') = decode('{}', 'hex')",
                hex(text.as_bytes())
            ),
            (DatabaseKind::Postgres, CellValue::Json(_)) => format!("{name}::text = {value_sql}"),
            _ => format!("{name} = {value_sql}"),
        };
        clauses.push(clause);
    }
    Ok(clauses.join(" AND "))
}

fn build_statements(
    kind: DatabaseKind,
    snapshot: &TableSnapshot,
    changes: &[RowChange],
) -> Result<Vec<String>> {
    ensure!(
        !snapshot.primary_key.is_empty(),
        "Grid editing requires a primary key"
    );
    let table = qualified_table(kind, &snapshot.table);
    let mut statements = Vec::new();
    for change in changes {
        match change {
            RowChange::Delete { original } => statements.push(format!(
                "DELETE FROM {table} WHERE {}",
                original_predicate(kind, snapshot, original)?
            )),
            RowChange::Update { original, values } => {
                ensure!(!values.is_empty(), "Row has no changed cells");
                let assignments = assignments(kind, &snapshot.columns, values, false)?;
                let predicate = original_predicate(kind, snapshot, original)?;
                statements.push(format!(
                    "UPDATE {table} SET {} WHERE {predicate}",
                    assignments
                        .into_iter()
                        .map(|(column, value)| format!("{column} = {value}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            RowChange::Insert { values } => {
                let assignments = assignments(kind, &snapshot.columns, values, true)?;
                if assignments.is_empty() {
                    statements.push(if kind == DatabaseKind::MySql {
                        format!("INSERT INTO {table} () VALUES ()")
                    } else {
                        format!("INSERT INTO {table} DEFAULT VALUES")
                    });
                } else {
                    statements.push(format!(
                        "INSERT INTO {table} ({}) VALUES ({})",
                        assignments
                            .iter()
                            .map(|(column, _)| column.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        assignments
                            .iter()
                            .map(|(_, value)| value.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
    }
    Ok(statements)
}

fn assignments(
    kind: DatabaseKind,
    columns: &[ColumnMetadata],
    values: &[(usize, CellInput)],
    insert: bool,
) -> Result<Vec<(String, String)>> {
    let mut seen = HashSet::new();
    let mut assignments = Vec::new();
    for (index, value) in values {
        ensure!(seen.insert(index), "Column specified more than once");
        let column = columns.get(*index).context("Column no longer exists")?;
        if insert && *value == CellInput::Default {
            continue;
        }
        assignments.push((
            quote_identifier(kind, &column.name),
            input_literal(kind, column, value)?,
        ));
    }
    Ok(assignments)
}

fn validate_filter(filter: &str) -> Result<()> {
    let mut characters = filter.chars().peekable();
    let mut quote = None;
    let mut depth = 0usize;
    while let Some(character) = characters.next() {
        if let Some(delimiter) = quote {
            ensure!(
                character != '\\',
                "Backslash escapes are not supported in WHERE filters; use doubled quotes"
            );
            if character == delimiter {
                if characters.peek() == Some(&delimiter) {
                    characters.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => quote = Some(character),
            '(' => depth += 1,
            ')' => {
                ensure!(depth > 0, "Unbalanced parentheses in WHERE filter");
                depth -= 1;
            }
            ';' | '#' => {
                bail!("WHERE filter must be a single expression without comments or semicolons")
            }
            '$' | '\\' | '[' | ']' => {
                bail!("Use ordinary quoted identifiers and doubled-quote strings in WHERE filters")
            }
            '-' if characters.peek() == Some(&'-') => {
                bail!("Comments are not supported in WHERE filters")
            }
            '/' if characters.peek() == Some(&'*') => {
                bail!("Comments are not supported in WHERE filters")
            }
            _ => {}
        }
    }
    ensure!(
        quote.is_none() && depth == 0,
        "Unclosed quote or parentheses in WHERE filter"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, DatabaseConnection, TableInfo)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("editing.sqlite");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE items (tenant INTEGER NOT NULL, id INTEGER NOT NULL, name TEXT DEFAULT 'new', data BLOB, PRIMARY KEY(tenant,id));
             INSERT INTO items VALUES (1,1,'one',X'00ff'),(1,2,NULL,NULL),(1,3,'NULL',X'01');"
        )?;
        let connection = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path.clone()),
                ..Default::default()
            },
            Secrets::default(),
        )?;
        let table = TableInfo {
            schema: None,
            name: "items".into(),
            is_view: false,
        };
        Ok((directory, path, connection, table))
    }

    fn pagination_fixture(
        row_count: u32,
    ) -> Result<(tempfile::TempDir, PathBuf, DatabaseConnection, TableInfo)> {
        let fixture = fixture()?;
        let external = rusqlite::Connection::open(&fixture.1)?;
        external.execute("DELETE FROM items", [])?;
        external.execute(
            "WITH RECURSIVE ids(id) AS (
                 SELECT 1 WHERE ?1 > 0
                 UNION ALL SELECT id + 1 FROM ids WHERE id < ?1
             ) INSERT INTO items (tenant,id) SELECT 1,id FROM ids",
            [row_count],
        )?;
        Ok(fixture)
    }

    #[test]
    fn table_pages_report_exact_totals_and_apply_the_same_filter() -> Result<()> {
        let (_directory, _path, connection, table) = pagination_fixture(1201)?;
        for (limit, offset, rows, first_id, last_id) in [
            (500, 0, 500, 1, 500),
            (500, 500, 500, 501, 1000),
            (500, 1000, 201, 1001, 1201),
            (100, 100, 100, 101, 200),
            (1000, 0, 1000, 1, 1000),
            (1000, 1000, 201, 1001, 1201),
            (2000, 0, 1201, 1, 1201),
            (5000, 0, 1201, 1, 1201),
            (10_000, 0, 1201, 1, 1201),
        ] {
            let snapshot = connection.start_browse(&table, limit, offset)?.execute()?;
            assert_eq!(snapshot.total_rows, 1201);
            assert_eq!(snapshot.offset, offset);
            assert_eq!(snapshot.result.rows.len(), rows);
            assert_eq!(
                snapshot.result.values.first().and_then(|row| row.get(1)),
                Some(&CellValue::Int(first_id))
            );
            assert_eq!(
                snapshot.result.values.last().and_then(|row| row.get(1)),
                Some(&CellValue::Int(last_id))
            );
        }
        for (offset, rows, first_id, last_id) in [(0, 500, 1, 999), (500, 101, 1001, 1201)] {
            let snapshot = connection
                .start_browse_filtered(&table, Some("id % 2 = 1"), 500, offset)?
                .execute()?;
            assert_eq!(snapshot.total_rows, 601);
            assert_eq!(snapshot.offset, offset);
            assert_eq!(snapshot.result.rows.len(), rows);
            assert_eq!(
                snapshot.result.values.first().and_then(|row| row.get(1)),
                Some(&CellValue::Int(first_id))
            );
            assert_eq!(
                snapshot.result.values.last().and_then(|row| row.get(1)),
                Some(&CellValue::Int(last_id))
            );
        }
        let empty = connection
            .start_browse_filtered(&table, Some("id < 0"), 500, 1000)?
            .execute()?;
        assert_eq!(empty.total_rows, 0);
        assert_eq!(empty.offset, 0);
        assert!(empty.result.rows.is_empty());

        let (_directory, _path, connection, table) = pagination_fixture(MAX_ROW_LIMIT + 1)?;
        for (offset, rows) in [(0, MAX_ROW_LIMIT), (u64::from(MAX_ROW_LIMIT), 1)] {
            let snapshot = connection
                .start_browse(&table, u32::MAX, offset)?
                .execute()?;
            assert_eq!(snapshot.total_rows, u64::from(MAX_ROW_LIMIT) + 1);
            assert_eq!(snapshot.offset, offset);
            assert_eq!(snapshot.result.rows.len(), usize::try_from(rows)?);
        }
        Ok(())
    }

    #[test]
    fn table_pages_clamp_to_the_last_page_after_external_deletions() -> Result<()> {
        let (_directory, path, connection, table) = pagination_fixture(1000)?;
        let external = rusqlite::Connection::open(path)?;
        for (remaining, offset, rows) in [
            (1000_u64, 500, 500),
            (501, 500, 1),
            (500, 0, 500),
            (0, 0, 0),
        ] {
            external.execute("DELETE FROM items WHERE id > ?1", [remaining])?;
            let snapshot = connection.start_browse(&table, 500, u64::MAX)?.execute()?;
            assert_eq!(snapshot.total_rows, remaining);
            assert_eq!(snapshot.offset, offset);
            assert_eq!(snapshot.result.rows.len(), rows);
        }
        let cancelled = connection.start_browse(&table, 500, 0)?;
        cancelled.cancel_handle().cancel()?;
        assert!(cancelled.execute().is_err());
        Ok(())
    }

    #[test]
    fn drop_table_and_view_quote_identifiers_and_keep_operation_ownership() -> Result<()> {
        let (_directory, path, connection, _table) = fixture()?;
        let table = TableInfo {
            schema: Some("main".into()),
            name: "odd\"; DROP TABLE items;--".into(),
            is_view: false,
        };
        let view = TableInfo {
            schema: Some("main".into()),
            name: "view\"'name".into(),
            is_view: true,
        };
        let external = rusqlite::Connection::open(&path)?;
        external.execute_batch(&format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY); CREATE VIEW {} AS SELECT 1 AS id;",
            qualified_table(DatabaseKind::Sqlite, &table),
            qualified_table(DatabaseKind::Sqlite, &view)
        ))?;
        let pending = connection.start_query("SELECT 1", 10)?;
        assert!(connection.start_drop_table(&table).is_err());
        drop(pending);
        let cancelled = connection.start_drop_table(&table)?;
        cancelled.cancel_handle().cancel()?;
        assert!(cancelled.execute().is_err());
        let read_only = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path),
                read_only: true,
                ..Default::default()
            },
            Secrets::default(),
        )?;
        assert!(read_only.start_drop_table(&table).is_err());
        assert!(read_only.start_drop_table(&view).is_err());
        connection.start_drop_table(&table)?.execute()?;
        connection.start_drop_table(&view)?.execute()?;
        assert_eq!(
            external.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN (?1,?2)",
                [&table.name, &view.name],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
        assert_eq!(
            connection.query("SELECT COUNT(*) FROM items", 10)?.rows,
            [vec!["3"]]
        );
        Ok(())
    }

    #[test]
    fn writable_sql_reports_dml_counts_and_preserves_typed_values() -> Result<()> {
        let (_directory, _path, connection, _table) = fixture()?;
        assert!(!connection.is_read_only());
        assert_eq!(
            connection
                .query("INSERT INTO items VALUES (2,1,'quote''s',X'1234')", 10)?
                .affected_rows,
            Some(1)
        );
        assert_eq!(
            connection
                .query("UPDATE items SET name='changed' WHERE tenant=2", 10)?
                .affected_rows,
            Some(1)
        );
        assert_eq!(
            connection
                .query("DELETE FROM items WHERE tenant=2", 10)?
                .affected_rows,
            Some(1)
        );
        assert_eq!(
            connection
                .query("SELECT NULL,'NULL',9223372036854775807,X'00ff'", 10)?
                .values,
            vec![vec![
                CellValue::Null,
                CellValue::Text("NULL".into()),
                CellValue::Int(i64::MAX),
                CellValue::Bytes(vec![0, 255])
            ]]
        );
        assert!(
            connection
                .query(
                    "INSERT INTO items VALUES (2,1,'not inserted',NULL); DELETE FROM items",
                    10
                )
                .is_err()
        );
        assert_eq!(
            connection.query("SELECT COUNT(*) FROM items", 10)?.rows,
            [vec!["3"]]
        );
        Ok(())
    }

    #[test]
    fn composite_keys_bulk_update_delete_insert_and_binary_round_trip() -> Result<()> {
        let (_directory, _path, connection, table) = fixture()?;
        let snapshot = connection.start_browse(&table, 100, 0)?.execute()?;
        assert!(snapshot.editability_error.is_none());
        assert_eq!(snapshot.primary_key, [0, 1]);
        let first = snapshot
            .result
            .values
            .first()
            .context("Missing first row")?
            .clone();
        let second = snapshot
            .result
            .values
            .get(1)
            .context("Missing second row")?
            .clone();
        let third = snapshot
            .result
            .values
            .get(2)
            .context("Missing third row")?
            .clone();
        let injection = "O'Reilly\\'; DROP TABLE items; --\0tail".to_string();
        let result = connection
            .start_changes(
                &snapshot,
                vec![
                    RowChange::Update {
                        original: first,
                        values: vec![
                            (2, CellInput::Text(injection.clone())),
                            (3, CellInput::Text("0xab00ff".into())),
                        ],
                    },
                    RowChange::Update {
                        original: second,
                        values: vec![(2, CellInput::Text("NULL".into()))],
                    },
                    RowChange::Delete { original: third },
                    RowChange::Insert {
                        values: vec![
                            (0, CellInput::Text("2".into())),
                            (1, CellInput::Text("4".into())),
                            (2, CellInput::Default),
                            (3, CellInput::Null),
                        ],
                    },
                ],
            )?
            .execute()?;
        assert_eq!(result.affected_rows, 4);
        let rows = connection
            .query(
                "SELECT tenant,id,name,data FROM items ORDER BY tenant,id",
                100,
            )?
            .values;
        assert_eq!(
            rows,
            vec![
                vec![
                    CellValue::Int(1),
                    CellValue::Int(1),
                    CellValue::Text(injection),
                    CellValue::Bytes(vec![0xab, 0, 255])
                ],
                vec![
                    CellValue::Int(1),
                    CellValue::Int(2),
                    CellValue::Text("NULL".into()),
                    CellValue::Null
                ],
                vec![
                    CellValue::Int(2),
                    CellValue::Int(4),
                    CellValue::Text("new".into()),
                    CellValue::Null
                ],
            ]
        );
        Ok(())
    }

    #[test]
    fn concurrent_change_rolls_back_the_entire_batch() -> Result<()> {
        let (_directory, path, connection, table) = fixture()?;
        let snapshot = connection.start_browse(&table, 100, 0)?.execute()?;
        let first = snapshot
            .result
            .values
            .first()
            .context("Missing first row")?
            .clone();
        let second = snapshot
            .result
            .values
            .get(1)
            .context("Missing second row")?
            .clone();
        rusqlite::Connection::open(path)?
            .execute("UPDATE items SET name='someone else' WHERE id=2", [])?;
        let result = connection
            .start_changes(
                &snapshot,
                vec![
                    RowChange::Update {
                        original: first,
                        values: vec![(2, CellInput::Text("must rollback".into()))],
                    },
                    RowChange::Delete { original: second },
                ],
            )?
            .execute();
        assert!(result.is_err());
        assert_eq!(
            connection
                .query("SELECT name FROM items ORDER BY id", 100)?
                .rows,
            [vec!["one"], vec!["someone else"], vec!["NULL"]]
        );
        Ok(())
    }

    #[test]
    fn constraint_failure_rolls_back_previous_changes_and_session_recovers() -> Result<()> {
        let (_directory, _path, connection, table) = fixture()?;
        let snapshot = connection.start_browse(&table, 100, 0)?.execute()?;
        let first = snapshot
            .result
            .values
            .first()
            .context("Missing first row")?
            .clone();
        assert!(
            connection
                .start_changes(
                    &snapshot,
                    vec![
                        RowChange::Delete { original: first },
                        RowChange::Insert {
                            values: vec![
                                (0, CellInput::Text("1".into())),
                                (1, CellInput::Text("2".into()))
                            ]
                        },
                    ]
                )?
                .execute()
                .is_err()
        );
        assert_eq!(
            connection.query("SELECT COUNT(*) FROM items", 10)?.rows,
            [vec!["3"]]
        );
        assert_eq!(
            connection
                .query("UPDATE items SET name='fine' WHERE id=1", 10)?
                .affected_rows,
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn no_key_views_and_read_only_connections_do_not_allow_grid_changes() -> Result<()> {
        let (_directory, path, connection, _table) = fixture()?;
        connection.query("CREATE TABLE no_key(name TEXT)", 10)?;
        let no_key = TableInfo {
            schema: None,
            name: "no_key".into(),
            is_view: false,
        };
        let snapshot = connection.start_browse(&no_key, 10, 0)?.execute()?;
        assert!(
            snapshot
                .editability_error
                .as_deref()
                .is_some_and(|error| error.contains("primary key"))
        );
        assert!(
            connection
                .start_changes(&snapshot, vec![RowChange::Insert { values: vec![] }])
                .is_err()
        );
        let read_only = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path),
                read_only: true,
                ..Default::default()
            },
            Secrets::default(),
        )?;
        assert!(
            read_only
                .query("INSERT INTO no_key VALUES ('no')", 10)
                .is_err()
        );
        let snapshot = read_only.start_browse(&no_key, 10, 0)?.execute()?;
        assert_eq!(
            snapshot.editability_error.as_deref(),
            Some("This connection is read-only")
        );
        Ok(())
    }

    #[test]
    fn pending_batch_cancellation_and_manual_transactions_preserve_data() -> Result<()> {
        let (_directory, _path, connection, table) = fixture()?;
        let snapshot = connection.start_browse(&table, 100, 0)?.execute()?;
        let first = snapshot
            .result
            .values
            .first()
            .context("Missing first row")?
            .clone();
        let operation = connection.start_changes(
            &snapshot,
            vec![RowChange::Delete {
                original: first.clone(),
            }],
        )?;
        assert!(connection.start_query("SELECT 1", 10).is_err());
        operation.cancel_handle().cancel()?;
        assert!(operation.execute().is_err());
        connection.query("BEGIN", 10)?;
        connection.query("INSERT INTO items VALUES (2,1,'manual',NULL)", 10)?;
        assert!(
            connection
                .start_changes(&snapshot, vec![RowChange::Delete { original: first }])?
                .execute()
                .is_err()
        );
        connection.query("ROLLBACK", 10)?;
        assert_eq!(
            connection.query("SELECT COUNT(*) FROM items", 10)?.rows,
            [vec!["3"]]
        );
        Ok(())
    }

    #[test]
    fn filtered_snapshot_returns_only_matching_rows_and_refuses_injected_statements() -> Result<()>
    {
        let (_directory, _path, connection, table) = fixture()?;
        let snapshot = connection
            .start_browse_filtered(&table, Some("tenant=1 AND (id=2 OR name='NULL')"), 100, 0)?
            .execute()?;
        assert_eq!(snapshot.result.rows.len(), 2);
        for filter in [
            "1); DELETE FROM items; --",
            "1) UNION SELECT 1,1,1,1 --",
            "$$($$ = 'x') UNION SELECT 1,1,1,1",
            "name='x\\') OR 1=1 --",
        ] {
            assert!(
                connection
                    .start_browse_filtered(&table, Some(filter), 100, 0)
                    .is_err()
            );
        }
        assert!(validate_filter("name='O''Reilly; --' AND id IN (1,2)").is_ok());
        Ok(())
    }
}
