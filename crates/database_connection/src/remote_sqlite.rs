#![allow(clippy::result_large_err)]

use crate::{ConnectionConfig, Secrets};
use anyhow::{Context as _, Result};
use dbflux_core::{
    ColumnInfo, ColumnKind, ColumnMeta, Connection, ConstraintInfo, DbError, DbKind,
    DriverCapabilities, DriverMetadata, ForeignKeyInfo, IndexData, IndexInfo, QueryCancelHandle,
    QueryHandle, QueryRequest, QueryResult, RelationalSchema, SchemaFeatures,
    SchemaLoadingStrategy, SchemaSnapshot, SqlDialect, TableInfo, Value, ViewInfo,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value as JsonValue, json};
use ssh2::{Channel, Session};
use std::{
    io::{ErrorKind, Read, Write},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const WORKER_SOURCE: &str = include_str!("remote_sqlite.py");
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

static METADATA: LazyLock<DriverMetadata> = LazyLock::new(|| {
    let mut metadata = dbflux_driver_sqlite::METADATA.clone();
    metadata.description = "SQLite file on an SSH server".into();
    metadata.capabilities = DriverCapabilities::VIEWS
        | DriverCapabilities::REQUEST_ROW_LIMIT
        | DriverCapabilities::INDEXES
        | DriverCapabilities::FOREIGN_KEYS
        | DriverCapabilities::PAGINATION
        | DriverCapabilities::SORTING
        | DriverCapabilities::FILTERING
        | DriverCapabilities::EXPORT_CSV
        | DriverCapabilities::EXPORT_JSON
        | DriverCapabilities::QUERY_CANCELLATION
        | DriverCapabilities::TRANSACTIONAL_DDL
        | DriverCapabilities::MULTI_STATEMENT;
    metadata.mutation = None;
    metadata.ddl = None;
    if let Some(query) = metadata.query.as_mut() {
        query.max_query_parameters = 0;
    }
    metadata
});

pub(super) fn connect(config: &ConnectionConfig, secrets: &Secrets) -> Result<Box<dyn Connection>> {
    let tunnel = config
        .ssh_tunnel_config()
        .context("Remote SQLite requires an SSH server")?;
    let path = config
        .path
        .as_ref()
        .and_then(|path| path.to_str())
        .context("Enter a UTF-8 SQLite file path on the SSH server")?;
    let session = dbflux_ssh::establish_session(&tunnel, secrets.ssh_secret.as_deref())
        .context("Could not authenticate to the SQLite SSH server")?;
    let mut channel = session
        .channel_session()
        .context("Could not open an SSH execution channel")?;
    channel
        .exec(&worker_command())
        .context("Could not start Python 3 for remote SQLite")?;
    session.set_blocking(false);

    let connection = RemoteSqliteConnection {
        worker: Mutex::new(Worker {
            session: Some(session),
            channel: Some(channel),
            next_id: 0,
        }),
        cancellation: Arc::new(Cancellation::default()),
    };
    connection
        .request(
            "open",
            json!({ "path": path, "read_only": config.read_only }),
            Some(STARTUP_TIMEOUT),
        )
        .context("Could not open remote SQLite. The SSH server must have python3 with its sqlite3 module, and the database file must already exist")?;
    Ok(Box::new(connection))
}

fn worker_command() -> String {
    // Only bundled source enters the shell command; file paths and SQL travel over stdin.
    format!("python3 -u -c '{}'", WORKER_SOURCE.replace('\'', "'\\''"))
}

#[derive(Default)]
struct Cancellation {
    active_id: AtomicU64,
    cancelled_id: AtomicU64,
}

impl QueryCancelHandle for Cancellation {
    fn cancel(&self) -> Result<(), DbError> {
        let active_id = self.active_id.load(Ordering::Acquire);
        if active_id != 0 {
            self.cancelled_id.store(active_id, Ordering::Release);
        }
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        let active_id = self.active_id.load(Ordering::Acquire);
        active_id != 0 && self.cancelled_id.load(Ordering::Acquire) == active_id
    }
}

struct ActiveRequest<'a>(&'a Cancellation);

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.0.active_id.store(0, Ordering::Release);
    }
}

struct RemoteSqliteConnection {
    worker: Mutex<Worker>,
    cancellation: Arc<Cancellation>,
}

impl RemoteSqliteConnection {
    fn request(
        &self,
        operation: &str,
        parameters: JsonValue,
        timeout: Option<Duration>,
    ) -> Result<JsonValue, DbError> {
        self.worker
            .lock()
            .map_err(|_| {
                DbError::connection_failed(
                    "Remote SQLite connection state failed; reconnect to continue",
                )
            })?
            .request(operation, parameters, &self.cancellation, timeout)
    }
}

impl Connection for RemoteSqliteConnection {
    fn metadata(&self) -> &DriverMetadata {
        &METADATA
    }

    fn ping(&self) -> Result<(), DbError> {
        self.execute(&QueryRequest::new("SELECT 1").with_limit(1))?;
        Ok(())
    }

    fn close(&mut self) -> Result<(), DbError> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| DbError::connection_failed("Remote SQLite connection state failed"))?;
        if worker.channel.is_some() {
            let result = worker.request(
                "close",
                json!({}),
                &self.cancellation,
                Some(STARTUP_TIMEOUT),
            );
            worker.disconnect();
            result?;
        }
        Ok(())
    }

    fn execute(&self, request: &QueryRequest) -> Result<QueryResult, DbError> {
        validate_request(request)?;
        let start = Instant::now();
        let result = self.request(
            "execute",
            json!({
                "sql": request.sql,
                "limit": request.limit,
                "read_only": request.read_only.is_required(),
            }),
            None,
        )?;
        let result: ExecutionResult = decode(result)?;
        result.into_query_result(start.elapsed())
    }

    fn cancel(&self, _handle: &QueryHandle) -> Result<(), DbError> {
        self.cancel_active()
    }

    fn cancel_active(&self) -> Result<(), DbError> {
        self.cancellation.cancel()
    }

    fn cancel_handle(&self) -> Arc<dyn QueryCancelHandle> {
        self.cancellation.clone()
    }

    fn cleanup_after_cancel(&self) -> Result<(), DbError> {
        self.request("cleanup", json!({}), Some(STARTUP_TIMEOUT))?;
        Ok(())
    }

    fn schema(&self) -> Result<SchemaSnapshot, DbError> {
        let schema: RemoteSchema = decode(self.request("schema", json!({}), None)?)?;
        Ok(SchemaSnapshot::relational(RelationalSchema {
            current_database: Some("main".into()),
            tables: schema
                .tables
                .into_iter()
                .map(RemoteTable::into_table)
                .collect(),
            views: schema.views,
            ..Default::default()
        }))
    }

    fn table_details(
        &self,
        _database: &str,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DbError> {
        let table: RemoteTable = decode(self.request(
            "table_details",
            json!({ "schema": schema, "table": table }),
            None,
        )?)?;
        Ok(table.into_table())
    }

    fn execute_atomic_row_changes(
        &self,
        _schema: Option<&str>,
        _table: &str,
        statements: &[String],
    ) -> Result<u64, DbError> {
        let result: AtomicResult =
            decode(self.request("atomic", json!({ "statements": statements }), None)?)?;
        Ok(result.affected_rows)
    }

    fn kind(&self) -> DbKind {
        DbKind::SQLite
    }

    fn schema_loading_strategy(&self) -> SchemaLoadingStrategy {
        SchemaLoadingStrategy::SingleDatabase
    }

    fn schema_features(&self) -> SchemaFeatures {
        SchemaFeatures::FOREIGN_KEYS
    }

    fn supports_transactional_ddl(&self) -> bool {
        true
    }

    fn dialect(&self) -> &dyn SqlDialect {
        &dbflux_driver_sqlite::driver::SqliteDialect
    }
}

fn validate_request(request: &QueryRequest) -> Result<(), DbError> {
    if !request.params.is_empty() {
        return Err(DbError::NotSupported(
            "Remote SQLite bind parameters are unsupported; nothing was executed".into(),
        ));
    }
    if request.statement_timeout.is_some() {
        return Err(DbError::NotSupported(
            "Remote SQLite statement timeouts are unsupported; nothing was executed".into(),
        ));
    }
    if request.execution_context.is_some() {
        return Err(DbError::NotSupported(
            "Remote SQLite execution contexts are unsupported; nothing was executed".into(),
        ));
    }
    Ok(())
}

struct Worker {
    session: Option<Session>,
    channel: Option<Channel>,
    next_id: u64,
}

impl Worker {
    fn disconnect(&mut self) {
        self.channel.take();
        self.session.take();
    }

    fn request(
        &mut self,
        operation: &str,
        mut parameters: JsonValue,
        cancellation: &Cancellation,
        timeout: Option<Duration>,
    ) -> Result<JsonValue, DbError> {
        let channel = self.channel.as_mut().ok_or_else(|| {
            DbError::connection_failed("Remote SQLite is disconnected; reconnect to continue")
        })?;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            DbError::connection_failed("Remote SQLite request counter exhausted; reconnect")
        })?;
        let request_id = self.next_id;
        let parameters_object = parameters.as_object_mut().ok_or_else(|| {
            DbError::Parse("Remote SQLite request parameters must be an object".into())
        })?;
        parameters_object.insert("id".into(), json!(request_id));
        parameters_object.insert("operation".into(), json!(operation));
        let mut request =
            serde_json::to_vec(&parameters).map_err(|error| DbError::Parse(error.to_string()))?;
        request.push(b'\n');
        cancellation.active_id.store(request_id, Ordering::Release);
        let _active_request = ActiveRequest(cancellation);
        let response = exchange(channel, &request, request_id, cancellation, timeout);
        match response {
            Ok(response) if response.ok => Ok(response.result),
            Ok(response) if response.cancelled => Err(DbError::Cancelled),
            Ok(response) => Err(DbError::query_failed(response.error.unwrap_or_else(|| {
                "Remote SQLite operation failed without an error message".into()
            }))),
            Err(error) => {
                self.disconnect();
                Err(DbError::connection_failed(format!(
                    "{error}. The SSH connection was closed. A statement may already have completed on the server; verify its result before retrying any changes"
                )))
            }
        }
    }
}

fn exchange(
    channel: &mut Channel,
    request: &[u8],
    request_id: u64,
    cancellation: &Cancellation,
    timeout: Option<Duration>,
) -> Result<Response> {
    let started = Instant::now();
    let mut sent = 0;
    let cancel_request = format!("{{\"id\":{request_id},\"operation\":\"cancel\"}}\n");
    let mut cancel_sent = 0;
    let mut cancelled_at = None;
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let mut pending_response = None;
    let mut buffer = [0; 16 * 1024];
    loop {
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            anyhow::bail!("Remote SQLite worker did not respond before the connection timeout");
        }
        let cancelled = cancellation.cancelled_id.load(Ordering::Acquire) == request_id;
        if cancelled {
            let cancelled_at = cancelled_at.get_or_insert_with(Instant::now);
            anyhow::ensure!(
                cancelled_at.elapsed() < CANCEL_TIMEOUT,
                "Remote SQLite did not acknowledge cancellation"
            );
        }

        let mut progressed = false;
        if let Some(remaining) = request
            .get(sent..)
            .filter(|remaining| !remaining.is_empty())
        {
            progressed |= write_available(channel, remaining, &mut sent)?;
        } else if cancelled
            && let Some(remaining) = cancel_request
                .as_bytes()
                .get(cancel_sent..)
                .filter(|remaining| !remaining.is_empty())
        {
            progressed |= write_available(channel, remaining, &mut cancel_sent)?;
        }

        // A partially written cancel frame must finish before the next request uses stdin.
        if (cancel_sent == 0 || cancel_sent == cancel_request.len())
            && let Some(response) = pending_response.take()
        {
            return Ok(response);
        }

        match channel.stderr().read(&mut buffer) {
            Ok(count) => {
                let remaining = 8192_usize.saturating_sub(diagnostics.len());
                if let Some(bytes) = buffer.get(..count.min(remaining)) {
                    diagnostics.extend_from_slice(bytes);
                }
                progressed |= count > 0;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("Could not read the SSH worker diagnostics"),
        }
        if pending_response.is_some() {
            if !progressed {
                std::thread::sleep(Duration::from_millis(5));
            }
            continue;
        }
        match channel.read(&mut buffer) {
            Ok(0) => {
                let diagnostics = String::from_utf8_lossy(&diagnostics);
                anyhow::bail!(
                    "Remote SQLite worker stopped unexpectedly: {}",
                    diagnostics.trim()
                );
            }
            Ok(count) => {
                if let Some(bytes) = buffer.get(..count) {
                    output.extend_from_slice(bytes);
                }
                anyhow::ensure!(
                    output.len() <= MAX_RESPONSE_BYTES,
                    "Remote SQLite response exceeded 64 MiB; use a smaller result set"
                );
                if let Some(newline) = output.iter().position(|byte| *byte == b'\n') {
                    let payload = output
                        .get(..newline)
                        .context("Invalid worker response length")?;
                    let response: Response = serde_json::from_slice(payload)
                        .context("Remote SQLite returned an invalid response")?;
                    anyhow::ensure!(
                        response.id == request_id,
                        "Remote SQLite response ID did not match the active request"
                    );
                    if cancel_sent == 0 || cancel_sent == cancel_request.len() {
                        return Ok(response);
                    }
                    pending_response = Some(response);
                }
                progressed = true;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("Could not read the SSH worker response"),
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn write_available(channel: &mut Channel, bytes: &[u8], sent: &mut usize) -> Result<bool> {
    match channel.write(bytes) {
        Ok(0) => anyhow::bail!("SSH worker input closed unexpectedly"),
        Ok(count) => {
            *sent += count;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error).context("Could not send a request to the SSH worker"),
    }
}

#[derive(Deserialize)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(default)]
    result: JsonValue,
    error: Option<String>,
    #[serde(default)]
    cancelled: bool,
}

fn decode<T: DeserializeOwned>(value: JsonValue) -> Result<T, DbError> {
    serde_json::from_value(value)
        .map_err(|error| DbError::Parse(format!("Invalid remote SQLite result: {error}")))
}

#[derive(Deserialize)]
struct RemoteSchema {
    tables: Vec<RemoteTable>,
    views: Vec<ViewInfo>,
}

#[derive(Deserialize)]
struct RemoteTable {
    name: String,
    schema: Option<String>,
    columns: Option<Vec<ColumnInfo>>,
    indexes: Option<Vec<IndexInfo>>,
    foreign_keys: Option<Vec<ForeignKeyInfo>>,
    constraints: Option<Vec<ConstraintInfo>>,
    #[serde(default)]
    pseudo_columns: Vec<String>,
}

impl RemoteTable {
    fn into_table(self) -> TableInfo {
        TableInfo {
            name: self.name,
            schema: self.schema,
            columns: self.columns,
            pseudo_columns: self.pseudo_columns.into_boxed_slice(),
            indexes: self.indexes.map(IndexData::Relational),
            foreign_keys: self.foreign_keys,
            constraints: self.constraints,
            sample_fields: None,
            presentation: Default::default(),
            child_items: None,
            storage_hints: None,
        }
    }
}

#[derive(Deserialize)]
struct AtomicResult {
    affected_rows: u64,
}

#[derive(Deserialize)]
struct ExecutionResult {
    results: Vec<ResultSet>,
}

impl ExecutionResult {
    fn into_query_result(self, duration: Duration) -> Result<QueryResult, DbError> {
        let mut results = self.results.into_iter();
        let mut result = match results.next() {
            Some(result) => result.into_query_result(duration)?,
            None => QueryResult::empty(),
        };
        for additional in results {
            result.push_additional_result(additional.into_query_result(Duration::ZERO)?);
        }
        Ok(result)
    }
}

#[derive(Deserialize)]
struct ResultSet {
    columns: Vec<String>,
    rows: Vec<Vec<RemoteValue>>,
    affected_rows: Option<u64>,
    truncated: bool,
}

impl ResultSet {
    fn into_query_result(self, duration: Duration) -> Result<QueryResult, DbError> {
        let columns = self
            .columns
            .into_iter()
            .map(|name| ColumnMeta {
                name,
                type_name: String::new(),
                kind: ColumnKind::Unknown,
                nullable: true,
                is_primary_key: false,
            })
            .collect();
        let rows = self
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(RemoteValue::into_value).collect())
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = QueryResult::table(columns, rows, self.affected_rows, duration);
        result.set_rows_truncated(self.truncated);
        Ok(result)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum RemoteValue {
    Null,
    Integer(String),
    Real(String),
    Text(String),
    Blob(String),
}

impl RemoteValue {
    fn into_value(self) -> Result<Value, DbError> {
        match self {
            Self::Null => Ok(Value::Null),
            Self::Text(value) => Ok(Value::Text(value)),
            Self::Integer(value) => value
                .parse()
                .map(Value::Int)
                .map_err(|error| DbError::Parse(format!("Invalid remote SQLite integer: {error}"))),
            Self::Real(value) => value.parse().map(Value::Float).map_err(|error| {
                DbError::Parse(format!("Invalid remote SQLite real number: {error}"))
            }),
            Self::Blob(value) => {
                if value.len() % 2 != 0 {
                    return Err(DbError::Parse("Invalid remote SQLite blob length".into()));
                }
                value
                    .as_bytes()
                    .chunks_exact(2)
                    .map(|pair| {
                        let text = std::str::from_utf8(pair)
                            .map_err(|error| DbError::Parse(error.to_string()))?;
                        u8::from_str_radix(text, 16)
                            .map_err(|error| DbError::Parse(error.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_values_preserve_sqlite_integer_range_and_binary_data() -> Result<()> {
        let result: ExecutionResult = decode(json!({
            "results": [{
                "columns": ["smallest", "largest", "bytes", "text", "infinity", "empty"],
                "rows": [[
                    {"type": "integer", "value": "-9223372036854775808"},
                    {"type": "integer", "value": "9223372036854775807"},
                    {"type": "blob", "value": "00017fff"},
                    {"type": "text", "value": "line\n\u{0}строка"},
                    {"type": "real", "value": "inf"},
                    {"type": "null"}
                ]],
                "affected_rows": null,
                "truncated": true
            }]
        }))?;
        let result = result.into_query_result(Duration::ZERO)?;
        assert_eq!(
            result.rows,
            vec![vec![
                Value::Int(i64::MIN),
                Value::Int(i64::MAX),
                Value::Bytes(vec![0, 1, 127, 255]),
                Value::Text("line\n\u{0}строка".into()),
                Value::Float(f64::INFINITY),
                Value::Null,
            ]]
        );
        assert!(result.rows_truncated());
        Ok(())
    }

    #[test]
    fn unsupported_requests_are_rejected_before_transport() {
        let mut request = QueryRequest::new("DELETE FROM records");
        request.params.push(Value::Int(1));
        assert!(matches!(
            validate_request(&request),
            Err(DbError::NotSupported(_))
        ));
        request.params.clear();
        request.statement_timeout = Some(Duration::from_secs(1));
        assert!(matches!(
            validate_request(&request),
            Err(DbError::NotSupported(_))
        ));
    }

    #[test]
    fn completed_cancellation_cannot_cancel_another_request() -> Result<()> {
        let cancellation = Cancellation::default();
        cancellation.active_id.store(1, Ordering::Release);
        cancellation.cancel()?;
        assert!(cancellation.is_cancelled());
        drop(ActiveRequest(&cancellation));
        assert!(!cancellation.is_cancelled());
        cancellation.active_id.store(2, Ordering::Release);
        assert!(!cancellation.is_cancelled());
        Ok(())
    }

    #[test]
    fn malformed_values_are_not_coerced() {
        assert!(
            RemoteValue::Integer("9223372036854775808".into())
                .into_value()
                .is_err()
        );
        assert!(RemoteValue::Blob("0".into()).into_value().is_err());
        assert!(RemoteValue::Blob("zz".into()).into_value().is_err());
    }
}
