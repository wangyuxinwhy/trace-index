#!/usr/bin/env python3
"""Reproducible performance benchmark harness for trace-index.

Supports two suites and optional A/B comparison:

1. `synthetic` (default): Generates a deterministic multi-runtime corpus
   (Codex, Claude Code, Pi, Antigravity) with configurable size, then measures
   cold bulk indexing, no-op incremental sync, hot single-record append sync,
   core query latencies (P50/P95), `index status`, and `record verify`.
2. `live`: Runs the read-only query matrix, `index status`, and `record verify`
   against an existing database (never mutating it), plus optional isolated
   CoW/copy append-only benchmarking on a supplied trace file.

Examples:
    python3 scripts/bench.py --bin ./target/release/trace-index
    python3 scripts/bench.py --suite synthetic --sessions 400 --loops 10
    python3 scripts/bench.py --suite live --db ~/.local/share/trace-index/unified-v4.sqlite
    python3 scripts/bench.py --baseline /path/to/old --bin ./target/release/trace-index
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


QUERY_MATRIX: list[tuple[str, str]] = [
    (
        "recent_requests",
        """
        SELECT item_id, session_id, occurred_at
        FROM items
        WHERE json_extract(semantic, '$.role') = 'human.request'
        ORDER BY occurred_at DESC
        LIMIT 25
        """,
    ),
    (
        "recent_requests_with_blobs",
        """
        SELECT i.item_id, i.session_id, i.occurred_at, substr(b.text, 1, 120) AS preview
        FROM items i
        LEFT JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
        WHERE json_extract(i.semantic, '$.role') = 'human.request'
        ORDER BY i.occurred_at DESC
        LIMIT 25
        """,
    ),
    (
        "reverse_join_call_to_output",
        """
        SELECT c.item_id AS call_id, o.item_id AS output_id,
               json_extract(o.semantic, '$.value.exit_code') AS exit_code
        FROM items c
        JOIN items o ON json_extract(o.semantic, '$.value.call_item_id') = +c.item_id
        WHERE json_extract(c.semantic, '$.role') = 'agent.tool_call.shell'
          AND json_extract(o.semantic, '$.role') = 'tool.output'
        ORDER BY c.occurred_at DESC
        LIMIT 20
        """,
    ),
    (
        "failed_shell_output_to_call",
        """
        SELECT o.item_id AS output_id, c.item_id AS call_id,
               json_extract(c.semantic, '$.value.tool_name') AS tool_name
        FROM items o
        JOIN items c ON c.item_id = json_extract(o.semantic, '$.value.call_item_id')
        WHERE json_extract(o.semantic, '$.role') = 'tool.output'
          AND (json_extract(o.semantic, '$.value.failed') = 1
               OR json_extract(o.semantic, '$.value.exit_code') != 0)
        ORDER BY o.occurred_at DESC
        LIMIT 20
        """,
    ),
    (
        "tool_name_filter",
        """
        SELECT item_id, session_id, occurred_at
        FROM items
        WHERE json_extract(semantic, '$.value.tool_name') = 'exec_command'
        ORDER BY occurred_at DESC
        LIMIT 25
        """,
    ),
    (
        "fts_terms_cjk",
        """
        SELECT i.item_id, i.session_id, bm25(item_search_terms) AS rank
        FROM item_search_terms
        JOIN items i ON i.item_id = item_search_terms.rowid
        WHERE item_search_terms MATCH search_query('索引 优化')
        ORDER BY rank
        LIMIT 25
        """,
    ),
    (
        "fts_terms_identifier",
        """
        SELECT i.item_id, i.session_id, bm25(item_search_terms) AS rank
        FROM item_search_terms
        JOIN items i ON i.item_id = item_search_terms.rowid
        WHERE item_search_terms MATCH search_query('sync_session')
        ORDER BY rank
        LIMIT 25
        """,
    ),
    (
        "fts_trigram_substring",
        """
        SELECT i.item_id, i.session_id
        FROM item_search
        JOIN items i ON i.item_id = item_search.rowid
        WHERE item_search MATCH trigram_query('persist_diff')
        LIMIT 25
        """,
    ),
    (
        "session_list_recent",
        """
        SELECT session_id, runtime, native_id, created_at
        FROM sessions
        ORDER BY created_at DESC
        LIMIT 25
        """,
    ),
    (
        "count_items_by_role",
        """
        SELECT json_extract(semantic, '$.role') AS role, COUNT(*) AS n
        FROM items
        GROUP BY role
        ORDER BY n DESC
        """,
    ),
]


def run_cli(binary: str, args: list[str], qos_prefix: list[str] | None = None) -> tuple[float, dict[str, Any]]:
    cmd = (qos_prefix or []) + [binary] + args
    t0 = time.perf_counter()
    res = subprocess.run(cmd, capture_output=True, text=True)
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    if res.returncode != 0:
        raise RuntimeError(f"command failed ({res.returncode}): {' '.join(cmd)}\nstderr: {res.stderr}")
    return elapsed_ms, json.loads(res.stdout)


def copy_cow_or_regular(src: Path, dst: Path) -> None:
    if platform.system() == "Darwin":
        res = subprocess.run(["cp", "-c", str(src), str(dst)], capture_output=True)
        if res.returncode == 0:
            return
    shutil.copy2(src, dst)


def generate_synthetic_corpus(root: Path, sessions: int, loops_per_session: int) -> dict[str, Any]:
    """Generates deterministic multi-runtime traces covering Codex, Claude, Pi, and Antigravity."""
    codex_dir = root / "codex"
    claude_dir = root / "claude"
    pi_dir = root / "pi"
    agy_root = root / "brain"
    for d in (codex_dir, claude_dir, pi_dir, agy_root):
        d.mkdir(parents=True, exist_ok=True)

    total_bytes = 0
    hot_codex: Path | None = None
    hot_agy: Path | None = None
    hot_codex_line = ""
    hot_agy_line = ""

    # Scale one Codex and one Antigravity session 4x larger to test hot-file append.
    for idx in range(sessions):
        runtime_slot = idx % 4
        n_loops = loops_per_session * (4 if idx in (0, 3) else 1)
        base_sec = 1_760_000_000 + idx * 3600

        if runtime_slot == 0:
            path = codex_dir / f"rollout-{idx:04d}.jsonl"
            lines = [
                json.dumps({
                    "timestamp": f"2026-10-14T{(idx % 24):02d}:00:00.000Z",
                    "type": "session_meta",
                    "payload": {"id": f"codex-bench-{idx:04d}", "cwd": "/workspace/trace-index"},
                }),
                json.dumps({
                    "timestamp": f"2026-10-14T{(idx % 24):02d}:00:00.050Z",
                    "type": "world_state",
                    "payload": {"full": True, "state": {"note": "x" * 256}},
                }),
            ]
            for lp in range(n_loops):
                ts_prefix = f"2026-10-14T{(idx % 24):02d}:{(lp // 60) % 60:02d}:{lp % 60:02d}"
                call_id = f"call-{idx}-{lp}"
                failed_out = "error: exit status 1" if lp % 5 == 0 else "ok: persist_diff sync_session passed"
                lines.extend([
                    json.dumps({"timestamp": f"{ts_prefix}.100Z", "type": "turn_context", "payload": {"turn_id": f"t-{idx}-{lp}", "model": "gpt-5"}}),
                    json.dumps({"timestamp": f"{ts_prefix}.150Z", "type": "event_msg", "payload": {"type": "user_message", "message": f"请排查第 {lp} 轮数据库 索引 优化 sync_session 与 persist_diff 性能问题 (session {idx})"}}),
                    json.dumps({"timestamp": f"{ts_prefix}.200Z", "type": "response_item", "payload": {"type": "function_call", "name": "exec_command", "call_id": call_id, "arguments": json.dumps({"cmd": f"cargo test --locked persist_diff && echo loop_{lp}", "workdir": "/workspace/trace-index"})}}),
                    json.dumps({"timestamp": f"{ts_prefix}.300Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": call_id, "output": f"Exit code: {1 if lp % 5 == 0 else 0}\nOutput:\n{failed_out}"}}),
                    json.dumps({"timestamp": f"{ts_prefix}.400Z", "type": "response_item", "payload": {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": f"完成第 {lp} 轮索引 优化分析，已验证 sync_session 与 persist_diff。"}]}}),
                    json.dumps({"timestamp": f"{ts_prefix}.500Z", "type": "event_msg", "payload": {"type": "task_complete"}}),
                ])
                if lp % 10 == 9:
                    lines.append(json.dumps({"timestamp": f"{ts_prefix}.550Z", "type": "compacted", "payload": {"message": "", "replacement_history": [{"type": "message", "content": "h" * 512}] * 8}}))
            payload = "\n".join(lines) + "\n"
            path.write_text(payload, encoding="utf-8")
            total_bytes += len(payload.encode("utf-8"))
            if idx == 0:
                hot_codex = path
                hot_codex_line = json.dumps({
                    "timestamp": "2026-10-14T23:59:59.000Z",
                    "type": "response_item",
                    "payload": {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "appended codex summary"}]},
                })

        elif runtime_slot == 1:
            path = claude_dir / f"claude-{idx:04d}.jsonl"
            sid = f"claude-bench-{idx:04d}"
            lines = []
            for lp in range(n_loops):
                ts = f"2026-10-15T{(idx % 24):02d}:{(lp // 60) % 60:02d}:{lp % 60:02d}.000Z"
                lines.extend([
                    json.dumps({"type": "user", "sessionId": sid, "uuid": f"u-{idx}-{lp}", "timestamp": ts, "promptId": f"p-{lp}", "message": {"role": "user", "content": f"分析 Claude 索引 优化 sync_session 问题 #{lp}"}}),
                    json.dumps({"type": "assistant", "sessionId": sid, "uuid": f"a-{idx}-{lp}", "timestamp": ts, "message": {"role": "assistant", "model": "claude-sonnet-4", "stop_reason": "end_turn", "content": [{"type": "text", "text": f"已定位 persist_diff 与 索引 优化点 #{lp}"}]}}),
                ])
            payload = "\n".join(lines) + "\n"
            path.write_text(payload, encoding="utf-8")
            total_bytes += len(payload.encode("utf-8"))

        elif runtime_slot == 2:
            path = pi_dir / f"pi-{idx:04d}.jsonl"
            lines = [json.dumps({"type": "session", "version": 3, "id": f"pi-bench-{idx:04d}", "cwd": "/workspace/trace-index", "timestamp": "2026-10-16T00:00:00.000Z"})]
            for lp in range(n_loops):
                ts = f"2026-10-16T{(idx % 24):02d}:{(lp // 60) % 60:02d}:{lp % 60:02d}.000Z"
                lines.extend([
                    json.dumps({"type": "message", "id": f"m-u-{lp}", "timestamp": ts, "message": {"role": "user", "content": f"检查 Pi 索引 优化与 persist_diff #{lp}"}}),
                    json.dumps({"type": "message", "id": f"m-a-{lp}", "timestamp": ts, "message": {"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": f"Pi 排查 sync_session 完成 #{lp}"}]}}),
                ])
            payload = "\n".join(lines) + "\n"
            path.write_text(payload, encoding="utf-8")
            total_bytes += len(payload.encode("utf-8"))

        else:
            conv_uuid = f"00000000-0000-4000-8000-{idx:012x}"
            logs = agy_root / conv_uuid / ".system_generated" / "logs"
            logs.mkdir(parents=True, exist_ok=True)
            path = logs / "transcript_full.jsonl"
            lines = []
            step = 0
            for lp in range(n_loops):
                ts = f"2026-10-17T{(idx % 24):02d}:{(lp // 60) % 60:02d}:{lp % 60:02d}Z"
                lines.append(json.dumps({"step_index": step, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "created_at": ts, "content": f"Antigravity 索引 优化排查 #{lp} sync_session"}))
                step += 1
                lines.append(json.dumps({"step_index": step, "source": "MODEL", "type": "GENERIC", "status": "DONE", "created_at": ts, "content": "display noise " * 40}))
                step += 1
                lines.append(json.dumps({"step_index": step, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": ts, "model": "gemini-2.5", "thinking": f"checking persist_diff #{lp}", "content": f"已完成 #{lp} 索引 优化", "tool_calls": [{"name": "run_command", "args": {"CommandLine": f"cargo clippy --locked && echo step_{lp}", "Cwd": "/workspace/trace-index"}}]}))
                step += 1
            payload = "\n".join(lines) + "\n"
            path.write_text(payload, encoding="utf-8")
            total_bytes += len(payload.encode("utf-8"))
            if idx == 3:
                hot_agy = path
                hot_agy_line = json.dumps({"step_index": step, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-10-17T23:59:59Z", "content": "appended agy response"})

    return {
        "roots": [codex_dir, claude_dir, pi_dir, agy_root],
        "sessions": sessions,
        "total_bytes": total_bytes,
        "hot_codex": hot_codex,
        "hot_codex_line": hot_codex_line,
        "hot_agy": hot_agy,
        "hot_agy_line": hot_agy_line,
    }


def benchmark_queries(binary: str, db_path: Path, iterations: int) -> dict[str, dict[str, Any]]:
    results: dict[str, dict[str, Any]] = {}

    # 1. index status
    times = []
    for _ in range(iterations):
        ms, _ = run_cli(binary, ["--db", str(db_path), "index", "status"])
        times.append(ms)
    results["index_status"] = summarize_times(times)

    # 2. Core SQL query matrix
    for name, sql in QUERY_MATRIX:
        times = []
        row_count = 0
        for _ in range(iterations):
            ms, out = run_cli(binary, ["--db", str(db_path), "query", "run", sql])
            times.append(ms)
            row_count = len(out["data"]["rows"])
        entry = summarize_times(times)
        entry["rows"] = row_count
        results[f"query:{name}"] = entry

    # 3. record verify on one real record_id
    _, sample = run_cli(binary, ["--db", str(db_path), "query", "run", "SELECT record_id FROM records LIMIT 1"])
    if sample["data"]["rows"]:
        rec_id = str(sample["data"]["rows"][0]["record_id"])
        times = []
        for _ in range(iterations):
            ms, _ = run_cli(binary, ["--db", str(db_path), "record", "verify", rec_id])
            times.append(ms)
        results["record_verify"] = summarize_times(times)

    return results


def summarize_times(samples: list[float]) -> dict[str, Any]:
    ordered = sorted(samples)
    p95_idx = min(len(ordered) - 1, max(0, int(round(0.95 * (len(ordered) - 1)))))
    return {
        "p50_ms": round(statistics.median(ordered), 2),
        "p95_ms": round(ordered[p95_idx], 2),
        "min_ms": round(ordered[0], 2),
    }


def run_synthetic_suite(binary: str, corpus_dir: Path, corpus: dict[str, Any], iterations: int) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix="ti-bench-db-") as tmp:
        db_path = Path(tmp) / "bench.sqlite"
        root_args = [str(r) for r in corpus["roots"]]

        # 1. Cold bulk sync
        cold_wall_ms, cold_out = run_cli(binary, ["--db", str(db_path), "index", "sync"] + root_args)
        cold_m = cold_out["data"]["metrics"]
        db_bytes = cold_m["database_bytes_after"]
        src_mb = corpus["total_bytes"] / (1024 * 1024)

        # 2. No-op incremental sync
        noop_wall_ms, noop_out = run_cli(binary, ["--db", str(db_path), "index", "sync"] + root_args)

        # 3. Hot append-only sync (clone DB + traces so corpus_dir stays untouched for A/B)
        append_metrics: dict[str, Any] = {}
        for label, src_trace, append_line in [
            ("append_codex", corpus["hot_codex"], corpus["hot_codex_line"]),
            ("append_antigravity", corpus["hot_agy"], corpus["hot_agy_line"]),
        ]:
            if src_trace is None:
                continue
            append_db = Path(tmp) / f"{label}.sqlite"
            copy_cow_or_regular(db_path, append_db)
            rel = src_trace.relative_to(corpus_dir)
            trace_copy = Path(tmp) / f"{label}_src" / rel
            trace_copy.parent.mkdir(parents=True, exist_ok=True)
            copy_cow_or_regular(src_trace, trace_copy)
            con = sqlite3.connect(append_db)
            con.execute(
                "UPDATE trace_sources SET path = ? WHERE path = ?",
                (str(trace_copy.resolve()), str(src_trace.resolve())),
            )
            con.commit()
            con.close()
            with trace_copy.open("a", encoding="utf-8") as handle:
                handle.write(append_line + "\n")
            wall_ms, out = run_cli(binary, ["--db", str(append_db), "index", "sync", str(trace_copy)])
            m = out["data"]["metrics"]
            append_metrics[label] = {
                "wall_ms": round(wall_ms, 2),
                "elapsed_ms": m["elapsed_ms"],
                "persist_ms": m["phases_ms"]["persist"],
                "written_items": m["writes"]["items"],
            }

        # 4. Query & status matrix
        queries = benchmark_queries(binary, db_path, iterations)

        return {
            "suite": "synthetic",
            "binary": binary,
            "corpus": {
                "sessions": corpus["sessions"],
                "source_mb": round(src_mb, 2),
                "db_mb": round(db_bytes / (1024 * 1024), 2),
                "compression_ratio": round(db_bytes / max(corpus["total_bytes"], 1), 2),
                "indexed_items": cold_out["data"]["indexed_items"],
            },
            "cold_sync": {
                "wall_ms": round(cold_wall_ms, 2),
                "elapsed_ms": cold_m["elapsed_ms"],
                "mb_per_sec": round(src_mb / max(cold_wall_ms / 1000.0, 0.001), 2),
                "phases_ms": cold_m["phases_ms"],
            },
            "noop_sync": {
                "wall_ms": round(noop_wall_ms, 2),
                "elapsed_ms": noop_out["data"]["metrics"]["elapsed_ms"],
                "unchanged_files": noop_out["data"]["unchanged_files"],
            },
            "append_sync": append_metrics,
            "queries": queries,
        }


def run_live_suite(
    binary: str,
    db_path: Path,
    iterations: int,
    append_trace: Path | None,
    background_qos: bool,
) -> dict[str, Any]:
    qos = ["taskpolicy", "-b"] if (background_qos and platform.system() == "Darwin") else []
    queries = benchmark_queries(binary, db_path, iterations)
    append_result: dict[str, Any] = {}

    if append_trace is not None:
        with tempfile.TemporaryDirectory(prefix="ti-live-append-") as tmp:
            db_copy = Path(tmp) / "live_clone.sqlite"
            copy_cow_or_regular(db_path, db_copy)
            resolved = append_trace.resolve()
            # Preserve Antigravity `brain/<uuid>/.system_generated/logs/` layout when present.
            parts = resolved.parts
            if "brain" in parts:
                rel = Path(*parts[parts.index("brain") :])
            else:
                rel = Path(resolved.name)
            trace_copy = Path(tmp) / rel
            trace_copy.parent.mkdir(parents=True, exist_ok=True)
            copy_cow_or_regular(resolved, trace_copy)
            with trace_copy.open("rb") as handle:
                handle.seek(0, os.SEEK_END)
                handle.seek(max(0, handle.tell() - 65536))
                last_line = handle.read().splitlines()[-1]
            con = sqlite3.connect(db_copy)
            updated = con.execute(
                "UPDATE trace_sources SET path = ? WHERE path = ?",
                (str(trace_copy.resolve()), str(resolved)),
            ).rowcount
            con.commit()
            con.close()
            if updated != 1:
                raise RuntimeError(f"trace path not found in database: {resolved}")
            for tag in ("cold", "warm"):
                with trace_copy.open("ab") as handle:
                    handle.write(last_line + b"\n")
                wall_ms, out = run_cli(
                    binary, ["--db", str(db_copy), "index", "sync", str(trace_copy)], qos_prefix=qos
                )
                m = out["data"]["metrics"]
                append_result[tag] = {
                    "wall_ms": round(wall_ms, 2),
                    "elapsed_ms": m["elapsed_ms"],
                    "phases_ms": m["phases_ms"],
                    "writes": m["writes"],
                }

    return {
        "suite": "live",
        "binary": binary,
        "database": str(db_path),
        "queries": queries,
        "append_sync": append_result,
    }


def flatten_metrics(report: dict[str, Any]) -> list[tuple[str, float]]:
    rows: list[tuple[str, float]] = []
    if "cold_sync" in report:
        rows.append(("cold_sync.wall_ms", report["cold_sync"]["wall_ms"]))
    if "noop_sync" in report:
        rows.append(("noop_sync.wall_ms", report["noop_sync"]["wall_ms"]))
    for name, val in report.get("append_sync", {}).items():
        rows.append((f"append.{name}.wall_ms", val["wall_ms"]))
    for name, val in report.get("queries", {}).items():
        rows.append((f"{name}.p50_ms", val["p50_ms"]))
    return rows


def print_single_markdown(report: dict[str, Any]) -> None:
    print(f"\n### Trace Index Benchmark ({report['suite']}) — `{report['binary']}`")
    if "corpus" in report:
        c = report["corpus"]
        print(
            f"Corpus: {c['sessions']} sessions, {c['source_mb']} MB source -> "
            f"{c['db_mb']} MB DB ({c['compression_ratio']}x), {c['indexed_items']} items"
        )
    print("\n| Metric | Value |")
    print("| --- | ---: |")
    for key, val in flatten_metrics(report):
        print(f"| `{key}` | {val:.2f} ms |")


def print_comparison_markdown(base: dict[str, Any], cand: dict[str, Any]) -> None:
    base_map = dict(flatten_metrics(base))
    print(f"\n### A/B Comparison ({cand['suite']})")
    print(f"- Baseline: `{base['binary']}`")
    print(f"- Candidate: `{cand['binary']}`\n")
    print("| Metric | Baseline (ms) | Candidate (ms) | Speedup |")
    print("| --- | ---: | ---: | ---: |")
    for key, cand_val in flatten_metrics(cand):
        base_val = base_map.get(key)
        if base_val is None:
            continue
        speedup = base_val / max(cand_val, 0.01)
        print(f"| `{key}` | {base_val:.2f} | {cand_val:.2f} | **{speedup:.2f}x** |")


def main() -> int:
    parser = argparse.ArgumentParser(description="Reproducible benchmark harness for trace-index.")
    parser.add_argument("--bin", dest="binary", default="./target/release/trace-index", help="Candidate trace-index binary")
    parser.add_argument("--baseline", default=None, help="Optional baseline binary for A/B comparison")
    parser.add_argument("--suite", choices=("synthetic", "live"), default="synthetic")
    parser.add_argument("--sessions", type=int, default=80, help="Synthetic session count (default: 80)")
    parser.add_argument("--loops", type=int, default=8, help="Synthetic loops per session (default: 8)")
    parser.add_argument("--iterations", type=int, default=5, help="Query iterations per benchmark (default: 5)")
    parser.add_argument("--db", type=Path, default=None, help="Database path for --suite live")
    parser.add_argument("--append-trace", type=Path, default=None, help="Optional trace file for isolated --suite live append test")
    parser.add_argument("--background-qos", action="store_true", help="Run live append test under macOS taskpolicy -b")
    parser.add_argument("--json-out", type=Path, default=None, help="Optional path to write JSON benchmark report")
    args = parser.parse_args()

    if args.suite == "synthetic":
        with tempfile.TemporaryDirectory(prefix="ti-bench-corpus-") as corpus_tmp:
            corpus_dir = Path(corpus_tmp)
            corpus = generate_synthetic_corpus(corpus_dir, args.sessions, args.loops)
            cand = run_synthetic_suite(args.binary, corpus_dir, corpus, args.iterations)
            base = run_synthetic_suite(args.baseline, corpus_dir, corpus, args.iterations) if args.baseline else None
    else:
        if args.db is None:
            parser.error("--suite live requires --db <path>")
        cand = run_live_suite(args.binary, args.db.expanduser(), args.iterations, args.append_trace, args.background_qos)
        base = (
            run_live_suite(args.baseline, args.db.expanduser(), args.iterations, args.append_trace, args.background_qos)
            if args.baseline
            else None
        )

    if base is not None:
        print_comparison_markdown(base, cand)
        payload: dict[str, Any] = {"baseline": base, "candidate": cand}
    else:
        print_single_markdown(cand)
        payload = cand

    if args.json_out is not None:
        args.json_out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
