import json
import pathlib
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest


WORKER = pathlib.Path(__file__).parents[1] / "src" / "remote_sqlite.py"


class Session:
    def __init__(self):
        self.process = subprocess.Popen(
            [sys.executable, "-u", "-c", WORKER.read_text()],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8",
        )
        self.responses = queue.Queue()
        self.next_id = 0

        def read_responses():
            for line in self.process.stdout:
                self.responses.put(json.loads(line))
        self.reader = threading.Thread(target=read_responses, daemon=True)
        self.reader.start()

    def send(self, operation, **arguments):
        self.next_id += 1
        request = {"id": self.next_id, "operation": operation, **arguments}
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        return self.next_id

    def receive(self, request_id):
        response = self.responses.get(timeout=10)
        if response["id"] != request_id:
            raise AssertionError("Unexpected worker response: " + repr(response))
        return response

    def request(self, operation, **arguments):
        response = self.receive(self.send(operation, **arguments))
        if not response["ok"]:
            raise AssertionError(response)
        return response["result"]

    def reject(self, operation, **arguments):
        response = self.receive(self.send(operation, **arguments))
        if response["ok"]:
            raise AssertionError("Expected rejection: " + repr(response))
        return response

    def query(self, sql, **arguments):
        return self.request("execute", sql=sql, **arguments)["results"]

    def cancel(self, request_id):
        self.process.stdin.write(json.dumps({"id": request_id, "operation": "cancel"}) + "\n")
        self.process.stdin.flush()

    def stop(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10)
            raise AssertionError("Worker did not exit after stdin closed")
        finally:
            self.reader.join(timeout=2)
            self.process.stdout.close()
            error = self.process.stderr.read()
            self.process.stderr.close()
        if self.process.returncode != 0 or error:
            raise AssertionError("Worker failed: " + error)


class RemoteSqliteTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = pathlib.Path(self.directory.name) / "data ' ?#% ü.sqlite"
        with sqlite3.connect(self.path) as connection:
            connection.executescript(
                "CREATE TABLE parents (first INTEGER, second INTEGER, PRIMARY KEY(first, second));"
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL "
                "CHECK (length(name) > 0), value BLOB);"
                "INSERT INTO items VALUES(1, 'first', X'00FF');"
                "INSERT INTO items VALUES(2, 'second', NULL);"
            )
        self.session = Session()
        self.addCleanup(self.session.stop)
        self.session.request("open", path=str(self.path), read_only=False)

    def scalar(self, sql):
        return self.session.query(sql)[0]["rows"][0][0].get("value")

    def test_read_write_types_and_quoted_path(self):
        rows = self.session.query(
            "SELECT NULL, -9223372036854775808, 9223372036854775807, "
            "1.25, 1e999, -1e999, 'héllo\nworld', X'00FF'"
        )[0]["rows"]
        self.assertEqual(rows, [[
            {"type": "null"},
            {"type": "integer", "value": "-9223372036854775808"},
            {"type": "integer", "value": "9223372036854775807"},
            {"type": "real", "value": "1.25"},
            {"type": "real", "value": "inf"},
            {"type": "real", "value": "-inf"},
            {"type": "text", "value": "héllo\nworld"},
            {"type": "blob", "value": "00ff"},
        ]])
        result = self.session.query("UPDATE items SET name = 'changed' WHERE id = 1")
        self.assertEqual(result[0]["affected_rows"], 1)
        with sqlite3.connect(self.path) as connection:
            self.assertEqual(connection.execute("SELECT name FROM items WHERE id = 1").fetchone(),
                             ("changed",))

    def test_missing_path_is_not_created_and_relative_path_rejected(self):
        session = Session()
        self.addCleanup(session.stop)
        absent = pathlib.Path(self.directory.name) / "missing.sqlite"
        session.reject("open", path=str(absent))
        self.assertFalse(absent.exists())
        session.reject("open", path="relative.sqlite")
        corrupt = pathlib.Path(self.directory.name) / "corrupt.sqlite"
        corrupt.write_bytes(b"This is not a SQLite database" * 100)
        session.reject("open", path=str(corrupt))
        session.request("open", path=str(self.path))

    def test_read_only_profile_and_requests_reject_escape_routes(self):
        session = Session()
        self.addCleanup(session.stop)
        session.request("open", path=str(self.path), read_only=True)
        blocked = [
            "UPDATE items SET name = 'changed' WHERE id = 1",
            "WITH selected AS (SELECT 1) DELETE FROM items WHERE id IN selected",
            "PRAGMA query_only = OFF; DELETE FROM items",
            "PRAGMA writable_schema = ON",
            "PRAGMA user_version = 8",
            "ATTACH DATABASE ':memory:' AS other",
            "CREATE TEMP TABLE escaped (id)",
            "BEGIN; SELECT 1; COMMIT",
            "VACUUM",
        ]
        for sql in blocked:
            for target in [session, self.session]:
                with self.subTest(sql=sql, read_only_profile=target is session):
                    target.reject("execute", sql=sql, read_only=True)
                    self.assertEqual(target.query("SELECT COUNT(*) FROM items")[0]["rows"][0][0]["value"], "2")
        session.reject("atomic", statements=["DELETE FROM items WHERE id = 1"])
        self.session.query("UPDATE items SET name = 'writable again' WHERE id = 1")
        session.query("PRAGMA table_info(items)")
        self.session.query("SELECT name FROM pragma_table_xinfo('items')", read_only=True)

    def test_manual_transaction_survives_requests_and_rejected_operations(self):
        self.session.query("BEGIN")
        self.session.query("UPDATE items SET name = 'uncommitted' WHERE id = 1")
        self.session.reject("atomic", statements=["DELETE FROM items WHERE id = 1"])
        self.session.reject("execute", sql="SELECT 1", read_only=True)
        self.session.reject("execute", sql="UPDATE missing SET name = 'bad'")
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "uncommitted")
        with sqlite3.connect(self.path) as connection:
            self.assertEqual(connection.execute("SELECT name FROM items WHERE id = 1").fetchone(),
                             ("first",))
        self.session.query("ROLLBACK")
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")
        self.session.reject("execute", sql="BEGIN; UPDATE items SET name = 'bad' WHERE id = 1; SELECT missing")
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")

    def test_atomic_conflict_and_constraint_failure_roll_back_every_change(self):
        for failure in ["DELETE FROM items WHERE id = 10",
                        "UPDATE items SET name = 'taken'",
                        "UPDATE items SET name = '' WHERE id = 2",
                        "COMMIT"]:
            with self.subTest(failure=failure):
                self.session.reject("atomic", statements=[
                    "UPDATE items SET name = 'changed' WHERE id = 1", failure])
                self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")
        self.assertEqual(self.session.request("atomic", statements=[
            "UPDATE items SET name = 'changed' WHERE id = 1",
            "DELETE FROM items WHERE id = 2",
        ]), {"affected_rows": 2})
        self.assertEqual(self.scalar("SELECT COUNT(*) FROM items"), "1")

    def test_shared_row_limit_drains_returning_and_executes_later_statements(self):
        results = self.session.query(
            "SELECT id FROM items ORDER BY id;"
            "UPDATE items SET name = name || '-done' RETURNING id;"
            "INSERT INTO items(id, name) VALUES(3, 'third')", limit=1)
        self.assertEqual(len(results), 3)
        self.assertEqual(results[0]["rows"], [[{"type": "integer", "value": "1"}]])
        self.assertTrue(results[0]["truncated"])
        self.assertEqual(results[1]["rows"], [])
        self.assertTrue(results[1]["truncated"])
        self.assertEqual(results[1]["affected_rows"], 2)
        self.assertEqual(self.scalar("SELECT COUNT(*) FROM items WHERE name LIKE '%-done'"), "2")
        self.assertEqual(self.scalar("SELECT COUNT(*) FROM items"), "3")
        self.assertFalse(self.session.query("SELECT id FROM items LIMIT 1", limit=1)[0]["truncated"])

    def test_statement_split_preserves_triggers_comments_and_quotes(self):
        self.session.query(
            "CREATE TABLE audit (message TEXT);"
            "CREATE TRIGGER changes AFTER UPDATE ON items BEGIN "
            "INSERT INTO audit VALUES('first; value'); "
            "INSERT INTO audit VALUES('it''s second;'); END;"
            "-- comment with ;\nUPDATE items SET name = 'quoted;name' WHERE id = 1;"
            "/* trailing ; comment */"
        )
        self.assertEqual(self.scalar("SELECT COUNT(*) FROM audit"), "2")
        self.session.reject("execute", sql="DELETE FROM audit; SELECT 'unfinished")
        self.assertEqual(self.scalar("SELECT COUNT(*) FROM audit"), "2")
        self.assertEqual(self.session.query("-- comments only\n/* more; */"), [])

    def test_schema_metadata_quotes_generated_columns_foreign_keys_and_checks(self):
        self.session.query(
            'CREATE TABLE "odd\'table" (id INTEGER, parent_first INTEGER, parent_second INTEGER, '
            'value TEXT CHECK (value != \')\' AND length(value) > 0), '
            'derived TEXT GENERATED ALWAYS AS (value || \';CHECK(ignored)\') VIRTUAL, '
            'PRIMARY KEY(id), UNIQUE(value), FOREIGN KEY(parent_first, parent_second) '
            'REFERENCES parents ON DELETE CASCADE);'
            'CREATE INDEX "index\'quote" ON "odd\'table"(value);'
            'CREATE TABLE codes (code TEXT PRIMARY KEY) WITHOUT ROWID;'
            'CREATE VIEW item_view AS SELECT * FROM items;'
        )
        schema = self.session.request("schema")
        self.assertIn({"name": "odd'table", "schema": "main"}, schema["tables"])
        self.assertIn({"name": "item_view", "schema": "main"}, schema["views"])
        details = self.session.request("table_details", schema="main", table="ODD'TABLE")
        self.assertEqual([column["name"] for column in details["columns"]],
                         ["id", "parent_first", "parent_second", "value"])
        self.assertEqual(details["pseudo_columns"], ["derived", "rowid", "oid", "_rowid_"])
        self.assertTrue(any(index["name"] == "index'quote" for index in details["indexes"]))
        self.assertEqual(details["foreign_keys"][0]["referenced_columns"], ["first", "second"])
        self.assertEqual(details["foreign_keys"][0]["on_delete"], "CASCADE")
        checks = [constraint for constraint in details["constraints"] if constraint["kind"] == "Check"]
        self.assertEqual([check["check_clause"] for check in checks], ["value != ')' AND length(value) > 0"])
        for table in ["codes", "item_view"]:
            self.assertEqual(self.session.request("table_details", table=table)["pseudo_columns"], [])
        self.session.reject("table_details", table="absent")

    def test_attached_schema_and_hidden_virtual_columns(self):
        self.session.query("ATTACH ':memory:' AS extra; CREATE TABLE extra.remote_name (id INTEGER)")
        self.assertIn({"name": "remote_name", "schema": "extra"},
                      self.session.request("schema")["tables"])
        details = self.session.request("table_details", schema="extra", table="remote_name")
        self.assertEqual(details["columns"][0]["name"], "id")
        self.session.query("CREATE VIRTUAL TABLE search USING fts5(body)")
        details = self.session.request("table_details", table="search")
        self.assertEqual([column["name"] for column in details["columns"]], ["body"])
        self.assertIn("rank", details["pseudo_columns"])

    def test_cancellation_is_scoped_and_session_reusable(self):
        request_id = self.session.send("execute", sql=
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM numbers) "
            "SELECT sum(n) FROM numbers")
        time.sleep(0.05)
        self.session.cancel(request_id)
        response = self.session.receive(request_id)
        self.assertFalse(response["ok"])
        self.assertTrue(response["cancelled"])
        self.session.request("cleanup")
        self.session.cancel(request_id)
        self.assertEqual(self.scalar("SELECT 1"), "1")
        self.session.query("BEGIN; UPDATE items SET name = 'pending' WHERE id = 1")
        self.session.request("cleanup")
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")

    def test_cancelled_atomic_batch_and_eof_roll_back(self):
        request_id = self.session.send("atomic", statements=[
            "UPDATE items SET name = 'pending' WHERE id = 1",
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers) "
            "UPDATE items SET name = (SELECT sum(n) FROM numbers) WHERE id = 2",
        ])
        time.sleep(0.05)
        self.session.cancel(request_id)
        self.assertTrue(self.session.receive(request_id)["cancelled"])
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")
        session = Session()
        session.request("open", path=str(self.path))
        session.query("BEGIN; UPDATE items SET name = 'lost session' WHERE id = 1")
        session.send("execute", sql="WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT sum(x) FROM n")
        session.stop()
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")

    def test_cancelled_read_only_request_restores_writes(self):
        request_id = self.session.send("execute", read_only=True, sql=
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM numbers) "
            "SELECT sum(n) FROM numbers")
        time.sleep(0.05)
        self.session.cancel(request_id)
        response = self.session.receive(request_id)
        self.assertTrue(response["cancelled"])
        self.assertEqual(self.scalar("SELECT 1"), "1")
        self.session.query("UPDATE items SET name = 'writable' WHERE id = 1")
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "writable")

    def test_close_exits_without_waiting_for_stdin_eof(self):
        session = Session()
        self.addCleanup(session.stop)
        session.request("open", path=str(self.path))
        session.query("BEGIN; UPDATE items SET name = 'pending' WHERE id = 1")
        session.request("close")
        self.assertEqual(session.process.wait(timeout=10), 0)
        self.assertEqual(self.scalar("SELECT name FROM items WHERE id = 1"), "first")


if __name__ == "__main__":
    unittest.main()
