//! Blocking database operations. Call these methods on a background executor.

mod profiles;
mod schema_edit;
mod table_edit;

pub use profiles::{
    load_profiles, remove_profile, save_profile, save_profile_if_unchanged, update_profile_color,
    update_profile_metadata, update_profile_scope,
};
pub use schema_edit::{ColumnChangeOperation, SchemaEntry, StructureOperation, TableStructure};
pub use table_edit::{
    BrowseOperation, CellInput, CellValue, ChangeOperation, ChangeResult, ColumnMetadata,
    RowChange, TableSnapshot, cell_text,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use dbflux_core::{
    Connection, ConnectionProfile, DataStructure, DbConfig, DbDriver, QueryCancelHandle,
    QueryRequest, ReadOnlyEnforcement, RelationalSchema, SchemaLoadingStrategy, SshAuthMethod,
    SshTunnelConfig, secrecy::SecretString,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const DEFAULT_ROW_LIMIT: u32 = 500;
pub const MAX_ROW_LIMIT: u32 = 10_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseKind {
    #[default]
    Postgres,
    MySql,
    Sqlite,
}

impl DatabaseKind {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::MySql => 3306,
            Self::Sqlite => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: DatabaseKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub path: Option<PathBuf>,
    pub ssl_mode: Option<String>,
    pub ssh: Option<SshConfig>,
    pub read_only: bool,
    pub color: Option<ConnectionColor>,
    pub project_scope: Option<Vec<PathBuf>>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Database".into(),
            kind: DatabaseKind::Postgres,
            host: "localhost".into(),
            port: 5432,
            user: String::new(),
            database: String::new(),
            path: None,
            ssl_mode: None,
            ssh: None,
            read_only: false,
            color: None,
            project_scope: None,
        }
    }
}

impl ConnectionConfig {
    pub fn visible_in_project(&self, roots: &[PathBuf]) -> bool {
        let Some(scope) = self.project_scope.as_deref() else {
            return true;
        };
        match (
            normalize_project_roots(scope),
            normalize_project_roots(roots),
        ) {
            (Ok(scope), Ok(roots)) => scope == roots,
            _ => false,
        }
    }

    fn profile(&self) -> Result<ConnectionProfile> {
        let ssl_mode = self.normalized_ssl_mode()?;
        let ssh_tunnel = self.ssh.as_ref().map(|ssh| SshTunnelConfig {
            host: ssh.host.clone(),
            port: ssh.port,
            user: ssh.user.clone(),
            auth_method: if ssh.use_password {
                SshAuthMethod::Password
            } else {
                SshAuthMethod::PrivateKey {
                    key_path: ssh.key_path.clone(),
                }
            },
            ssh_config_host: ssh.config_host.clone(),
        });
        let config = match self.kind {
            DatabaseKind::Postgres => DbConfig::Postgres {
                use_uri: false,
                uri: None,
                host: self.host.clone(),
                port: self.port,
                user: self.user.clone(),
                database: self.database.clone(),
                ssl_mode,
                ssl_root_cert_path: None,
                ssl_client_cert_path: None,
                ssl_client_key_path: None,
                ssh_tunnel,
                ssh_tunnel_profile_id: None,
            },
            DatabaseKind::MySql => DbConfig::MySQL {
                use_uri: false,
                uri: None,
                host: self.host.clone(),
                port: self.port,
                user: self.user.clone(),
                database: (!self.database.is_empty()).then(|| self.database.clone()),
                ssl_mode,
                ssl_root_cert_path: None,
                ssl_client_cert_path: None,
                ssl_client_key_path: None,
                ssh_tunnel,
                ssh_tunnel_profile_id: None,
            },
            DatabaseKind::Sqlite => {
                ensure!(
                    self.ssh.is_none(),
                    "SQLite uses a local file; SSH is only supported for PostgreSQL and MySQL"
                );
                let path = self
                    .path
                    .as_ref()
                    .context("Choose a SQLite database file")?;
                ensure!(path.is_file(), "SQLite database file does not exist");
                DbConfig::SQLite {
                    path: path.clone(),
                    connection_id: None,
                }
            }
        };
        let mut profile = ConnectionProfile::new(self.name.clone(), config);
        profile.read_only_flag = self.read_only;
        Ok(profile)
    }

    fn normalized_ssl_mode(&self) -> Result<Option<String>> {
        let Some(mode) = self.ssl_mode.as_deref() else {
            return Ok(None);
        };
        let mode = match self.kind {
            DatabaseKind::Postgres => mode.trim().to_ascii_lowercase().replace('_', "-"),
            DatabaseKind::MySql => mode.trim().to_ascii_uppercase().replace('-', "_"),
            DatabaseKind::Sqlite => bail!("SQLite connections do not use a TLS mode"),
        };
        let valid = match self.kind {
            DatabaseKind::Postgres => matches!(
                mode.as_str(),
                "disable" | "allow" | "prefer" | "require" | "verify-ca" | "verify-full"
            ),
            DatabaseKind::MySql => matches!(
                mode.as_str(),
                "DISABLED" | "PREFERRED" | "REQUIRED" | "VERIFY_CA" | "VERIFY_IDENTITY"
            ),
            DatabaseKind::Sqlite => false,
        };
        ensure!(
            valid,
            "Unsupported TLS mode: {mode}. Choose a mode supported by this database engine"
        );
        Ok(Some(mode))
    }
}

/// Normalize local project identities without filesystem access on the UI thread.
pub fn normalize_project_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    use std::path::Component;

    ensure!(
        !roots.is_empty(),
        "A project connection requires at least one local root"
    );
    let mut normalized = Vec::with_capacity(roots.len());
    for root in roots {
        ensure!(
            root.is_absolute(),
            "Project roots must be absolute local paths"
        );
        let mut path = PathBuf::new();
        for component in root.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    path.pop();
                }
                component => path.push(component.as_os_str()),
            }
        }
        normalized.push(path);
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
    pub use_password: bool,
    pub config_host: Option<String>,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            key_path: None,
            use_password: false,
            config_host: None,
        }
    }
}

/// Secrets are intentionally neither serializable nor printable.
#[derive(Default)]
pub struct Secrets {
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    pub is_view: bool,
}

impl TableInfo {
    pub fn select_sql(&self, kind: DatabaseKind, limit: u32, offset: u64) -> String {
        let quote = if kind == DatabaseKind::MySql {
            '`'
        } else {
            '"'
        };
        let quote_identifier = |identifier: &str| {
            format!(
                "{quote}{}{quote}",
                identifier.replace(quote, &format!("{quote}{quote}"))
            )
        };
        let table = self
            .schema
            .as_deref()
            .filter(|schema| !schema.is_empty())
            .map_or_else(
                || quote_identifier(&self.name),
                |schema| {
                    format!(
                        "{}.{}",
                        quote_identifier(schema),
                        quote_identifier(&self.name)
                    )
                },
            );
        format!(
            "SELECT * FROM {table} LIMIT {} OFFSET {offset};",
            limit.clamp(1, MAX_ROW_LIMIT)
        )
    }
}

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub row_limit_reached: bool,
    pub execution_time: Duration,
    pub affected_rows: Option<u64>,
    pub values: Vec<Vec<CellValue>>,
}

#[derive(Clone)]
pub struct DatabaseConnection {
    inner: Arc<ConnectionState>,
    kind: DatabaseKind,
    cancel_handle: Arc<dyn QueryCancelHandle>,
}

impl DatabaseConnection {
    pub fn connect(config: &ConnectionConfig, secrets: Secrets) -> Result<Self> {
        let profile = config.profile()?;
        let driver: Box<dyn DbDriver> = match config.kind {
            DatabaseKind::Postgres => Box::new(dbflux_driver_postgres::PostgresDriver::new()),
            DatabaseKind::MySql => Box::new(dbflux_driver_mysql::MysqlDriver::new(
                dbflux_core::DbKind::MySQL,
            )),
            DatabaseKind::Sqlite => Box::new(dbflux_driver_sqlite::SqliteDriver::new()),
        };
        let password = secrets.password.map(SecretString::from);
        let ssh_secret = secrets.ssh_secret.map(SecretString::from);
        let connection = driver
            .connect_with_secrets(&profile, password.as_ref(), ssh_secret.as_ref())
            .context("Could not connect to the database")?;
        let cancel_handle = connection.cancel_handle();
        Ok(Self {
            inner: Arc::new(ConnectionState {
                connection: Mutex::new(Some(connection)),
                busy: AtomicBool::new(false),
                disconnected: AtomicBool::new(false),
                operation: Mutex::new(None),
                read_only: config.read_only,
                database: config.database.clone(),
            }),
            kind: config.kind,
            cancel_handle,
        })
    }

    pub fn list_tables(&self) -> Result<Vec<TableInfo>> {
        ensure!(
            !self.inner.disconnected.load(Ordering::Acquire),
            "Database is disconnected"
        );
        let guard = self.inner.lock()?;
        let connection = guard.as_ref().context("Database is disconnected")?;
        let schema = if connection.schema_loading_strategy()
            == SchemaLoadingStrategy::LazyPerDatabase
            && !self.inner.database.is_empty()
        {
            let schema = connection
                .schema_for_database(&self.inner.database)
                .context("Could not load tables for the selected database")?;
            RelationalSchema {
                schemas: vec![schema],
                ..Default::default()
            }
        } else {
            let schema = connection
                .schema()
                .context("Could not load database tables")?;
            let DataStructure::Relational(schema) = schema.structure else {
                bail!("This connection does not expose relational tables");
            };
            schema
        };
        let mut tables = Vec::new();
        for table in schema.tables {
            tables.push(TableInfo {
                schema: table.schema,
                name: table.name,
                is_view: false,
            });
        }
        for view in schema.views {
            tables.push(TableInfo {
                schema: view.schema,
                name: view.name,
                is_view: true,
            });
        }
        for schema in schema.schemas {
            for table in schema.tables {
                tables.push(TableInfo {
                    schema: Some(schema.name.clone()),
                    name: table.name,
                    is_view: false,
                });
            }
            for view in schema.views {
                tables.push(TableInfo {
                    schema: Some(schema.name.clone()),
                    name: view.name,
                    is_view: true,
                });
            }
        }
        tables.sort_by(|left, right| (&left.schema, &left.name).cmp(&(&right.schema, &right.name)));
        Ok(tables)
    }

    pub fn query(&self, sql: &str, limit: u32) -> Result<QueryResult> {
        self.start_query(sql, limit)?.execute()
    }

    pub fn start_query(&self, sql: &str, limit: u32) -> Result<QueryOperation> {
        ensure!(!sql.trim().is_empty(), "Enter a SQL query");
        ensure!(
            dbflux_core::QueryLanguage::Sql.statement_count(sql) == 1,
            "Run one SQL statement at a time"
        );
        ensure!(
            !self.inner.disconnected.load(Ordering::Acquire),
            "Database is disconnected"
        );
        self.inner
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("A query is already running on this connection; finish or cancel it before starting another"))?;
        let control = Arc::new(OperationControl {
            state: Mutex::new(OperationStatus {
                phase: OperationPhase::Pending,
                cancelled: false,
            }),
            cancel_handle: self.cancel_handle.clone(),
        });
        let operation = QueryOperation {
            connection: self.inner.clone(),
            control,
            sql: sql.to_owned(),
            limit: limit.clamp(1, MAX_ROW_LIMIT),
        };
        *self
            .inner
            .operation
            .lock()
            .map_err(|_| anyhow!("Database query state failed; reconnect to continue"))? =
            Some(Arc::downgrade(&operation.control));
        Ok(operation)
    }

    pub fn browse_sql(&self, table: &TableInfo, limit: u32, offset: u64) -> String {
        table.select_sql(self.kind, limit, offset)
    }

    pub fn browse(&self, table: &TableInfo, limit: u32, offset: u64) -> Result<QueryResult> {
        self.query(&self.browse_sql(table, limit, offset), limit)
    }

    pub fn cancel(&self) -> Result<()> {
        let control = self
            .inner
            .operation
            .lock()
            .map_err(|_| anyhow!("Database query state failed; reconnect to continue"))?
            .clone();
        if let Some(control) = control {
            CancelQuery(control).cancel()?;
        }
        Ok(())
    }

    /// Prevents every session handle from starting work while teardown is queued.
    /// This only sets an atomic flag and may be called on the UI thread.
    pub fn mark_disconnected(&self) {
        self.inner.disconnected.store(true, Ordering::Release);
    }

    pub fn disconnect(&self) -> Result<()> {
        self.mark_disconnected();
        self.cancel()?;
        let connection = self.inner.lock()?.take();
        if let Some(mut connection) = connection {
            connection
                .close()
                .context("Could not close the database connection")?;
        }
        Ok(())
    }
}

pub struct QueryOperation {
    connection: Arc<ConnectionState>,
    control: Arc<OperationControl>,
    sql: String,
    limit: u32,
}

impl QueryOperation {
    pub fn cancel_handle(&self) -> CancelQuery {
        CancelQuery(Arc::downgrade(&self.control))
    }

    pub fn execute(self) -> Result<QueryResult> {
        let guard = self.connection.lock()?;
        ensure!(
            !self.connection.disconnected.load(Ordering::Acquire),
            "Database is disconnected"
        );
        let connection = guard.as_ref().context("Database is disconnected")?;
        {
            let mut state = self
                .control
                .state
                .lock()
                .map_err(|_| anyhow!("Database query cancellation state failed"))?;
            ensure!(!state.cancelled, "Query cancelled");
            state.phase = OperationPhase::Running;
        }
        let sql = &self.sql;
        let request = QueryRequest::new(sql)
            .with_limit(self.limit)
            .with_read_only(if self.connection.read_only {
                ReadOnlyEnforcement::Required
            } else {
                ReadOnlyEnforcement::None
            });
        let result = connection.execute(&request).context("Query failed")?;
        QueryResult::from_driver(result)
    }
}

impl QueryResult {
    fn from_driver(result: dbflux_core::QueryResult) -> Result<Self> {
        ensure!(
            result.additional_results.is_empty(),
            "Run one query at a time; multiple result sets are not yet supported"
        );
        let row_limit_reached = result.rows_truncated();
        Ok(QueryResult {
            columns: result
                .columns
                .into_iter()
                .map(|column| column.name)
                .collect(),
            rows: result
                .rows
                .iter()
                .map(|row| row.iter().cloned().map(display_value).collect())
                .collect(),
            values: result.rows,
            affected_rows: result.affected_rows,
            row_limit_reached,
            execution_time: result.execution_time,
        })
    }
}

impl Drop for QueryOperation {
    fn drop(&mut self) {
        let mut state = match self.control.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.phase = OperationPhase::Finished;
        self.connection.busy.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct CancelQuery(Weak<OperationControl>);

impl CancelQuery {
    pub fn cancel(&self) -> Result<()> {
        let Some(control) = self.0.upgrade() else {
            return Ok(());
        };
        for _ in 0..100 {
            {
                let mut state = control
                    .state
                    .lock()
                    .map_err(|_| anyhow!("Database query cancellation state failed"))?;
                state.cancelled = true;
                if state.phase != OperationPhase::Running {
                    return Ok(());
                }
                // Completion takes this lock before releasing the reservation,
                // so retries can never interrupt a subsequent console's query.
                control
                    .cancel_handle
                    .cancel()
                    .context("Could not cancel the query")?;
            }
            // Driver cancellation sent just before the first statement starts
            // may be ignored. Retry briefly until this operation has stopped.
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

struct OperationControl {
    state: Mutex<OperationStatus>,
    cancel_handle: Arc<dyn QueryCancelHandle>,
}

struct OperationStatus {
    phase: OperationPhase,
    cancelled: bool,
}

#[derive(PartialEq)]
enum OperationPhase {
    Pending,
    Running,
    Finished,
}

struct ConnectionState {
    connection: Mutex<Option<Box<dyn Connection>>>,
    busy: AtomicBool,
    disconnected: AtomicBool,
    operation: Mutex<Option<Weak<OperationControl>>>,
    read_only: bool,
    database: String,
}

fn display_value(value: dbflux_core::Value) -> String {
    match value {
        dbflux_core::Value::DateTime(value) => value.to_rfc3339(),
        dbflux_core::Value::Time(value) => value.to_string(),
        value => value.as_display_string_truncated(usize::MAX),
    }
}

impl ConnectionState {
    fn lock(&self) -> Result<MutexGuard<'_, Option<Box<dyn Connection>>>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("Database connection task failed; reconnect to continue"))
    }
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        // The last UI handle can be dropped on the foreground thread. Driver
        // and SSH teardown can block, so the connection is released elsewhere.
        let connection = match self.connection.get_mut() {
            Ok(connection) => connection.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(mut connection) = connection {
            std::thread::spawn(move || {
                if connection.close().is_err() {
                    // Report only the operation, not server addresses or credentials.
                    eprintln!("Database connection cleanup failed");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, DatabaseConnection)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("database.sqlite");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);\n\
             INSERT INTO items VALUES (1, 'one'), (2, 'two'), (3, 'three');\n\
             CREATE VIEW first_item AS SELECT * FROM items WHERE id = 1;",
        )?;
        let connection = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path),
                read_only: true,
                ..Default::default()
            },
            Secrets::default(),
        )?;
        Ok((directory, connection))
    }

    #[test]
    fn sqlite_lists_and_browses_real_tables() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let tables = connection.list_tables()?;
        let table = tables
            .iter()
            .find(|table| table.name == "items")
            .context("Missing table")?;
        assert!(
            tables
                .iter()
                .any(|table| table.name == "first_item" && table.is_view)
        );
        let result = connection.browse(table, 2, 1)?;
        assert_eq!(result.columns, ["id", "name"]);
        assert_eq!(result.rows, [vec!["2", "two"], vec!["3", "three"]]);
        connection.disconnect()?;
        assert!(connection.query("SELECT 1", 10).is_err());
        Ok(())
    }

    #[test]
    fn queries_are_bounded_and_read_only_even_with_comments() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let result = connection.query("/* preview */ SELECT * FROM items ORDER BY id", 2)?;
        assert_eq!(result.rows.len(), 2);
        assert!(result.row_limit_reached);
        for sql in [
            "/* SELECT */ DELETE FROM items",
            "SELECT 1; DELETE FROM items",
            "PRAGMA query_only = OFF; DELETE FROM items",
            "WITH source AS (SELECT 1) DELETE FROM items",
        ] {
            assert!(
                connection.query(sql, 10).is_err(),
                "Write must be refused: {sql}"
            );
        }
        assert_eq!(
            connection.query("SELECT count(*) FROM items", 10)?.rows,
            [vec!["3"]]
        );
        Ok(())
    }

    #[test]
    fn cancellation_does_not_wait_for_the_query_lock() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let operation = connection.start_query(
            "WITH RECURSIVE numbers(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000000000) SELECT sum(value) FROM numbers", 10,
        )?;
        let control = operation.control.clone();
        let cancel = operation.cancel_handle();
        let worker = std::thread::spawn(move || operation.execute());
        for _ in 0..200 {
            if control
                .state
                .lock()
                .map_err(|_| anyhow!("Query state poisoned"))?
                .phase
                == OperationPhase::Running
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        cancel.cancel()?;
        ensure!(
            worker.is_finished(),
            "Query cancellation did not complete promptly"
        );
        let result = worker
            .join()
            .map_err(|_| anyhow!("Query worker panicked"))?;
        assert!(result.is_err());
        assert_eq!(connection.query("SELECT 1", 10)?.rows, [vec!["1"]]);
        Ok(())
    }

    #[test]
    fn reservations_and_cancel_handles_belong_to_one_query() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let first = connection.start_query("SELECT 1", 10)?;
        let stale_cancel = first.cancel_handle();
        assert!(connection.start_query("SELECT 2", 10).is_err());
        first.execute()?;
        let second = connection.start_query("SELECT 2", 10)?;
        stale_cancel.cancel()?;
        assert_eq!(second.execute()?.rows, [vec!["2"]]);

        let pending = connection.start_query("SELECT 3", 10)?;
        pending.cancel_handle().cancel()?;
        assert!(pending.execute().is_err());
        let dropped = connection.start_query("SELECT 4", 10)?;
        drop(dropped);
        assert_eq!(connection.query("SELECT 5", 10)?.rows, [vec!["5"]]);
        Ok(())
    }

    #[test]
    fn marking_disconnected_immediately_invalidates_clones_and_pending_queries() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let console_connection = connection.clone();
        let pending = console_connection.start_query("SELECT 1", 10)?;
        connection.mark_disconnected();
        assert!(pending.execute().is_err());
        assert!(console_connection.start_query("SELECT 2", 10).is_err());
        assert!(console_connection.list_tables().is_err());
        connection.disconnect()?;
        Ok(())
    }

    #[test]
    fn result_values_preserve_long_text_and_integer_precision() -> Result<()> {
        let (_directory, connection) = fixture()?;
        let text = "данные".repeat(400);
        let result = connection.query(&format!("SELECT '{text}', 9223372036854775807"), 10)?;
        assert_eq!(result.rows, [vec![text, "9223372036854775807".into()]]);
        Ok(())
    }

    #[test]
    fn browse_quotes_identifiers_and_clamps_limit() {
        let table = TableInfo {
            schema: Some("my\"schema".into()),
            name: "weird\"table".into(),
            is_view: false,
        };
        assert_eq!(
            table.select_sql(DatabaseKind::Postgres, u32::MAX, 3),
            "SELECT * FROM \"my\"\"schema\".\"weird\"\"table\" LIMIT 10000 OFFSET 3;"
        );
        let table = TableInfo {
            schema: Some("my`schema".into()),
            name: "weird`table".into(),
            is_view: false,
        };
        assert_eq!(
            table.select_sql(DatabaseKind::MySql, 0, 0),
            "SELECT * FROM `my``schema`.`weird``table` LIMIT 1 OFFSET 0;"
        );
    }

    #[test]
    fn serialized_profiles_contain_no_password_fields() -> Result<()> {
        let profile = ConnectionConfig::default();
        let json = serde_json::to_value(profile)?;
        assert!(json.get("password").is_none());
        assert!(json.get("ssh_secret").is_none());
        Ok(())
    }

    #[test]
    fn legacy_profiles_remain_global_and_colors_round_trip() -> Result<()> {
        let legacy: ConnectionConfig = serde_json::from_str(r#"{"id":"legacy","name":"Legacy"}"#)?;
        assert_eq!(legacy.color, None);
        assert_eq!(legacy.project_scope, None);
        assert!(legacy.visible_in_project(&[]));
        for (color, encoded) in [
            (ConnectionColor::Red, "red"),
            (ConnectionColor::Orange, "orange"),
            (ConnectionColor::Yellow, "yellow"),
            (ConnectionColor::Green, "green"),
            (ConnectionColor::Blue, "blue"),
            (ConnectionColor::Purple, "purple"),
        ] {
            let config = ConnectionConfig {
                color: Some(color),
                ..Default::default()
            };
            let json = serde_json::to_value(&config)?;
            assert_eq!(
                json.get("color").and_then(serde_json::Value::as_str),
                Some(encoded)
            );
            assert_eq!(
                serde_json::from_value::<ConnectionConfig>(json)?.color,
                Some(color)
            );
        }
        Ok(())
    }

    #[test]
    fn project_visibility_requires_the_same_nonempty_normalized_root_set() -> Result<()> {
        let roots = vec![
            PathBuf::from("/projects/backend"),
            PathBuf::from("/projects/frontend"),
        ];
        let config = ConnectionConfig {
            project_scope: Some(vec![
                PathBuf::from("/projects/frontend/./"),
                PathBuf::from("/projects/temporary/../backend"),
                PathBuf::from("/projects/backend"),
            ]),
            ..Default::default()
        };
        assert!(config.visible_in_project(&roots));
        assert!(!config.visible_in_project(&roots[..1]));
        assert!(!config.visible_in_project(&[]));
        assert!(!config.visible_in_project(&[PathBuf::from("projects/backend")]));
        assert!(!config.visible_in_project(&[PathBuf::from("/projects/another")]));
        let empty_scope = ConnectionConfig {
            project_scope: Some(Vec::new()),
            ..Default::default()
        };
        assert!(!empty_scope.visible_in_project(&[]));
        assert!(normalize_project_roots(&[]).is_err());
        assert!(normalize_project_roots(&[PathBuf::from("relative")]).is_err());
        assert_eq!(
            normalize_project_roots(&[PathBuf::from("/../projects/backend")])?,
            vec![PathBuf::from("/projects/backend")]
        );
        Ok(())
    }

    #[test]
    fn tls_modes_are_normalized_and_unknown_modes_are_rejected() -> Result<()> {
        let postgres = ConnectionConfig {
            ssl_mode: Some(" VERIFY_FULL ".into()),
            ..Default::default()
        };
        assert_eq!(postgres.normalized_ssl_mode()?, Some("verify-full".into()));
        let mysql = ConnectionConfig {
            kind: DatabaseKind::MySql,
            ssl_mode: Some(" verify-identity ".into()),
            ..Default::default()
        };
        assert_eq!(mysql.normalized_ssl_mode()?, Some("VERIFY_IDENTITY".into()));
        for kind in [DatabaseKind::Postgres, DatabaseKind::MySql] {
            let invalid = ConnectionConfig {
                kind,
                ssl_mode: Some("requre".into()),
                ..Default::default()
            };
            assert!(invalid.profile().is_err());
        }
        assert_eq!(ConnectionConfig::default().normalized_ssl_mode()?, None);
        Ok(())
    }

    #[test]
    fn postgres_required_tls_never_sends_plaintext_startup() -> Result<()> {
        use std::io::{Read as _, Write as _};
        for mode in ["require", "verify-ca", "verify-full"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let server = std::thread::spawn(move || -> Result<usize> {
                let (mut socket, _) = listener.accept()?;
                socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = [0; 8];
                socket.read_exact(&mut request)?;
                ensure!(
                    request == [0, 0, 0, 8, 4, 210, 22, 47],
                    "Expected PostgreSQL SSLRequest"
                );
                socket.write_all(b"N")?;
                let mut after_refusal = [0];
                Ok(socket.read(&mut after_refusal)?)
            });
            let config = ConnectionConfig {
                host: "127.0.0.1".into(),
                port: address.port(),
                user: "reader".into(),
                database: "test".into(),
                ssl_mode: Some(mode.into()),
                ..Default::default()
            };
            assert!(DatabaseConnection::connect(&config, Secrets::default()).is_err());
            let sent = server
                .join()
                .map_err(|_| anyhow!("TLS probe server panicked"))??;
            assert_eq!(
                sent, 0,
                "{mode} must not downgrade to a plaintext startup packet"
            );
        }
        Ok(())
    }
}
