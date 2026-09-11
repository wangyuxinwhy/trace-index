"""Exercise Python's normal help/docstring tools against the installed wheel."""

from __future__ import annotations

import contextlib
import doctest
import inspect
import io
import json
import os
from pathlib import Path
import pydoc
import re
import subprocess
import sys
import tempfile
import unittest

import trace_index as ti


class HelpContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.source = self.root / "session.jsonl"
        self.source.write_text(
            "\n".join(
                json.dumps(event)
                for event in [
                    {
                        "type": "session_meta",
                        "payload": {"id": "help-example", "cwd": "/path/to/project"},
                    },
                    {"type": "turn_context", "payload": {"turn_id": "example-loop"}},
                    {
                        "type": "event_msg",
                        "payload": {"type": "user_message", "message": "hello"},
                    },
                ]
            )
            + "\n"
        )
        self.db = self.root / "index.sqlite"

    def tearDown(self):
        self.tmp.cleanup()

    def test_help_import_and_docs_work_without_configuration_or_database(self):
        # Only the test starts a subprocess: the tested package has no CLI dependency.
        config = self.root / "missing-config.toml"
        database = self.root / "missing-database.sqlite"
        code = """
import inspect
import trace_index as ti
from unittest.mock import patch
with patch('subprocess.Popen', side_effect=AssertionError('no subprocess in Help')):
    help(ti)
    help(ti.Index.query)
    help(ti.Index.sync)
    assert 'parameters' in inspect.signature(ti.Index.query).parameters
    assert ti.docs()
    assert ti.doc('start-here').lstrip().startswith('# Start Here')
"""
        run = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "TRACE_INDEX_CONFIG": str(config),
                "TRACE_INDEX_DB": str(database),
                "PATH": "",
            },
            check=False,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stderr, "")
        self.assertFalse(config.exists())
        self.assertFalse(database.exists())
        self.assertIn("Conceptual model:", run.stdout)
        self.assertIn("Semantic role vocabulary:", run.stdout)
        self.assertIn("parameters", run.stdout)
        self.assertIn("SyncFailedError", run.stdout)

    def test_shared_context_is_in_standard_module_help_once(self):
        shared = inspect.getdoc(ti._native)
        self.assertTrue(shared.startswith("Conceptual model:"))
        self.assertEqual(ti.__doc__.count(shared), 1)
        rendered = pydoc.render_doc(ti, renderer=pydoc.plaintext)
        self.assertEqual(rendered.count("Source ──contains──> Record"), 1)
        self.assertLess(rendered.index("Conceptual model:"), rendered.index("CLASSES"))
        for heading in [
            "Semantic model and SQL encoding:",
            "Semantic role vocabulary:",
            "Public SQL surface:",
            "Session identity:",
            "Query correctly:",
            "Investigation defaults:",
            "Operational boundary:",
        ]:
            self.assertIn(heading, rendered)
        self.assertIn("help(ti.Index.query)", rendered)
        self.assertIn('doc("how-to/search-literals")', rendered)
        self.assertNotIn("trace-index --help", inspect.getdoc(ti.Index.query))
        self.assertNotIn("Conceptual model:", inspect.getdoc(ti.Index))
        self.assertNotIn("Conceptual model:", inspect.getdoc(ti.Index.query))

    def test_docstrings_examples_use_standard_doctest(self):
        tested = 0
        targets = [
            ti.docs,
            ti.doc,
            ti.Index,
            ti.Index.open,
            ti.Index.close,
            ti.Index.query,
            ti.Index.schema,
            ti.Index.status,
            ti.Index.sync,
            ti.Index.record,
            ti.Record.verify,
            ti.Record.inspect,
        ]
        for target in targets:
            with (
                self.subTest(target=target.__qualname__),
                ti.Index(db=self.db) as index,
            ):
                index.sync([self.source])
                record_id = index.query(
                    "SELECT record_id FROM records ORDER BY record_id LIMIT 1"
                ).rows[0]["record_id"]
                namespace = {name: getattr(ti, name) for name in ti.__all__}
                namespace.update(
                    index=index,
                    database=self.db,
                    paths=[self.source],
                    record=index.record(record_id),
                    record_id=record_id,
                )
                example = doctest.DocTestParser().get_doctest(
                    inspect.getdoc(target), namespace, target.__qualname__, None, 0
                )
                self.assertTrue(example.examples, target.__qualname__)
                runner = doctest.DocTestRunner()
                output = io.StringIO()
                result = runner.run(example, out=output.write)
                self.assertEqual(result.failed, 0, output.getvalue())
                tested += result.attempted
        self.assertGreaterEqual(tested, len(targets))

    def test_tutorial_python_blocks_run_on_a_disposable_index(self):
        # The tutorial declares database/trace_paths/sql/record_id as reader inputs.
        # Execute its code verbatim using fixture-selected inputs, not rewritten SQL.
        tutorial = ti.doc("start-here")
        blocks = re.findall(r"^```python\n(.*?)^```", tutorial, re.M | re.S)
        self.assertTrue(blocks)
        namespace = {
            "database": self.db,
            "trace_paths": [self.source],
            "sql": "SELECT item_id FROM items ORDER BY item_id",
        }
        output = io.StringIO()
        try:
            with contextlib.redirect_stdout(output):
                for block in blocks:
                    if "index" in namespace and self.db.exists():
                        row = (
                            namespace["index"]
                            .query(
                                "SELECT record_id FROM records ORDER BY record_id LIMIT 1"
                            )
                            .rows[0]
                        )
                        namespace["record_id"] = row["record_id"]
                    exec(compile(block, "start-here Python example", "exec"), namespace)
            self.assertIn("Conceptual model:", output.getvalue())
            with self.assertRaises(ti.ClosedIndexError):
                namespace["index"].query("SELECT 1")
        finally:
            if "index" in namespace:
                namespace["index"].close()

    def test_unknown_topic_recovery_uses_python_discovery(self):
        with self.assertRaises(ValueError) as caught:
            ti.doc("nonexistent-topic")
        self.assertIn("docs()", str(caught.exception))
        self.assertNotIn("trace-index docs list", str(caught.exception))

    def test_shared_query_recovery_does_not_prescribe_cli_flags(self):
        with ti.Index(db=self.db) as index:
            index.sync([self.source])
            cases = [
                (
                    "SELECT 1 AS n UNION ALL SELECT 2",
                    ti.QueryBudget(limit=1),
                    "row limit",
                ),
                (
                    "SELECT printf('%2000s', 'x') AS text",
                    ti.QueryBudget(max_output_bytes=1024),
                    "output budget",
                ),
                (
                    "SELECT 'abcdef' AS text",
                    ti.QueryBudget(max_cell_bytes=1),
                    "cell byte budget",
                ),
            ]
            for sql, budget, reason in cases:
                result = index.query(sql, budget=budget)
                self.assertIn(reason, result.hint)
                self.assertNotIn("--", result.hint)
                with self.assertRaises(ti.IncompleteResultError) as caught:
                    result.require_complete()
                self.assertEqual(str(caught.exception), result.hint)
            with self.assertRaises(ti.QueryError) as caught:
                index.query(r"SELECT 1 AS n\n")
            self.assertIn("real newline characters", str(caught.exception))
            self.assertNotIn("--stdin", str(caught.exception))
            self.assertNotIn("--file", str(caught.exception))
            self.assertEqual(index.query("SELECT 1 AS n\n").rows[0]["n"], 1)

    def test_unknown_relation_recovery_uses_python_discovery(self):
        with ti.Index(db=self.db) as index:
            index.sync([self.source])
            with self.assertRaises(ti.IndexUnavailableError) as caught:
                index.schema("nonexistent-relation")
            self.assertIn("index.schema()", str(caught.exception))
            self.assertNotIn("trace-index schema list", str(caught.exception))
            if os.environ.get("TRACE_INDEX_CLI"):
                run = subprocess.run(
                    [
                        os.environ["TRACE_INDEX_CLI"],
                        "--db",
                        str(self.db),
                        "schema",
                        "get",
                        "nonexistent-relation",
                    ],
                    capture_output=True,
                    text=True,
                )
                self.assertNotEqual(run.returncode, 0)
                self.assertIn("trace-index schema list", run.stderr)

    @unittest.skipUnless(os.environ.get("TRACE_INDEX_CLI"), "compiled CLI required")
    def test_compiled_cli_has_identical_shared_context_and_documents(self):
        binary = os.environ["TRACE_INDEX_CLI"]
        run = subprocess.run(
            [binary, "--help"],
            capture_output=True,
            text=True,
            check=True,
            env={
                **os.environ,
                "TRACE_INDEX_CONFIG": str(self.root / "missing.toml"),
                "TRACE_INDEX_DB": str(self.db),
            },
        )
        self.assertEqual(run.stderr, "")
        shared = inspect.getdoc(ti._native)
        self.assertEqual(run.stdout.count(shared), 1)
        self.assertFalse(self.db.exists())
        for topic in ti.docs():
            cli = subprocess.check_output(
                [binary, "docs", "get", topic.name, "--output", "json"], text=True
            )
            self.assertEqual(ti.doc(topic.name), json.loads(cli)["data"]["content"])
        unknown = subprocess.run(
            [binary, "docs", "get", "nonexistent-topic"], capture_output=True, text=True
        )
        self.assertNotEqual(unknown.returncode, 0)
        self.assertIn("trace-index docs list", unknown.stderr)


if __name__ == "__main__":
    unittest.main()
