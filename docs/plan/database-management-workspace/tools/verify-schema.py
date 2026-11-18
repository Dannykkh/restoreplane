"""Execute the planning DDL and constraint examples; this is not a product test.

Reads only db-schema.md/schema-client-v1.sql. Test databases are in memory or
inside a new TemporaryDirectory; no product store or external DB is opened.
State transitions below are examples of the documented writer protocol, not
an implementation or certification of the Rust Workbench state machine.
"""

from __future__ import annotations

import json
import pathlib
import re
import sqlite3
import sys
import tempfile
import unittest
import uuid
from contextlib import closing, contextmanager


ROOT = pathlib.Path(__file__).resolve().parents[1]
EXPECTED_TABLES = {
    "journal_header", "target_bindings", "operations", "statements", "saved_queries"
}
DDL = ""


def new_id() -> str:
    return str(uuid.uuid4())


def sql_body(text: str) -> str:
    return "\n".join(
        line.rstrip() for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("--")
    )


def load_ddl() -> str:
    markdown = (ROOT / "db-schema.md").read_text(encoding="utf-8-sig")
    fences = re.findall(r"^```sql\s*\n(.*?)^```", markdown, re.M | re.S)
    if len(fences) != 1:
        raise ValueError("db-schema.md must contain exactly one canonical SQL fence")
    extracted = (ROOT / "schema-client-v1.sql").read_text(encoding="utf-8-sig")
    if sql_body(extracted) != sql_body(fences[0]):
        raise ValueError("schema-client-v1.sql differs from db-schema.md canonical DDL")
    if sqlite3.sqlite_version_info < (3, 37, 0):
        raise RuntimeError("SQLite >=3.37 is required; weaker runtime is not a pass")
    return extracted


def insert(connection: sqlite3.Connection, table: str, values: dict) -> None:
    # All identifiers are fixed by this script, never read from DB data or CLI.
    names = ",".join(values)
    markers = ",".join("?" for _ in values)
    connection.execute(f"INSERT INTO {table} ({names}) VALUES ({markers})", tuple(values.values()))


class Fixture:
    def __init__(self, path: str = ":memory:") -> None:
        self.connection = sqlite3.connect(path)
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript(DDL)
        self.owner = "a" * 64
        self.epoch = new_id()
        self.binding = new_id()
        insert(self.connection, "journal_header", {
            "singleton": 1, "schema_version": 1, "owner_id": self.owner,
            "journal_epoch": self.epoch, "created_at_ms": 1000, "updated_at_ms": 1000,
        })
        insert(self.connection, "target_bindings", {
            "owner_id": self.owner, "workspace_id": "workspace-a", "target_id": "target-a",
            "binding_id": self.binding, "connection_profile_id": "profile-a",
            "descriptor_revision": "b" * 64, "authority_epoch": "service-boot-a",
            "source_identity_digest": "c" * 64, "engine": "postgresql", "state": "active",
            "created_at_ms": 1000, "updated_at_ms": 1000,
        })
        self.connection.commit()

    def operation(self, **changes) -> dict:
        values = {
            "owner_id": self.owner, "workspace_id": "workspace-a", "target_id": "target-a",
            "binding_id": self.binding, "operation_id": new_id(), "kind": "verified_apply",
            "idempotency_key": new_id(), "payload_digest": "d" * 64,
            "journal_epoch": self.epoch, "process_boot_epoch": new_id(),
            "window_instance_id": new_id(), "state": "accepted",
            "token_expires_at_ms": 2000, "dedup_retain_until_ms": 3000,
            "created_at_ms": 1000, "updated_at_ms": 1000,
        }
        return dict(values, **changes)

    def statement(self, operation: dict, **changes) -> dict:
        return dict({
            "owner_id": operation["owner_id"], "workspace_id": operation["workspace_id"],
            "target_id": operation["target_id"], "operation_id": operation["operation_id"],
            "ordinal": 0, "statement_digest": "e" * 64, "effect_class": "write",
            "outcome": "sent", "effect_evidence": "pending_transaction",
            "created_at_ms": 1000, "updated_at_ms": 1000,
        }, **changes)

    def saved_query(self, **changes) -> dict:
        # Synthetic opaque bytes test BLOB constraints; this is not DPAPI proof.
        return dict({
            "owner_id": self.owner, "workspace_id": "workspace-a", "target_id": "target-a",
            "binding_id": self.binding, "query_id": new_id(),
            "encryption_format": "current-user-dpapi-v1", "encrypted_content": b"synthetic-blob",
            "created_at_ms": 1000, "updated_at_ms": 1000,
        }, **changes)


class SchemaDocumentChecks(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()
        self.connection = self.fixture.connection
        self.addCleanup(self.connection.close)

    @contextmanager
    def rejected(self, error_name: str):
        self.connection.execute("SAVEPOINT negative_case")
        try:
            with self.assertRaises(sqlite3.IntegrityError) as caught:
                yield
            if error_name == "SQLITE_CONSTRAINT_DATATYPE":
                # Some CPython builds omit this newer name while SQLite returns
                # the correct extended numeric error; do not accept any error.
                self.assertEqual(caught.exception.sqlite_errorcode, 3091)
            else:
                self.assertEqual(caught.exception.sqlite_errorname, error_name)
        finally:
            self.connection.execute("ROLLBACK TO negative_case")
            self.connection.execute("RELEASE negative_case")

    def test_schema_has_exactly_five_strict_tables_and_foreign_keys(self) -> None:
        tables = {row["name"] for row in self.connection.execute(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        )}
        self.assertEqual(tables, EXPECTED_TABLES)
        strict = {row["name"]: row["strict"] for row in self.connection.execute("PRAGMA table_list")}
        self.assertTrue(all(strict[name] == 1 for name in EXPECTED_TABLES))
        self.assertEqual(self.connection.execute("PRAGMA user_version").fetchone()[0], 1)
        self.assertEqual(self.connection.execute("PRAGMA foreign_keys").fetchone()[0], 1)
        self.assertEqual(self.connection.execute("PRAGMA trusted_schema").fetchone()[0], 0)
        self.assertEqual(self.connection.execute("PRAGMA synchronous").fetchone()[0], 2)
        for table in EXPECTED_TABLES - {"journal_header"}:
            self.assertTrue(list(self.connection.execute(f"PRAGMA foreign_key_list({table})")))
        self.assertFalse(list(self.connection.execute("PRAGMA foreign_key_check")))

    def test_owner_workspace_target_binding_are_not_interchangeable(self) -> None:
        for changes in ({"owner_id": "f" * 64}, {"workspace_id": "workspace-b"},
                        {"target_id": "target-b"}, {"binding_id": new_id()}):
            with self.subTest(changes=tuple(changes)):
                with self.rejected("SQLITE_CONSTRAINT_FOREIGNKEY"):
                    insert(self.connection, "operations", self.fixture.operation(**changes))

    def test_scoped_idempotency_rejects_same_key_with_other_payload(self) -> None:
        operation = self.fixture.operation()
        insert(self.connection, "operations", operation)
        for digest in (operation["payload_digest"], "f" * 64):
            with self.rejected("SQLITE_CONSTRAINT_UNIQUE"):
                insert(self.connection, "operations", dict(
                    operation, operation_id=new_id(), payload_digest=digest
                ))
        # Different kind is a distinct documented idempotency namespace.
        insert(self.connection, "operations", dict(operation, operation_id=new_id(), kind="query"))

    def test_state_version_dispatch_and_time_checks(self) -> None:
        self.assertEqual(self.connection.execute(
            "SELECT storage_revision FROM journal_header WHERE singleton=1"
        ).fetchone()[0], 0)
        with self.rejected("SQLITE_CONSTRAINT_CHECK"):
            self.connection.execute("UPDATE journal_header SET storage_revision=-1")
        for changes in ({"state": "invented"}, {"version": 0}, {"dispatch_count": 2},
                        {"connection_generation": -1}, {"execution_epoch": -1},
                        {"state": "completed"}, {"terminal_at_ms": 1200},
                        {"state": "committed", "terminal_at_ms": 1200},
                        {"state": "rolled_back", "terminal_at_ms": 1200},
                        {"token_expires_at_ms": 1000}, {"token_expires_at_ms": 86401001},
                        {"dedup_retain_until_ms": 1999}, {"updated_at_ms": 999},
                        {"journal_epoch": "too-short"}):
            with self.subTest(changes=changes):
                with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                    insert(self.connection, "operations", self.fixture.operation(**changes))
        with self.rejected("SQLITE_CONSTRAINT_DATATYPE"):
            insert(self.connection, "operations", self.fixture.operation(version="not-an-integer"))
        insert(self.connection, "operations", self.fixture.operation(
            state="committed", transaction_outcome="committed", terminal_at_ms=1200
        ))

    def test_key_and_metadata_insert_bounds(self) -> None:
        insert(self.connection, "operations", self.fixture.operation(
            kind="export_job", movement_metadata_json='{"version":1,"state":"partial"}'
        ))
        for changes in (
            {"kind": "query", "movement_metadata_json": "{}"},
            {"kind": "import_job", "movement_metadata_json": "not-json"},
            {"kind": "export_job", "movement_metadata_json": json.dumps("x" * 65536)},
        ):
            with self.subTest(movement=tuple(changes)):
                with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                    insert(self.connection, "operations", self.fixture.operation(**changes))
        for size in (1, 256):
            insert(self.connection, "operations", self.fixture.operation(idempotency_key="k" * size))
        for changes in ({"idempotency_key": ""}, {"idempotency_key": "k" * 257},
                        {"failure_code": "x" * 65}, {"payload_digest": "x" * 63}):
            with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                insert(self.connection, "operations", self.fixture.operation(**changes))

    def test_import_children_require_full_range_and_correct_parent_kind(self) -> None:
        parent = self.fixture.operation(kind="import_job")
        insert(self.connection, "operations", parent)
        child = self.fixture.operation(
            kind="import_batch", parent_operation_id=parent["operation_id"], parent_kind="import_job",
            batch_index=0, source_digest="e" * 64, batch_digest="f" * 64,
            source_byte_start=0, source_byte_end=100, source_row_start=0, source_row_end=10,
        )
        for changes in ({"parent_kind": None}, {"batch_index": None}, {"source_byte_start": None},
                        {"source_row_end": None}, {"source_byte_end": 0}, {"source_row_end": 0},
                        {"batch_index": -1}, {"batch_digest": None}):
            with self.subTest(changes=changes):
                with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                    insert(self.connection, "operations", dict(child, **changes))
        other = self.fixture.operation(kind="query")
        insert(self.connection, "operations", other)
        with self.rejected("SQLITE_CONSTRAINT_FOREIGNKEY"):
            insert(self.connection, "operations", dict(child, parent_operation_id=other["operation_id"]))
        insert(self.connection, "operations", child)
        with self.rejected("SQLITE_CONSTRAINT_UNIQUE"):
            insert(self.connection, "operations", dict(child, operation_id=new_id(), idempotency_key=new_id()))
        with self.rejected("SQLITE_CONSTRAINT_TRIGGER"):
            self.connection.execute("DELETE FROM operations WHERE operation_id=?", (parent["operation_id"],))

    def test_statement_fk_ordinal_numeric_and_independent_effect_evidence(self) -> None:
        operation = self.fixture.operation(kind="manual_transaction")
        insert(self.connection, "operations", operation)
        statement = self.fixture.statement(operation, outcome="succeeded")
        for changes in ({"owner_id": "f" * 64}, {"workspace_id": "other"},
                        {"target_id": "other"}, {"operation_id": new_id()}):
            with self.rejected("SQLITE_CONSTRAINT_FOREIGNKEY"):
                insert(self.connection, "statements", dict(statement, **changes))
        for changes in ({"ordinal": -1}, {"ordinal": 10000}, {"outcome": "invented"},
                        {"matched_rows": "-1"}, {"changed_rows": "1.0"}):
            with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                insert(self.connection, "statements", dict(statement, **changes))
        insert(self.connection, "statements", dict(statement, matched_rows="18446744073709551615"))
        row = self.connection.execute("SELECT outcome,effect_evidence,matched_rows FROM statements").fetchone()
        self.assertEqual(tuple(row), ("succeeded", "pending_transaction", "18446744073709551615"))
        self.assertEqual(self.connection.execute("SELECT state FROM operations").fetchone()[0], "accepted")

    def test_saved_query_blob_format_size_and_scope_constraints(self) -> None:
        insert(self.connection, "saved_queries", self.fixture.saved_query(encrypted_content=b"x" * 262144))
        for changes in ({"encrypted_content": b""}, {"encrypted_content": b"x" * 262145},
                        {"encryption_format": "plaintext"}):
            with self.rejected("SQLITE_CONSTRAINT_CHECK"):
                insert(self.connection, "saved_queries", self.fixture.saved_query(**changes))
        with self.rejected("SQLITE_CONSTRAINT_DATATYPE"):
            insert(self.connection, "saved_queries", self.fixture.saved_query(encrypted_content="raw SQL"))
        with self.rejected("SQLITE_CONSTRAINT_FOREIGNKEY"):
            insert(self.connection, "saved_queries", self.fixture.saved_query(workspace_id="other"))

    def test_journal_columns_exclude_plaintext_payload_storage(self) -> None:
        prohibited = {"sql", "raw_sql", "query_text", "parameters", "password", "rows", "original_values", "draft_values"}
        for table in EXPECTED_TABLES:
            columns = {row["name"] for row in self.connection.execute(f"PRAGMA table_info({table})")}
            self.assertFalse(columns & prohibited, table)

    def test_two_writers_stale_compare_and_set_cannot_dispatch_twice(self) -> None:
        with tempfile.TemporaryDirectory(prefix="workbench-schema-check-") as directory:
            fixture = Fixture(str(pathlib.Path(directory) / "check.sqlite3"))
            first = fixture.connection
            second = sqlite3.connect(str(pathlib.Path(directory) / "check.sqlite3"))
            try:
                operation = fixture.operation()
                insert(first, "operations", operation)
                first.commit()
                self.assertEqual(first.execute("PRAGMA journal_mode").fetchone()[0], "wal")
                self.assertEqual(second.execute("SELECT version FROM operations").fetchone()[0], 1)
                update = """UPDATE operations SET state='dispatching',dispatch_count=1,
                    version=version+1,updated_at_ms=1100 WHERE owner_id=? AND workspace_id=?
                    AND target_id=? AND operation_id=? AND version=? AND state='accepted' AND dispatch_count=0"""
                scope = tuple(operation[key] for key in ("owner_id", "workspace_id", "target_id", "operation_id"))
                self.assertEqual(first.execute(update, (*scope, 1)).rowcount, 1)
                first.commit()
                self.assertEqual(second.execute(update, (*scope, 1)).rowcount, 0)
                self.assertEqual(second.execute(update, (*scope, 2)).rowcount, 0)
                second.commit()
                self.assertEqual(tuple(first.execute("SELECT state,version,dispatch_count FROM operations").fetchone()),
                                 ("dispatching", 2, 1))
            finally:
                second.close()
                first.close()

    def test_unknown_and_statement_evidence_survive_acknowledgment_and_reopen(self) -> None:
        with tempfile.TemporaryDirectory(prefix="workbench-unknown-check-") as directory:
            path = pathlib.Path(directory) / "check.sqlite3"
            fixture = Fixture(str(path))
            connection = fixture.connection
            operation = fixture.operation(
                state="outcome_unknown", transaction_outcome="outcome_unknown", terminal_at_ms=1200,
                version=5, dispatch_count=1,
            )
            try:
                insert(connection, "operations", operation)
                insert(connection, "statements", fixture.statement(
                    operation, outcome="outcome_unknown", effect_evidence="outcome_unknown"
                ))
                updated = connection.execute("""UPDATE operations SET operator_acknowledged_at_ms=1300,
                    updated_at_ms=1300,version=version+1 WHERE owner_id=? AND workspace_id=? AND target_id=?
                    AND operation_id=? AND state='outcome_unknown' AND version=5""", tuple(
                    operation[key] for key in ("owner_id", "workspace_id", "target_id", "operation_id")
                ))
                self.assertEqual(updated.rowcount, 1)
                connection.commit()
            finally:
                connection.close()
            with closing(sqlite3.connect(str(path))) as reopened:
                self.assertEqual(reopened.execute("SELECT state,transaction_outcome,version,dispatch_count,operator_acknowledged_at_ms FROM operations").fetchone(),
                                 ("outcome_unknown", "outcome_unknown", 6, 1, 1300))
                self.assertEqual(reopened.execute("SELECT outcome,effect_evidence FROM statements").fetchone(),
                                 ("outcome_unknown", "outcome_unknown"))
                self.assertFalse(reopened.execute("PRAGMA foreign_key_check").fetchall())

    def test_header_epoch_rotation_preserves_historical_unknown_operation(self) -> None:
        operation = self.fixture.operation(
            state="outcome_unknown", transaction_outcome="outcome_unknown",
            terminal_at_ms=1200, dispatch_count=1, version=5,
        )
        insert(self.connection, "operations", operation)
        original_epoch = self.fixture.epoch
        new_epoch = new_id()
        self.connection.execute(
            "UPDATE journal_header SET journal_epoch=?,updated_at_ms=1300 WHERE singleton=1",
            (new_epoch,),
        )
        self.connection.commit()
        row = self.connection.execute(
            "SELECT journal_epoch,state,transaction_outcome,version,dispatch_count FROM operations"
        ).fetchone()
        self.assertEqual(tuple(row), (original_epoch, "outcome_unknown", "outcome_unknown", 5, 1))
        self.assertEqual(self.connection.execute("SELECT journal_epoch FROM journal_header").fetchone()[0], new_epoch)
        self.assertFalse(list(self.connection.execute("PRAGMA foreign_key_check")))


def main() -> int:
    global DDL
    print("DOCUMENT SCHEMA VERIFICATION ONLY: not product tests, DPAPI proof, remote commit proof or performance evidence.", flush=True)
    try:
        DDL = load_ddl()
    except (OSError, ValueError, RuntimeError) as error:
        print(json.dumps({"document_schema_only": True, "passed": False, "error": str(error)}))
        return 1
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SchemaDocumentChecks)
    result = unittest.TextTestRunner(stream=sys.stdout, verbosity=2).run(suite)
    print(json.dumps({
        "document_schema_only": True, "product_tests_run": False,
        "sqlite_version": sqlite3.sqlite_version, "tests_run": result.testsRun,
        "failures": len(result.failures), "errors": len(result.errors), "passed": result.wasSuccessful(),
        "not_verified": ["Rust state machine", "Windows ACL/CurrentUser DPAPI", "remote database transactions",
                         "crash/power-loss durability", "runtime quotas", "performance"],
    }, sort_keys=True))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
