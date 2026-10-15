import json
import os
import queue
import re
import sqlite3
import sys
import threading
from urllib.parse import quote


SQL_TOKEN = re.compile(
    r"--[^\n]*(?:\n|$)|/\*[\s\S]*?\*/|'(?:''|[^'])*'|\"(?:\"\"|[^\"])*\""
    r"|`(?:``|[^`])*`|\[[^\]]*\]|[A-Za-z_][A-Za-z_0-9]*|[^\s]"
)
READ_PRAGMAS = {
    "application_id", "auto_vacuum", "busy_timeout", "cache_size", "cache_spill",
    "collation_list", "compile_options", "data_version", "database_list",
    "defer_foreign_keys", "encoding", "foreign_keys", "freelist_count",
    "function_list", "journal_mode", "journal_size_limit", "legacy_alter_table",
    "locking_mode", "max_page_count", "mmap_size", "module_list", "page_count",
    "page_size", "pragma_list", "query_only", "read_uncommitted",
    "recursive_triggers", "reverse_unordered_selects", "schema_version",
    "secure_delete", "soft_heap_limit", "synchronous", "temp_store",
    "threads", "trusted_schema", "user_version", "wal_autocheckpoint",
}
READ_ARGUMENT_PRAGMAS = {
    "foreign_key_check", "foreign_key_list", "index_info", "index_list",
    "index_xinfo", "integrity_check", "quick_check", "table_info", "table_list",
    "table_xinfo",
}


class Cancelled(Exception):
    pass


def tokens(sql):
    return [match for match in SQL_TOKEN.finditer(sql)
            if not match.group().startswith(("--", "/*"))]


def statements(sql):
    result = []
    start = 0
    for position, character in enumerate(sql):
        if character == ";" and sqlite3.complete_statement(sql[start:position + 1]):
            fragment = sql[start:position + 1]
            if any(token.group() != ";" for token in tokens(fragment)):
                result.append(fragment)
            start = position + 1
    remaining = sql[start:]
    if tokens(remaining):
        if not sqlite3.complete_statement(remaining + "\n;"):
            raise ValueError("Incomplete SQLite statement")
        result.append(remaining)
    return result


def identifier(value):
    return '"' + value.replace('"', '""') + '"'


def authorize_all(*arguments):
    # Python before 3.11 does not support clearing the authorizer with None.
    return sqlite3.SQLITE_OK


def cell(value):
    if value is None:
        return {"type": "null"}
    if isinstance(value, bytes):
        return {"type": "blob", "value": value.hex()}
    if isinstance(value, int):
        return {"type": "integer", "value": str(value)}
    if isinstance(value, float):
        return {"type": "real", "value": repr(value)}
    return {"type": "text", "value": value}


def check_constraints(sql):
    result = []
    lexemes = tokens(sql or "")
    for position, token in enumerate(lexemes):
        if token.group().upper() != "CHECK" or position + 1 >= len(lexemes):
            continue
        opening = lexemes[position + 1]
        if opening.group() != "(":
            continue
        depth = 1
        for closing in lexemes[position + 2:]:
            if closing.group() == "(":
                depth += 1
            elif closing.group() == ")":
                depth -= 1
                if depth == 0:
                    result.append({"name": "check_" + str(len(result)),
                                   "kind": "Check", "columns": [],
                                   "check_clause": sql[opening.end():closing.start()].strip()})
                    break
    return result


class Worker:
    def __init__(self):
        self.connection = None
        self.read_only = False
        self.requests = queue.Queue()
        self.events = {}
        self.lock = threading.Lock()
        self.output_lock = threading.Lock()
        self.stopping = threading.Event()
        self.active_id = None
        self.interrupt_enabled = False

    def respond(self, response):
        with self.output_lock:
            sys.stdout.write(json.dumps(response, ensure_ascii=True, allow_nan=False) + "\n")
            sys.stdout.flush()

    def read_requests(self):
        try:
            # A daemon blocked in buffered stdin can abort interpreter shutdown after close.
            def lines():
                buffered = b""
                while True:
                    chunk = os.read(sys.stdin.fileno(), 65536)
                    if not chunk:
                        if buffered:
                            yield buffered
                        return
                    buffered += chunk
                    while b"\n" in buffered:
                        line, buffered = buffered.split(b"\n", 1)
                        yield line

            for line in lines():
                try:
                    request = json.loads(line)
                    request_id = request["id"]
                    if not isinstance(request_id, int) or isinstance(request_id, bool):
                        raise ValueError("Request id must be an integer")
                    with self.lock:
                        if request.get("operation") == "cancel":
                            event = self.events.get(request_id)
                            if event is not None:
                                event.set()
                                if (self.active_id == request_id and self.interrupt_enabled
                                        and self.connection is not None):
                                    self.connection.interrupt()
                        else:
                            if request_id in self.events:
                                raise ValueError("Duplicate request id")
                            event = threading.Event()
                            self.events[request_id] = event
                            self.requests.put((request, event))
                except (ValueError, KeyError, TypeError) as error:
                    self.respond({"id": None, "ok": False, "error": str(error),
                                  "cancelled": False})
        finally:
            self.stopping.set()
            with self.lock:
                for event in self.events.values():
                    event.set()
                if self.connection is not None and self.interrupt_enabled:
                    self.connection.interrupt()
            self.requests.put(None)

    def require_active(self, event):
        if event.is_set() or self.stopping.is_set():
            raise Cancelled("Query cancelled")

    def authorize_read(self, action, first, second, database, source):
        if action in (sqlite3.SQLITE_SELECT, sqlite3.SQLITE_READ, sqlite3.SQLITE_RECURSIVE):
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_FUNCTION and (second or "").lower() not in {
            "load_extension", "writefile", "eval"
        }:
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_PRAGMA:
            name = (first or "").lower()
            if name in READ_ARGUMENT_PRAGMAS or (second is None and name in READ_PRAGMAS):
                return sqlite3.SQLITE_OK
        return sqlite3.SQLITE_DENY

    def rollback(self):
        with self.lock:
            self.interrupt_enabled = False
        self.connection.set_progress_handler(None, 0)
        self.connection.set_authorizer(authorize_all)
        if self.connection.in_transaction:
            self.connection.execute("ROLLBACK")

    def execute(self, request, event):
        connection = self.connection
        read_only = self.read_only or request.get("read_only", False)
        transaction_before = connection.in_transaction
        if read_only and transaction_before:
            raise ValueError("Read-only execution cannot run inside an open transaction; "
                             "end it with COMMIT or ROLLBACK first")
        limit = request.get("limit")
        if limit is not None and (not isinstance(limit, int) or limit < 0):
            raise ValueError("Row limit must be a nonnegative integer")
        sql_statements = statements(request["sql"])
        query_only_before = connection.execute("PRAGMA query_only").fetchone()[0]
        if read_only:
            connection.execute("PRAGMA query_only = ON")
            connection.set_authorizer(self.authorize_read)
        try:
            results = []
            for statement in sql_statements:
                self.require_active(event)
                cursor = connection.execute(statement)
                try:
                    columns = [description[0] for description in cursor.description or []]
                    rows = []
                    truncated = False
                    for row in cursor:
                        self.require_active(event)
                        if limit is None or limit > 0:
                            rows.append([cell(value) for value in row])
                            if limit is not None:
                                limit -= 1
                        else:
                            truncated = True
                    # Older Python versions do not refresh rowcount after RETURNING is drained.
                    affected_rows = (connection.execute("SELECT changes()").fetchone()[0]
                                     if cursor.rowcount >= 0 else None)
                    results.append({"columns": columns, "rows": rows,
                                    "affected_rows": affected_rows,
                                    "truncated": truncated})
                finally:
                    cursor.close()
            return {"results": results}
        except Exception as error:
            if not transaction_before and connection.in_transaction:
                try:
                    self.rollback()
                except sqlite3.Error as rollback_error:
                    raise RuntimeError(str(error) + "; rollback also failed: " + str(rollback_error)) from error
            raise
        finally:
            connection.set_authorizer(authorize_all)
            if read_only:
                with self.lock:
                    self.interrupt_enabled = False
                connection.set_progress_handler(None, 0)
                connection.execute("PRAGMA query_only = " + str(query_only_before))

    def atomic(self, request, event):
        connection = self.connection
        if self.read_only:
            raise ValueError("This connection is read-only")
        if connection.in_transaction:
            raise ValueError("Finish the open SQL transaction before editing rows")
        batches = [statements(statement) for statement in request["statements"]]
        if any(len(batch) != 1 for batch in batches):
            raise ValueError("Each row change must contain exactly one statement")
        connection.execute("BEGIN IMMEDIATE")
        try:
            # Row changes may not commit the enclosing transaction or start a savepoint.
            connection.set_authorizer(lambda action, *arguments: sqlite3.SQLITE_DENY
                                      if action in (sqlite3.SQLITE_TRANSACTION, sqlite3.SQLITE_SAVEPOINT)
                                      else sqlite3.SQLITE_OK)
            for batch in batches:
                self.require_active(event)
                cursor = connection.execute(batch[0])
                try:
                    for row in cursor:
                        self.require_active(event)
                    affected_rows = (connection.execute("SELECT changes()").fetchone()[0]
                                     if cursor.rowcount >= 0 else 0)
                    if affected_rows != 1:
                        raise ValueError("Row change affected " + str(affected_rows)
                                         + " rows instead of exactly one; refresh the table")
                finally:
                    cursor.close()
            self.require_active(event)
            connection.set_authorizer(authorize_all)
            connection.execute("COMMIT")
            return {"affected_rows": len(batches)}
        except Exception as error:
            try:
                self.rollback()
            except sqlite3.Error as rollback_error:
                raise RuntimeError(str(error) + "; rollback also failed: " + str(rollback_error)) from error
            raise
        finally:
            connection.set_authorizer(authorize_all)

    def schema(self):
        result = {"tables": [], "views": []}
        for schema in self.connection.execute("PRAGMA database_list"):
            schema_name = schema[1]
            for name, kind in self.connection.execute(
                    "SELECT name, type FROM " + identifier(schema_name)
                    + ".sqlite_master WHERE type IN ('table', 'view') "
                    "AND name NOT LIKE 'sqlite_%' ORDER BY name"):
                result["tables" if kind == "table" else "views"].append(
                    {"name": name, "schema": schema_name})
        return result

    def table_details(self, request):
        schema = request.get("schema") or "main"
        table = request["table"]
        connection = self.connection
        relation = connection.execute(
            "SELECT name, type, sql FROM " + identifier(schema)
            + ".sqlite_master WHERE name = ? COLLATE NOCASE AND type IN ('table', 'view')",
            (table,)).fetchone()
        if relation is None:
            raise ValueError("Table or view does not exist")
        table, kind, definition = relation

        def pragma(name, argument):
            return list(connection.execute("PRAGMA " + identifier(schema) + "." + name
                                           + "(" + identifier(argument) + ")"))

        columns = []
        pseudo_columns = []
        for column in pragma("table_xinfo", table):
            if column[6]:
                pseudo_columns.append(column[1])
            else:
                columns.append({"name": column[1], "type_name": column[2],
                                "nullable": not column[3] and not column[5],
                                "is_primary_key": column[5] > 0,
                                "default_value": column[4], "enum_values": None})
        if kind == "table":
            try:
                connection.execute("SELECT rowid, oid, _rowid_ FROM " + identifier(schema)
                                   + "." + identifier(table) + " LIMIT 0").close()
                pseudo_columns.extend(["rowid", "oid", "_rowid_"])
            except sqlite3.OperationalError as error:
                if "no such column" not in str(error):
                    raise
        indexes = []
        constraints = check_constraints(definition)
        for index in pragma("index_list", table):
            index_columns = [column[2] for column in pragma("index_info", index[1])
                             if column[2] is not None]
            indexes.append({"name": index[1], "columns": index_columns,
                            "is_unique": bool(index[2]), "is_primary": index[3] == "pk"})
            if index[3] == "u":
                constraints.append({"name": index[1], "kind": "Unique",
                                    "columns": index_columns, "check_clause": None})
        foreign_keys = {}
        for key in pragma("foreign_key_list", table):
            entry = foreign_keys.setdefault(key[0], {
                "name": "fk_" + str(key[0]), "columns": [],
                "referenced_table": key[2], "referenced_schema": schema,
                "referenced_columns": [],
                "on_delete": key[6] if key[6] != "NO ACTION" else None,
                "on_update": key[5] if key[5] != "NO ACTION" else None,
            })
            entry["columns"].append(key[3])
            if key[4] is not None:
                entry["referenced_columns"].append(key[4])
            else:
                primary_columns = sorted((column[5], column[1])
                                         for column in pragma("table_info", key[2]) if column[5])
                if key[1] < len(primary_columns):
                    entry["referenced_columns"].append(primary_columns[key[1]][1])
        return {"name": table, "schema": schema, "columns": columns,
                "pseudo_columns": pseudo_columns, "indexes": indexes,
                "foreign_keys": list(foreign_keys.values()), "constraints": constraints}

    def dispatch(self, request, event):
        self.require_active(event)
        operation = request["operation"]
        if operation == "open":
            if self.connection is not None:
                raise ValueError("Database is already open")
            if sqlite3.sqlite_version_info < (3, 26, 0):
                raise ValueError("Remote SQLite requires Python 3 with SQLite 3.26 or newer")
            path = request["path"]
            if not isinstance(path, str) or not os.path.isabs(path) or "\x00" in path:
                raise ValueError("Remote SQLite requires an absolute server file path")
            self.read_only = bool(request.get("read_only", False))
            # URI mode must never create a new database for a mistyped remote path.
            connection = sqlite3.connect("file:" + quote(path, safe="/")
                                         + "?mode=" + ("ro" if self.read_only else "rw"),
                                         uri=True, isolation_level=None, cached_statements=0)
            try:
                connection.execute("SELECT name FROM sqlite_master LIMIT 0").close()
                connection.execute("PRAGMA foreign_keys = ON")
                if self.read_only:
                    connection.execute("PRAGMA query_only = ON")
            except Exception:
                connection.close()
                raise
            with self.lock:
                self.connection = connection
            return None
        if self.connection is None:
            raise ValueError("Database is not open")
        self.connection.set_progress_handler(lambda: int(event.is_set() or self.stopping.is_set()), 1000)
        try:
            if operation == "execute":
                return self.execute(request, event)
            if operation == "atomic":
                return self.atomic(request, event)
            if operation == "schema":
                return self.schema()
            if operation == "table_details":
                return self.table_details(request)
            if operation in ("cleanup", "close"):
                self.rollback()
                if operation == "close":
                    self.close()
                return None
            raise ValueError("Unsupported SQLite worker operation")
        finally:
            if self.connection is not None:
                self.connection.set_progress_handler(None, 0)

    def close(self):
        with self.lock:
            if self.connection is not None:
                self.connection.close()
                self.connection = None

    def run(self):
        threading.Thread(target=self.read_requests, daemon=True).start()
        try:
            while not self.stopping.is_set():
                queued = self.requests.get()
                if queued is None:
                    break
                request, event = queued
                request_id = request["id"]
                with self.lock:
                    self.active_id = request_id
                    self.interrupt_enabled = True
                try:
                    result = self.dispatch(request, event)
                    response = {"id": request_id, "ok": True, "result": result}
                except Exception as error:
                    response = {"id": request_id, "ok": False, "error": str(error),
                                "cancelled": isinstance(error, Cancelled) or event.is_set()}
                finally:
                    with self.lock:
                        self.active_id = None
                        self.interrupt_enabled = False
                        self.events.pop(request_id, None)
                self.respond(response)
                if request.get("operation") == "close":
                    break
        finally:
            self.close()


if __name__ == "__main__":
    Worker().run()
