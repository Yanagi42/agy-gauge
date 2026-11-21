#!/usr/bin/env python3
"""Unit tests for agy_token_stats.py using isolated temporary fixtures."""

from __future__ import annotations

import ast
import datetime as dt
import json
import pathlib
import tempfile
import unittest

import agy_token_stats as mod


class AgyTokenStatsTests(unittest.TestCase):
    def _create_fixture(self, root: pathlib.Path) -> None:
        # Session 1: normal multi-turn session with duplicate step_index and a partial trailing line
        s1_logs = root / "brain" / "11111111-aaaa-bbbb-cccc-000000000001" / ".system_generated" / "logs"
        s1_logs.mkdir(parents=True)
        lines_s1 = [
            json.dumps({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-20T16:30:00Z",
                "content": "<USER_REQUEST>\n测试问题\n</USER_REQUEST>",
            }),
            json.dumps({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "RUNNING",
                "created_at": "2026-11-20T16:30:05Z",
                "model": "gemini-4-neon-high",
                "input_tokens": 1000,
                "cache_read_tokens": 0,
                "output_tokens": 10,
            }),
            # Duplicate step_index 1 should override the earlier RUNNING line
            json.dumps({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-20T16:30:06Z",
                "model": "gemini-4-neon-high",
                "input_tokens": 1000,
                "cache_read_tokens": 0,
                "output_tokens": 200,
            }),
            json.dumps({
                "step_index": 3,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-21T02:00:00Z",
                "model": "gemini-4-neon-xhigh",
                "input_tokens": 500,
                "cache_read_tokens": 1200,
                "output_tokens": 300,
            }),
            '{"step_index": 4, "input_tokens": 123',  # Simulated incomplete concurrent write
        ]
        (s1_logs / "transcript.jsonl").write_text("\n".join(lines_s1) + "\n", encoding="utf-8")

        # Session 2: only transcript_full.jsonl exists, with octal-escaped Chinese title in pbtxt
        s2_logs = root / "brain" / "22222222-aaaa-bbbb-cccc-000000000002" / ".system_generated" / "logs"
        s2_logs.mkdir(parents=True)
        (s2_logs / "transcript_full.jsonl").write_text(
            json.dumps({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-19T10:00:00Z",
                "model": "gemini-4-neon-high",
                "input_tokens": 2000,
                "cache_read_tokens": 0,
                "cache_write_tokens": 400,
                "output_tokens": 500,
            }) + "\n",
            encoding="utf-8",
        )

        # Session 3: empty brain directory (no transcript)
        (root / "brain" / "33333333-aaaa-bbbb-cccc-000000000003").mkdir(parents=True)

        # Annotations
        ann_dir = root / "annotations"
        ann_dir.mkdir(parents=True)
        (ann_dir / "11111111-aaaa-bbbb-cccc-000000000001.pbtxt").write_text(
            'title:"中文会话 \\"测试\\""\n', encoding="utf-8"
        )
        # UTF-8 bytes of "测试" are \346\265\213\350\257\225
        (ann_dir / "22222222-aaaa-bbbb-cccc-000000000002.pbtxt").write_text(
            'title:"\\346\\265\\213\\350\\257\\225"\n', encoding="utf-8"
        )

        # Cache metadata
        cache_dir = root / "cache"
        cache_dir.mkdir(parents=True)
        (cache_dir / "conversation_metadata.json").write_text(
            json.dumps({
                "conversations": {
                    "11111111-aaaa-bbbb-cccc-000000000001": {
                        "summary": {
                            "Title": "Fallback Title",
                            "WorkspaceURIs": ["file:///C:/Users/test/proj%20one"],
                        }
                    }
                }
            }),
            encoding="utf-8",
        )

    def test_scan_dedup_partial_line_and_metadata(self) -> None:
        tz_cn = dt.timezone(dt.timedelta(hours=8))
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)

            report = mod.scan_usage(root, tz=tz_cn)
            self.assertEqual(len(report.sessions), 2)
            self.assertEqual(report.skipped_empty_sessions, 1)
            self.assertEqual(report.skipped_lines, 1)

            s1 = next(s for s in report.sessions if s.conversation_id.startswith("11111111"))
            self.assertEqual(s1.title, '中文会话 "测试"')
            self.assertEqual(s1.workspace, "C:/Users/test/proj one")
            self.assertEqual(s1.counts.calls, 2)
            self.assertEqual(s1.counts.input_tokens, 1500)
            self.assertEqual(s1.counts.cache_read_tokens, 1200)
            self.assertEqual(s1.counts.output_tokens, 500)
            self.assertEqual(s1.counts.total_input_tokens, 2700)
            self.assertEqual(s1.counts.total_tokens, 3200)
            self.assertEqual(s1.counts.peak_context, 1700)

            s2 = next(s for s in report.sessions if s.conversation_id.startswith("22222222"))
            self.assertEqual(s2.title, "测试")
            self.assertEqual(s2.counts.cache_write_tokens, 400)
            self.assertEqual(s2.counts.total_input_tokens, 2400)
            self.assertEqual(s2.counts.total_tokens, 2900)

    def test_date_and_model_filters(self) -> None:
        tz_cn = dt.timezone(dt.timedelta(hours=8))
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)

            # 2026-11-20T16:30:06Z is 2026-11-21 00:30:06 +08:00!
            report = mod.scan_usage(
                root,
                tz=tz_cn,
                since=dt.date(2026, 11, 21),
                until=dt.date(2026, 11, 21),
            )
            self.assertEqual(len(report.sessions), 1)
            self.assertEqual(report.sessions[0].counts.calls, 2)

            # Filter by model substring
            report_x = mod.scan_usage(root, tz=tz_cn, model_filter="xhigh")
            self.assertEqual(len(report_x.sessions), 1)
            self.assertEqual(report_x.sessions[0].counts.calls, 1)
            self.assertEqual(report_x.sessions[0].counts.input_tokens, 500)

            # Filter by workspace substring
            report_ws = mod.scan_usage(root, tz=tz_cn, workspace_filter="proj one")
            self.assertEqual(len(report_ws.sessions), 1)
            self.assertTrue(report_ws.sessions[0].conversation_id.startswith("11111111"))

    def test_json_csv_and_text_render(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)
            report = mod.scan_usage(root, tz=tz_utc)
            agg = mod._aggregate(report.sessions)

            js = json.loads(mod.render_json(report, agg, sort_by="time", tz_label="UTC", include_turns=True))
            self.assertEqual(js["session_count"], 2)
            self.assertEqual(js["overview"]["total_tokens"], 6100)

            csv_out = mod.render_csv(report, agg, view="session", sort_by="total")
            self.assertIn("conversation_id", csv_out.splitlines()[0])
            self.assertEqual(len(csv_out.splitlines()), 3)

            txt = mod.render_text(
                report,
                agg,
                view="all",
                sort_by="time",
                limit=10,
                human=False,
                tz_label="UTC",
                since=None,
                until=None,
                model_filter=None,
                session_filter=None,
                color=False,
            )
            self.assertIn("By Model", txt)
            self.assertIn("By Workspace", txt)
            self.assertIn("中文会话", txt)

    def test_weekly_hourly_and_incremental_cache(self) -> None:
        tz_cn = dt.timezone(dt.timedelta(hours=8))
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)

            scanner = mod.IncrementalScanner(root)
            rep1 = scanner.scan(tz=tz_cn)
            self.assertEqual(rep1.files_reparsed, 2)
            self.assertEqual(rep1.files_cached, 0)

            agg = mod.aggregate_sessions(rep1.sessions)
            # 2026-11-19 (Thu), 2026-11-21 (Sat) -> both in 2026-W47
            self.assertIn("2026-W47", agg["by_week"])
            self.assertEqual(agg["week_active_days"]["2026-W47"], 2)
            self.assertEqual(agg["by_week"]["2026-W47"].calls, 3)
            # 16:30Z -> 00:30+08, 02:00Z -> 10:00+08, 10:00Z -> 18:00+08
            self.assertEqual(agg["by_hour"][0].calls, 1)
            self.assertEqual(agg["by_hour"][10].calls, 1)
            self.assertEqual(agg["by_hour"][18].calls, 1)

            # Second scan should hit cache with 0 reparses
            rep2 = scanner.scan(tz=tz_cn)
            self.assertEqual(rep2.files_reparsed, 0)
            self.assertEqual(rep2.files_cached, 2)

            # Append a new turn to session 1 -> only 1 file should reparse
            s1_file = root / "brain" / "11111111-aaaa-bbbb-cccc-000000000001" / ".system_generated" / "logs" / "transcript.jsonl"
            with s1_file.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps({
                        "step_index": 5,
                        "source": "MODEL",
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-21T03:00:00Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 100,
                        "cache_read_tokens": 200,
                        "output_tokens": 50,
                    }) + "\n"
                )
            rep3 = scanner.scan(tz=tz_cn)
            self.assertEqual(rep3.files_reparsed, 1)
            self.assertEqual(rep3.files_cached, 1)

    def test_checkpoint_compaction_and_hit_sort(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)
            s1_file = root / "brain" / "11111111-aaaa-bbbb-cccc-000000000001" / ".system_generated" / "logs" / "transcript.jsonl"
            with s1_file.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps({
                        "step_index": 9,
                        "type": "CHECKPOINT",
                        "status": "DONE",
                        "created_at": "2026-11-21T04:00:00Z",
                        "input_tokens": 500,
                        "cache_read_tokens": 5000,
                        "output_tokens": 300,
                    }) + "\n"
                )
            rep = mod.scan_usage(root, tz=tz_utc)
            agg = mod.aggregate_sessions(rep.sessions)
            self.assertIn("compaction", agg["by_model"])
            self.assertNotIn("unknown", agg["by_model"])
            s1 = next(s for s in rep.sessions if s.conversation_id.startswith("11111111"))
            self.assertEqual(mod._primary_model(s1), "gemini-4-neon-xhigh")
            ordered_hit = mod._sort_sessions(rep.sessions, "hit")
            # Multi-turn s1 comes ahead of single-turn s2 on --sort hit
            self.assertEqual(ordered_hit[0].conversation_id, s1.conversation_id)

    def test_no_file_writes_in_cli_source(self) -> None:
        base_dir = pathlib.Path(mod.__file__).parent
        forbidden_imports = {"sqlite3", "subprocess", "urllib.request", "socket", "http", "shutil"}
        forbidden_attrs = {"write_text", "write_bytes", "unlink", "rmdir", "mkdir", "rename"}

        for filename in ("agy_token_stats.py",):
            src = (base_dir / filename).read_text(encoding="utf-8")
            tree = ast.parse(src, filename=filename)
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        self.assertNotIn(alias.name, forbidden_imports, f"{filename} imports {alias.name}")
                elif isinstance(node, ast.ImportFrom):
                    if node.module:
                        self.assertNotIn(node.module, forbidden_imports, f"{filename} imports {node.module}")
                elif isinstance(node, ast.Call):
                    if isinstance(node.func, ast.Attribute):
                        if node.func.attr in forbidden_attrs:
                            self.fail(f"{filename}: forbidden write/mutation method called: {node.func.attr}")
                        if node.func.attr == "open":
                            self.assertGreaterEqual(len(node.args), 1)
                            self.assertIsInstance(node.args[0], ast.Constant)
                            self.assertEqual(node.args[0].value, "r")
                    elif isinstance(node.func, ast.Name) and node.func.id == "open":
                        self.fail(f"{filename}: use Path.open('r', ...) instead of bare open()")


if __name__ == "__main__":
    unittest.main()
