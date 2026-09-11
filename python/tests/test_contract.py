"""Installed-wheel tests using disposable synthetic traces, never a personal index."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import dataclasses
import json
import os
import re
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import trace_index as ti


def trace(path: Path, identity: str = "sample") -> Path:
    events = [
        {
            "type": "session_meta",
            "payload": {"id": identity, "cwd": "/work/reader's project"},
        },
        {"type": "turn_context", "payload": {"turn_id": "loop-a"}},
        {
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "hello 世界"},
        },
    ]
    path.write_text("".join(json.dumps(event) + "\n" for event in events))
    return path


class Contract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.db = self.root / "index.sqlite"
        self.source = trace(self.root / "trace.jsonl")
        self.index = ti.Index(db=self.db)

    def tearDown(self):
        self.index.close()
        self.tmp.cleanup()

    def build(self):
        return self.index.sync([self.source])

    def test_native_install_and_offline_discovery(self):
        self.assertTrue(ti._native.__file__.endswith((".so", ".pyd")))
        self.assertTrue(Path(ti.__file__).with_name("py.typed").exists())
        self.assertTrue(Path(ti.__file__).with_name("__init__.pyi").exists())
        with patch.object(
            subprocess, "Popen", side_effect=AssertionError("no subprocess")
        ):
            self.assertIn("Start Here", ti.doc("start-here"))
            self.assertTrue(any(topic.name == "start-here" for topic in ti.docs()))
            with self.assertRaises(ValueError):
                ti.doc("not-a-topic")
            self.build()
            self.assertEqual(self.index.query("SELECT 1 AS n").rows[0]["n"], 1)
        self.assertIn(":name", ti.Index.query.__doc__)

    def test_selection_and_reads_do_not_create(self):
        self.assertEqual(self.index.database, self.db)
        self.assertFalse(self.db.exists())
        with self.assertRaises(ti.IndexUnavailableError):
            self.index.query("SELECT 1")
        self.assertFalse(self.db.exists())
        with self.assertRaises(ti.IndexUnavailableError):
            ti.Index.open(db=self.db)
        self.assertFalse(self.db.exists())

    def test_native_sync_emits_no_output(self):
        # Capture OS pipes, not just Python's sys.stdout/sys.stderr objects.
        run = subprocess.run(
            [
                sys.executable,
                "-c",
                "import sys; from trace_index import Index; "
                "index=Index(db=sys.argv[1]); index.sync([sys.argv[2]]); index.close()",
                str(self.db),
                str(self.source),
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual((run.stdout, run.stderr), ("", ""))
        self.assertEqual(self.index.status().sources, 1)

    def test_sync_preserves_symlink_parent_semantics(self):
        target = self.root / "physical"
        (target / "subdir").mkdir(parents=True)
        (target / "traces").mkdir()
        (self.root / "traces").mkdir()
        trace(target / "traces" / "wanted.jsonl", "wanted")
        trace(self.root / "traces" / "wrong.jsonl", "wrong")
        link = self.root / "link"
        link.symlink_to(target / "subdir", target_is_directory=True)
        cwd = Path.cwd()
        try:
            os.chdir(self.root)
            self.index.sync([Path("link") / ".." / "traces"])
        finally:
            os.chdir(cwd)
        self.assertEqual(
            self.index.query("SELECT native_id FROM sessions").rows[0]["native_id"],
            "wanted",
        )

    def test_sync_incremental_append_rebuild(self):
        report = self.build()
        self.assertTrue(report.complete)
        self.assertEqual(report.indexed_files, 1)
        self.assertEqual(len(report.source_results), 1)
        self.assertEqual(self.build().unchanged_files, 1)
        before = self.index.query("SELECT count(*) AS n FROM records").rows[0]["n"]
        with self.source.open("a") as f:
            f.write(
                json.dumps(
                    {
                        "type": "event_msg",
                        "payload": {"type": "agent_message", "message": "finished"},
                    }
                )
                + "\n"
            )
        self.assertEqual(
            self.index.query("SELECT count(*) AS n FROM records").rows[0]["n"], before
        )
        self.build()
        self.assertEqual(
            self.index.query("SELECT count(*) AS n FROM records").rows[0]["n"],
            before + 1,
        )
        self.assertEqual(self.index.sync([self.source], rebuild=True).rebuilt_files, 1)
        self.assertEqual(self.index.status().sources, 1)

    def test_real_parameter_binding_and_types(self):
        self.build()
        params = {
            "text": "'\n世界; DROP TABLE sessions;--",
            "integer": 2**63 - 1,
            "real": 1.25,
            "blob": b"\x00\xff",
            "null": None,
            "boolean": True,
        }
        result = self.index.query(
            "SELECT :text AS text, :integer AS integer, :real AS real, :blob AS blob, :null AS n, :boolean AS b, :text AS again",
            parameters=params,
        ).require_complete()
        row = result.rows[0]
        self.assertEqual(row["text"], params["text"])
        self.assertEqual(row["again"], row["text"])
        self.assertEqual(row["integer"], params["integer"])
        self.assertEqual(row["real"], 1.25)
        self.assertEqual(row["blob"], b"\x00\xff")
        self.assertIsNone(row["n"])
        self.assertEqual(row["b"], 1)
        self.assertEqual(
            self.index.query(
                "SELECT native_id FROM sessions WHERE working_directory=:p",
                parameters={"p": "/work/reader's project"},
            ).returned,
            1,
        )
        self.assertIsInstance(
            self.index.query("SELECT semantic FROM items LIMIT 1").rows[0]["semantic"],
            str,
        )

    def test_parameter_errors_do_not_poison_connection(self):
        self.build()
        for sql, params in [
            ("SELECT :x", {}),
            ("SELECT 1", {"x": 1}),
            ("SELECT ?1", {"x": 1}),
            ("SELECT $x", {"x": 1}),
            ("SELECT :x", {"x": 2**63}),
            ("SELECT :x", {"x": float("nan")}),
            ("SELECT :x", {"x": float("inf")}),
        ]:
            with (
                self.subTest(sql=sql, params=params),
                self.assertRaises(ti.ParameterError),
            ):
                self.index.query(sql, parameters=params)
        for value in [[], {}, Path("p")]:
            with self.assertRaises(TypeError):
                self.index.query("SELECT :x", parameters={"x": value})
        self.assertEqual(self.index.query("SELECT 7 AS n").rows[0]["n"], 7)

    def test_readonly_and_single_statement(self):
        self.build()
        for sql in [
            "DELETE FROM domain_items",
            "CREATE TABLE leaked(n)",
            "ATTACH ':memory:' AS external",
            "SELECT 1; SELECT 2",
            "SELECT 1 AS n, 2 AS n",
            "SELECT * FROM nonexistent",
        ]:
            with self.subTest(sql=sql), self.assertRaises(ti.QueryError):
                self.index.query(sql)
        self.assertEqual(self.index.query("SELECT 9 AS n").rows[0]["n"], 9)

    def test_budgets_are_independent(self):
        self.build()
        result = self.index.query(
            "SELECT 1 AS n UNION ALL SELECT 2", budget=ti.QueryBudget(limit=1)
        )
        self.assertFalse(result.complete)
        self.assertEqual(result.incomplete_reason, "row_limit")
        with self.assertRaises(ti.IncompleteResultError) as caught:
            result.require_complete()
        self.assertIs(caught.exception.result, result)
        result = self.index.query(
            "SELECT '世界' AS text, x'00112233' AS blob",
            budget=ti.QueryBudget(max_cell_bytes=3),
        )
        self.assertTrue(result.complete)
        self.assertEqual(result.cells_truncated, 2)
        self.assertEqual(result.rows[0]["text"], "世")
        self.assertEqual(result.rows[0]["blob"], b"\x00\x11\x22")
        with self.assertRaises(ti.IncompleteResultError):
            result.require_complete()
        result = self.index.query(
            "SELECT printf('%2000s','x') AS text",
            budget=ti.QueryBudget(max_output_bytes=1024),
        )
        self.assertFalse(result.complete)
        self.assertEqual(result.returned, 0)
        self.assertEqual(result.incomplete_reason, "output_budget")
        self.assertNotIn("2000", repr(result))
        for kwargs in [
            {"limit": 0},
            {"limit": True},
            {"max_cell_bytes": 0},
            {"max_output_bytes": 1},
            {"timeout_seconds": float("nan")},
            {"timeout_seconds": 0},
        ]:
            with self.assertRaises(ValueError):
                ti.QueryBudget(**kwargs)

    def test_timeout_and_gil_release(self):
        self.build()
        ticks = []
        done = threading.Event()

        def worker():
            while not done.wait(0.002):
                ticks.append(time.monotonic())

        thread = threading.Thread(target=worker)
        thread.start()
        try:
            with self.assertRaises(ti.QueryTimeoutError) as caught:
                self.index.query(
                    "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x) SELECT sum(n) FROM x",
                    budget=ti.QueryBudget(timeout_seconds=0.1),
                )
        finally:
            done.set()
            thread.join()
        self.assertGreater(len(ticks), 2)
        message = str(caught.exception)
        self.assertIn("0.1-second timeout", message)
        self.assertNotIn("0-second timeout", message)
        self.assertNotIn("query timeout: interrupted", message)
        self.assertEqual(self.index.query("SELECT 1 AS n").returned, 1)

    def test_fractional_timeout_diagnostics_and_connection_recovery(self):
        self.build()
        for seconds in (0.000001, 0.01, 1.25):
            with self.subTest(seconds=seconds):
                with self.assertRaises(ti.QueryTimeoutError) as caught:
                    self.index.query(
                        "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x) SELECT sum(n) FROM x",
                        budget=ti.QueryBudget(timeout_seconds=seconds),
                    )
                # Decimal formatting need not match Python's exponent notation.
                message = str(caught.exception)
                duration = re.search(
                    r"query exceeded the ([0-9.e+-]+)-second timeout;", message
                )
                self.assertIsNotNone(duration, message)
                self.assertEqual(float(duration.group(1)), seconds)
                self.assertIn("COUNT or GROUP BY", message)
                self.assertNotIn("--", message)
                self.assertEqual(self.index.query("SELECT 1 AS n").rows[0]["n"], 1)

    def test_schema_lookup_errors_are_distinct_from_index_access_errors(self):
        self.build()
        with self.assertRaises(ti.UnknownRelationError) as caught:
            self.index.schema("not_a_public_relation")
        self.assertIsInstance(caught.exception, ti.TraceIndexError)
        self.assertNotIsInstance(caught.exception, ti.IndexUnavailableError)
        self.assertIn("not_a_public_relation", str(caught.exception))
        self.assertEqual(self.index.schema("items").relations[0].name, "items")
        self.assertEqual(self.index.status().sources, 1)
        missing = self.root / "missing.sqlite"
        with ti.Index(db=missing) as index:
            with self.assertRaises(ti.IndexUnavailableError):
                index.schema("items")
        self.assertFalse(missing.exists())
        incompatible = self.root / "incompatible.sqlite"
        with sqlite3.connect(incompatible) as db:
            db.execute("PRAGMA user_version=2147483647")
        with ti.Index(db=incompatible) as index:
            with self.assertRaises(ti.IndexUnavailableError):
                index.schema("items")
        self.index.close()
        with self.assertRaises(ti.ClosedIndexError):
            self.index.schema("not_a_public_relation")

    def test_parallel_same_and_separate_handles(self):
        self.build()
        with ThreadPoolExecutor(max_workers=4) as pool:
            values = list(
                pool.map(
                    lambda n: self.index.query(
                        "SELECT :n AS n", parameters={"n": n}
                    ).rows[0]["n"],
                    range(20),
                )
            )
        self.assertEqual(values, list(range(20)))
        with ti.Index.open(db=self.db) as second:
            with ThreadPoolExecutor(max_workers=2) as pool:
                results = list(
                    pool.map(
                        lambda index: index.query(
                            "SELECT count(*) AS n FROM items"
                        ).rows[0]["n"],
                        [self.index, second],
                    )
                )
            self.assertEqual(results[0], results[1])

    def test_lifecycle_and_detached_results(self):
        self.build()
        row = self.index.query("SELECT 1 AS n")
        record = self.index.record(1)
        self.index.close()
        self.index.close()
        self.assertEqual(row.rows[0]["n"], 1)
        with self.assertRaises(TypeError):
            row.rows[0]["n"] = 2
        for action in [
            lambda: self.index.query("SELECT 1"),
            self.index.status,
            self.index.schema,
            record.verify,
            record.inspect,
            lambda: self.index.sync([self.source]),
        ]:
            with self.assertRaises(ti.ClosedIndexError):
                action()
        with ti.Index.open(db=self.db) as other:
            other.query("SELECT 1")
        with self.assertRaises(ti.ClosedIndexError):
            other.query("SELECT 1")

    def test_record_evidence_and_source_changes(self):
        self.build()
        rid = self.index.query(
            "SELECT record_id FROM records ORDER BY record_id LIMIT 1"
        ).rows[0]["record_id"]
        record = self.index.record(rid)
        self.assertEqual(record.verify().status, "verified")
        inspected = record.inspect()
        self.assertTrue(inspected.raw_verified)
        self.assertEqual(inspected.representation["type"], "session_meta")
        self.assertGreater(
            record.inspect(
                budget=ti.InspectionBudget(max_record_bytes=1)
            ).externalized_values,
            0,
        )
        original = self.source.read_bytes()
        self.source.write_bytes(original.replace(b"sample", b"tamper"))
        self.assertEqual(record.verify().status, "hash_mismatch")
        self.assertFalse(record.inspect().raw_verified)
        self.source.write_bytes(b"{}")
        self.assertEqual(record.verify().status, "source_short")
        self.source.unlink()
        self.assertEqual(record.verify().status, "source_missing")
        self.assertFalse(record.inspect().raw_verified)
        with self.assertRaises(ti.RecordError):
            self.index.record(999999).verify()

    def test_policy_and_invalid_sync_inputs(self):
        self.index.sync([self.source], max_record_bytes=8192, max_text_bytes=1024)
        self.assertEqual(self.build().indexing_policy.max_published_text_bytes, 1024)
        with self.assertRaises(ti.SyncError):
            self.index.sync([self.source], rebuild=True, max_text_bytes=2048)
        self.assertEqual(
            self.index.status().indexing_policy.max_published_text_bytes, 1024
        )
        for paths in [str(self.source), self.source]:
            with self.assertRaises(TypeError):
                self.index.sync(paths)
        with self.assertRaises(ValueError):
            self.index.sync([self.source], max_text_bytes=0)
        with self.assertRaises(ti.SyncError):
            self.index.sync([self.root / "missing"])
        self.assertEqual(
            self.index.query("SELECT count(*) AS n FROM sessions").rows[0]["n"], 1
        )

    def test_sync_failure_retains_report_and_committed_sources(self):
        self.build()
        good = trace(self.root / "good.jsonl", "good")
        bad = trace(self.root / "bad.jsonl", "bad")
        with sqlite3.connect(self.db) as db:
            db.execute(
                "CREATE TRIGGER fail_bad BEFORE INSERT ON trace_records WHEN NEW.source_id IN (SELECT id FROM trace_sources WHERE path LIKE '%bad.jsonl') BEGIN SELECT RAISE(FAIL, 'synthetic source failure'); END"
            )
        with self.assertRaises(ti.SyncFailedError) as caught:
            self.index.sync([good, bad])
        report = caught.exception.report
        self.assertEqual(report.failed_files, 1)
        self.assertEqual(report.indexed_files, 1)
        self.assertFalse(report.complete)
        self.assertTrue(
            any(
                source.action == "failed" and source.error
                for source in report.source_results
            )
        )
        self.assertEqual(
            self.index.query(
                "SELECT count(*) AS n FROM sessions WHERE native_id='good'"
            ).rows[0]["n"],
            1,
        )

    def test_configured_roots_and_path_capture(self):
        config = self.root / "config.toml"
        config.write_text(
            'schema_version = 1\ndatabase = "configured.sqlite"\n[[roots]]\npath = "trace.jsonl"\n'
        )
        with ti.Index(config=config) as configured:
            self.assertEqual(configured.sync().indexed_files, 1)
            self.assertEqual(configured.database, self.root / "configured.sqlite")
        with ti.Index(config=config, db=self.db) as override:
            self.assertEqual(override.database, self.db)
            override.sync()
            cwd = Path.cwd()
            try:
                os.chdir(self.root)
                self.assertEqual(
                    override.query("SELECT count(*) AS n FROM sessions").rows[0]["n"], 1
                )
            finally:
                os.chdir(cwd)

    def test_skips_and_incomplete_tail_are_findings(self):
        unsupported = self.root / "unsupported.jsonl"
        unsupported.write_text('{"other": true}\n')
        with self.source.open("a") as f:
            f.write('{"incomplete":')
        report = self.index.sync([self.source, unsupported])
        self.assertTrue(report.complete)
        self.assertEqual(report.skipped_files, 1)
        self.assertEqual(report.incomplete_files, 1)

    @unittest.skipUnless(
        os.environ.get("TRACE_INDEX_CLI"), "compiled CLI path required for parity"
    )
    def test_cli_and_python_report_the_configured_timeout(self):
        self.build()
        sql = "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x) SELECT sum(n) FROM x"
        run = subprocess.run(
            [
                os.environ["TRACE_INDEX_CLI"],
                "--db",
                str(self.db),
                "query",
                "run",
                sql,
                "--timeout-seconds",
                "1",
            ],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(run.returncode, 1)
        self.assertEqual(run.stdout, "")
        with self.assertRaises(ti.QueryTimeoutError) as caught:
            self.index.query(sql, budget=ti.QueryBudget(timeout_seconds=1))
        for message in (run.stderr, str(caught.exception)):
            self.assertIn("query exceeded the 1-second timeout;", message)
            self.assertIn("COUNT or GROUP BY", message)
            self.assertNotIn("query timeout: interrupted", message)
        self.assertEqual(self.index.query("SELECT 1 AS n").rows[0]["n"], 1)

    @unittest.skipUnless(
        os.environ.get("TRACE_INDEX_CLI"), "compiled CLI path required for parity"
    )
    def test_budget_defaults_match_cli(self):
        binary = os.environ["TRACE_INDEX_CLI"]
        for command, budget in [
            (("query", "run"), ti.QueryBudget()),
            (("record", "inspect"), ti.InspectionBudget()),
        ]:
            help_text = subprocess.check_output([binary, *command, "--help"], text=True)
            for field in dataclasses.fields(budget):
                name = field.name.replace("_", "-")
                match = re.search(
                    r"--" + name + r"\b(?:(?!--).)*?\[default: ([0-9.]+)\]",
                    help_text,
                    re.S,
                )
                self.assertIsNotNone(match, name)
                self.assertEqual(float(match.group(1)), getattr(budget, field.name))

    @unittest.skipUnless(
        os.environ.get("TRACE_INDEX_CLI"), "compiled CLI path required for parity"
    )
    def test_compiled_cli_parity(self):
        self.build()
        binary = os.environ["TRACE_INDEX_CLI"]
        self.assertEqual(
            subprocess.check_output([binary, "--version"], text=True).strip(),
            f"trace-index {ti.__version__}",
        )

        def cli(*args):
            return json.loads(
                subprocess.check_output(
                    [binary, "--db", str(self.db), *args], text=True
                )
            )["data"]

        sql = "SELECT session_id, runtime, native_id FROM sessions ORDER BY session_id"
        native = self.index.query(sql)
        external = cli("query", "run", sql)
        self.assertEqual(list(native.rows), external["rows"])
        self.assertEqual(native.complete, external["complete"])
        self.assertEqual(native.cells_truncated, external["cells_truncated"])
        schema = self.index.schema()
        self.assertEqual(
            [r.name for r in schema.relations],
            [r["name"] for r in cli("schema", "list")["objects"]],
        )
        for relation in schema.relations:
            detailed = self.index.schema(relation.name).relations[0]
            self.assertEqual(
                [dataclasses.asdict(c) for c in detailed.columns],
                cli("schema", "get", relation.name)["objects"][0]["columns"],
            )
        self.assertEqual(self.index.status().sources, cli("index", "status")["sources"])
        for sql in [
            "SELECT '世界' AS t, x'00112233' AS b",
            "SELECT printf('%2000s','x') AS t UNION ALL SELECT 'y'",
        ]:
            native = self.index.query(
                sql, budget=ti.QueryBudget(max_cell_bytes=3, max_output_bytes=1024)
            )
            external = cli(
                "query",
                "run",
                sql,
                "--max-cell-bytes",
                "3",
                "--max-output-bytes",
                "1024",
            )
            self.assertEqual(native.returned, external["returned"])
            self.assertEqual(native.complete, external["complete"])
            self.assertEqual(native.cells_truncated, external["cells_truncated"])
        rid = self.index.query(
            "SELECT record_id FROM records ORDER BY record_id LIMIT 1"
        ).rows[0]["record_id"]
        self.assertEqual(
            dataclasses.asdict(self.index.record(rid).verify()),
            cli("record", "verify", str(rid)),
        )
        inspection = dataclasses.asdict(self.index.record(rid).inspect())
        if inspection["record"]["parse"]["error"] is None:
            del inspection["record"]["parse"]["error"]
        self.assertEqual(inspection, cli("record", "inspect", str(rid)))


if __name__ == "__main__":
    unittest.main()
