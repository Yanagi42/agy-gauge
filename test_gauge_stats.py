#!/usr/bin/env python3
"""Unit tests for gauge_stats.py using isolated temporary fixtures."""

from __future__ import annotations

import ast
import datetime as dt
import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest import mock

import gauge_stats as mod


class GaugeStatsTests(unittest.TestCase):
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

            # Single-session turn detail shows Gap column and Compactions summary
            rep_one = mod.scan_usage(root, tz=tz_utc, session_filter="11111111")
            txt_one = mod.render_text(
                rep_one,
                mod.aggregate_sessions(rep_one.sessions),
                view="summary",
                sort_by="time",
                limit=10,
                human=True,
                tz_label="UTC",
                since=None,
                until=None,
                model_filter=None,
                session_filter="11111111",
                color=False,
            )
            self.assertIn("Compactions=1", txt_one)
            self.assertIn("Gap", txt_one)

            # Model table puts compaction at the bottom secondary row even if its volume is highest
            txt_model = mod.render_text(
                rep,
                agg,
                view="model",
                sort_by="time",
                limit=10,
                human=True,
                tz_label="UTC",
                since=None,
                until=None,
                model_filter=None,
                session_filter=None,
                color=False,
            )
            model_lines = [ln for ln in txt_model.splitlines() if ln.strip()]
            self.assertTrue(model_lines[-1].startswith("compaction (secondary)"))

    def test_api_cost_calculation_and_pricing_override(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._create_fixture(root)
            # Append a compaction checkpoint to verify it inherits the session's real model pricing
            s1_file = root / "brain" / "11111111-aaaa-bbbb-cccc-000000000001" / ".system_generated" / "logs" / "transcript.jsonl"
            with s1_file.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps({
                        "step_index": 9,
                        "type": "CHECKPOINT",
                        "status": "DONE",
                        "created_at": "2026-11-21T04:00:00Z",
                        "input_tokens": 1_000_000,
                        "cache_read_tokens": 0,
                        "output_tokens": 0,
                    }) + "\n"
                )

            rep = mod.scan_usage(root, tz=tz_utc, pricing_path=None)
            agg = mod.aggregate_sessions(rep.sessions)
            s1 = next(s for s in rep.sessions if s.conversation_id.startswith("11111111"))
            # Checkpoint inherits gemini-4-neon-xhigh ($3/M input -> $3.00)
            cp_turn = next(t for t in s1.turns if t.step_type == "CHECKPOINT")
            self.assertAlmostEqual(cp_turn.cost.total_usd, 3.0, places=6)
            self.assertEqual(agg["total"].cost.unpriced_calls, 0)
            self.assertGreater(agg["total"].total_cost_usd, 3.0)

            # Custom pricing.json override (prefix + exact effort override)
            custom_pricing = root / "pricing.json"
            custom_pricing.write_text(
                json.dumps({
                    "models": {
                        "gemini-4-neon": {"input": 10.0, "cache_read": 1.0, "output": 20.0},
                        "gemini-4-neon-xhigh": {"input": 15.0, "cache_read": 1.5, "output": 30.0},
                    }
                }),
                encoding="utf-8",
            )
            rep_custom = mod.scan_usage(root, tz=tz_utc, pricing_path=custom_pricing)
            s1_custom = next(s for s in rep_custom.sessions if s.conversation_id.startswith("11111111"))
            cp_custom = next(t for t in s1_custom.turns if t.step_type == "CHECKPOINT")
            # Checkpoint inherits gemini-4-neon-xhigh (exact $15/M), while gemini-4-neon-high uses prefix ($10/M)
            self.assertAlmostEqual(cp_custom.cost.total_usd, 15.0, places=6)
            cat_custom = mod.PricingCatalog(custom_pricing)
            self.assertAlmostEqual(cat_custom.resolve_rate("gemini-4-neon-high", 0, None).input, 10.0, places=6)
            pricing_txt = mod.render_pricing_table(cat_custom, ["gemini-4-neon-xhigh"], dt.date(2026, 11, 22), False)
            self.assertIn("custom(exact)", pricing_txt)
            self.assertIn("claude-opus-5-5", pricing_txt)

            # Cost sorting and JSON/CSV/text outputs include cost fields
            ordered_cost = mod._sort_sessions(rep_custom.sessions, "cost")
            self.assertEqual(ordered_cost[0].conversation_id, s1_custom.conversation_id)
            js = json.loads(
                mod.render_json(
                    rep_custom,
                    mod.aggregate_sessions(rep_custom.sessions),
                    sort_by="cost",
                    tz_label="UTC",
                    include_turns=True,
                )
            )
            self.assertGreater(js["overview"]["cost_usd"], 10.0)
            self.assertIn("cost_usd", js["sessions"][0]["turns"][0])
            csv_out = mod.render_csv(rep_custom, mod.aggregate_sessions(rep_custom.sessions), view="model", sort_by="cost")
            self.assertIn("cost_usd", csv_out.splitlines()[0])
            txt = mod.render_text(
                rep_custom,
                mod.aggregate_sessions(rep_custom.sessions),
                view="summary",
                sort_by="cost",
                limit=10,
                human=True,
                tz_label="UTC",
                since=None,
                until=None,
                model_filter=None,
                session_filter=None,
                color=False,
            )
            self.assertIn("Estimated Cost", txt)
            self.assertIn("Cost($)", txt)

    def test_subagent_linking_workspace_inheritance_and_long_model_name(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            parent_id = "8c1095e2-1558-4a06-bfde-446204f978aa"
            child_id = "14ee4713-eb4d-4a3c-b6a9-6745e2e553e2"
            p_logs = root / "brain" / parent_id / ".system_generated" / "logs"
            c_logs = root / "brain" / child_id / ".system_generated" / "logs"
            cache_dir = root / "cache"
            p_logs.mkdir(parents=True)
            c_logs.mkdir(parents=True)
            cache_dir.mkdir(parents=True)

            (cache_dir / "conversation_metadata.json").write_text(
                json.dumps({
                    "conversations": {
                        parent_id: {
                            "summary": {
                                "Title": "SubVault Review",
                                "WorkspaceURIs": ["file:///C:/Users/test/agent-home"],
                            }
                        },
                        child_id: {"is_internal": True},
                    }
                }),
                encoding="utf-8",
            )
            (p_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "USER_INPUT",
                        "source": "USER_EXPLICIT",
                        "status": "DONE",
                        "content": "<USER_REQUEST>\nReview SubVault\n</USER_REQUEST>",
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-22T14:48:00Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 1000,
                        "cache_read_tokens": 0,
                        "output_tokens": 100,
                    }),
                    # Subagent reply to parent must never misclassify parent as a subagent.
                    json.dumps({
                        "step_index": 2,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": f"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-22T14:50:00Z sender={child_id} priority=MESSAGE_PRIORITY_HIGH content=Done\n</SYSTEM_MESSAGE>",
                    }),
                ]) + "\n",
                encoding="utf-8",
            )
            (c_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": f"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-22T14:48:47Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=You are executing **Wave 2 · Track A** for `SubVault`.\n</SYSTEM_MESSAGE>",
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-22T14:49:00Z",
                        "model": "gemini-4-hydrogen-tiered-xhigh",
                        "input_tokens": 2000,
                        "cache_read_tokens": 5000,
                        "output_tokens": 300,
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            rep = mod.scan_usage(root, tz=tz_utc, pricing_path=None)
            self.assertEqual(len(rep.sessions), 2)
            parent = next(s for s in rep.sessions if s.conversation_id == parent_id)
            child = next(s for s in rep.sessions if s.conversation_id == child_id)
            self.assertFalse(parent.is_subagent)
            self.assertIsNone(parent.parent_id)
            self.assertTrue(child.is_subagent)
            self.assertEqual(child.parent_id, parent_id)
            self.assertEqual(child.parent_title, "SubVault Review")
            self.assertEqual(child.workspace, "C:/Users/test/agent-home")
            self.assertEqual(child.title, "You are executing Wave 2 · Track A for SubVault.")

            # Inherited workspace works with --workspace filter
            rep_ws = mod.scan_usage(root, tz=tz_utc, workspace_filter="agent-home", pricing_path=None)
            self.assertEqual(len(rep_ws.sessions), 2)

            agg = mod.aggregate_sessions(rep.sessions)
            self.assertNotIn("(unknown)", agg["by_workspace"])

            # Default text view groups subagent under parent; --show-subagents lists full 30-char model unclipped
            txt_grouped = mod.render_text(
                rep, agg, view="summary", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                color=False, show_subagents=False,
            )
            self.assertIn("1 (+1 subagents)", txt_grouped)
            self.assertIn("(+1 sub)", txt_grouped)
            self.assertIn("gemini-4-hydrogen-tiered-xhigh", txt_grouped)

            txt_flat = mod.render_text(
                rep, agg, view="session", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                color=False, show_subagents=True,
            )
            self.assertIn("gemini-4-hydrogen-tiered-xhigh", txt_flat)
            self.assertIn("↳ You are executing Wave 2", txt_flat)

            js = json.loads(mod.render_json(rep, agg, sort_by="time", tz_label="UTC", include_turns=False))
            js_child = next(s for s in js["sessions"] if s["conversation_id"] == child_id)
            self.assertTrue(js_child["is_subagent"])
            self.assertEqual(js_child["parent_id"], parent_id)
            self.assertEqual(js_child["parent_title"], "SubVault Review")

    def test_swarm_worker_board_ping_upgrades_to_assignment_without_meta(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            parent_id = "ab159572-c4b8-4bd9-86e4-6b4bb34a5129"
            worker_sys_id = "a6330233-a6c5-4e24-82dc-24e1795519fe"
            worker_peer_id = "00b0f7b6-2eb8-40c6-ab17-8382ceef722e"
            other_worker_id = "def33a67-8eec-4265-be5b-85ee403d56d4"
            worker_early_id = "11e43752-b1f0-4710-9427-1c78a6b3b92b"

            for cid in (parent_id, worker_sys_id, worker_peer_id, worker_early_id):
                (root / "brain" / cid / ".system_generated" / "logs").mkdir(parents=True)
            cache_dir = root / "cache"
            cache_dir.mkdir(parents=True)
            # Workers intentionally omitted from conversation_metadata.json (live race window).
            (cache_dir / "conversation_metadata.json").write_text(
                json.dumps({
                    "conversations": {
                        parent_id: {
                            "summary": {
                                "Title": "Swarm Coordinator",
                                "WorkspaceURIs": ["file:///C:/Users/test/agent-home"],
                            }
                        }
                    }
                }),
                encoding="utf-8",
            )

            (root / "brain" / parent_id / ".system_generated" / "logs" / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "USER_INPUT",
                        "source": "USER_EXPLICIT",
                        "status": "DONE",
                        "content": "<USER_REQUEST>\n/swarm audit repos\n</USER_REQUEST>",
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-25T16:38:00Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 1000,
                        "cache_read_tokens": 0,
                        "output_tokens": 120,
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            # Worker 1: step=0 sender=system ping, step=1 coordinator assignment with @verifier
            (root / "brain" / worker_sys_id / ".system_generated" / "logs" / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            "[Message] timestamp=2026-11-25T16:38:11Z sender=system "
                            "priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #5\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            f"[Message] timestamp=2026-11-25T16:38:11Z sender={parent_id} "
                            "priority=MESSAGE_PRIORITY_HIGH content=You have just started. "
                            "Your assignment (recorded on the board as #5 for @verifier):\n\n"
                            "You are the **independent** `verifier` for all repos.\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                    json.dumps({
                        "step_index": 2,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-25T16:38:16Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 1500,
                        "cache_read_tokens": 2000,
                        "output_tokens": 200,
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            # Worker 2: step=0 sender=<other-worker> ping, step=1 upgrades parent_id to root coordinator
            (root / "brain" / worker_peer_id / ".system_generated" / "logs" / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            f"[Message] timestamp=2026-11-23T18:40:58Z sender={other_worker_id} "
                            "priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #2\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            f"[Message] timestamp=2026-11-23T18:40:58Z sender={parent_id} "
                            "priority=MESSAGE_PRIORITY_HIGH content=You have just started. "
                            "Your assignment (recorded on the board as #2 for @stack-bench):\n\n"
                            "# Benchmark Engineer\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                    json.dumps({
                        "step_index": 2,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-23T18:41:05Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 800,
                        "cache_read_tokens": 1200,
                        "output_tokens": 100,
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            # Worker 3: only step=0 board ping before turn -> subagent, never "(untitled)"
            (root / "brain" / worker_early_id / ".system_generated" / "logs" / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            "[Message] timestamp=2026-11-25T16:38:12Z sender=system "
                            "priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #6\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-25T16:38:19Z",
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 500,
                        "cache_read_tokens": 0,
                        "output_tokens": 60,
                    }),
                    json.dumps({
                        "step_index": 2,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": (
                            "<SYSTEM_MESSAGE>\n"
                            "[Message] timestamp=2026-11-25T16:41:00Z sender=00000000-0000-0000-0000-000000000000 "
                            "priority=MESSAGE_PRIORITY_HIGH content=[monitor] tick\n"
                            "</SYSTEM_MESSAGE>"
                        ),
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            rep = mod.scan_usage(root, tz=tz_utc, pricing_path=None)
            self.assertEqual(len(rep.sessions), 4)

            s1 = next(s for s in rep.sessions if s.conversation_id == worker_sys_id)
            self.assertTrue(s1.is_subagent)
            self.assertEqual(s1.parent_id, parent_id)
            self.assertEqual(s1.parent_title, "Swarm Coordinator")
            self.assertEqual(s1.workspace, "C:/Users/test/agent-home")
            self.assertEqual(s1.title, "@verifier · You are the independent verifier for all repos.")

            s2 = next(s for s in rep.sessions if s.conversation_id == worker_peer_id)
            self.assertTrue(s2.is_subagent)
            self.assertEqual(s2.parent_id, parent_id)
            self.assertEqual(s2.workspace, "C:/Users/test/agent-home")
            self.assertEqual(s2.title, "@stack-bench · Benchmark Engineer")

            s3 = next(s for s in rep.sessions if s.conversation_id == worker_early_id)
            self.assertTrue(s3.is_subagent)
            self.assertIsNone(s3.parent_id)
            self.assertEqual(s3.title, "[swarm] new board message #6")
            self.assertNotEqual(s3.title, "(untitled)")

    def test_redaction_across_text_json_csv_and_pricing(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp) / "Users" / "alice" / ".gemini" / "antigravity-cli"
            parent_id = "8c1095e2-1558-4a06-bfde-446204f978aa"
            child_id = "14ee4713-eb4d-4a3c-b6a9-6745e2e553e2"
            p_logs = root / "brain" / parent_id / ".system_generated" / "logs"
            c_logs = root / "brain" / child_id / ".system_generated" / "logs"
            cache_dir = root / "cache"
            p_logs.mkdir(parents=True)
            c_logs.mkdir(parents=True)
            cache_dir.mkdir(parents=True)

            (cache_dir / "conversation_metadata.json").write_text(
                json.dumps({
                    "conversations": {
                        parent_id: {
                            "summary": {
                                "Title": "Confidential Payroll Refactor",
                                "WorkspaceURIs": ["file:///C:/Users/alice/secret-payroll"],
                            }
                        },
                        child_id: {"is_internal": True},
                    }
                }),
                encoding="utf-8",
            )
            (p_logs / "transcript.jsonl").write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": "2026-11-22T14:48:00Z",
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 2000,
                    "cache_read_tokens": 8000,
                    "output_tokens": 500,
                }) + "\n",
                encoding="utf-8",
            )
            (c_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0,
                        "type": "SYSTEM_MESSAGE",
                        "source": "SYSTEM",
                        "status": "DONE",
                        "content": f"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-22T14:48:47Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=Inspect alice private key table.\n</SYSTEM_MESSAGE>",
                    }),
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": "2026-11-22T14:49:00Z",
                        "model": "gemini-4-hydrogen-tiered-xhigh",
                        "input_tokens": 1000,
                        "cache_read_tokens": 4000,
                        "output_tokens": 200,
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            rep = mod.scan_usage(root, tz=tz_utc, pricing_path=None)
            agg = mod.aggregate_sessions(rep.sessions)

            secrets = (
                "alice",
                "secret-payroll",
                "Confidential Payroll",
                "private key table",
                "8c1095e2",
                "14ee4713",
            )

            # 1. Full text table (grouped + flat subagents + single-session detail)
            txt_all = mod.render_text(
                rep, agg, view="all", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                color=False, show_subagents=True, redact=True,
            )
            for s_val in secrets:
                self.assertNotIn(s_val, txt_all)
            self.assertIn("~/.gemini/antigravity-cli", txt_all)
            self.assertIn("Project A", txt_all)
            self.assertIn("S-001", txt_all)
            self.assertIn("Session #1", txt_all)
            self.assertIn("Subagent #1.1", txt_all)
            self.assertIn("gemini-4-neon-xhigh", txt_all)
            self.assertIn("gemini-4-hydrogen-tiered-xhigh", txt_all)

            rep_child = mod.scan_usage(root, tz=tz_utc, session_filter="14ee4713", pricing_path=None)
            txt_detail = mod.render_text(
                rep_child, mod.aggregate_sessions(rep_child.sessions), view="summary", sort_by="time",
                limit=10, human=True, tz_label="UTC", since=None, until=None, model_filter=None,
                session_filter="14ee4713", color=False, redact=True,
            )
            for s_val in secrets:
                self.assertNotIn(s_val, txt_detail)
            self.assertIn("Session=<redacted>", txt_detail)
            self.assertIn("Project A", txt_detail)

            # 2. JSON export preserves structure, models, tokens, costs with zero leaked PII
            js_raw = mod.render_json(rep, agg, sort_by="time", tz_label="UTC", include_turns=True, redact=True)
            for s_val in secrets:
                self.assertNotIn(s_val, js_raw)
            js = json.loads(js_raw)
            self.assertTrue(js["redacted"])
            self.assertEqual(js["data_dir"], "~/.gemini/antigravity-cli")
            self.assertIn("Project A", js["by_workspace"])
            self.assertEqual(js["overview"]["total_tokens"], 15700)
            js_sub = next(s for s in js["sessions"] if s["is_subagent"])
            self.assertEqual(js_sub["conversation_id"], "S-001.1")
            self.assertEqual(js_sub["parent_id"], "S-001")
            self.assertEqual(js_sub["parent_title"], "Session #1")

            # 3. CSV exports (session + workspace)
            csv_sess = mod.render_csv(rep, agg, view="session", sort_by="time", redact=True)
            csv_ws = mod.render_csv(rep, agg, view="workspace", sort_by="time", redact=True)
            for s_val in secrets:
                self.assertNotIn(s_val, csv_sess)
                self.assertNotIn(s_val, csv_ws)
            self.assertIn("S-001", csv_sess)
            self.assertIn("Project A", csv_ws)

            # 4. Pricing table redacts local pricing.json directory path
            custom_pricing = pathlib.Path(tmp) / "Users" / "alice" / "agy-token-stats" / "pricing.json"
            pricing_txt = mod.render_pricing_table(
                mod.PricingCatalog(custom_pricing), ["gemini-4-neon-xhigh"], dt.date(2026, 11, 22), False, redact=True
            )
            self.assertNotIn("alice", pricing_txt)
            self.assertIn("./pricing.json", pricing_txt)

            # 5. Custom non-standard data_dir does not leak folder name
            self.assertEqual(mod._redact_data_dir(pathlib.Path(r"D:\PrivateClient\agy-data")), "<redacted>")

    def test_tools_multi_source_statusline_archive_and_budget(self) -> None:
        tz_utc = dt.timezone.utc
        now = dt.datetime.now(tz_utc).replace(microsecond=0)
        with tempfile.TemporaryDirectory() as tmp:
            cli_dir = pathlib.Path(tmp) / "antigravity-cli"
            app_dir = pathlib.Path(tmp) / "antigravity"
            s1_logs = cli_dir / "brain" / "aaa00001-0000-0000-0000-000000000001" / ".system_generated" / "logs"
            s2_logs = app_dir / "brain" / "bbb00002-0000-0000-0000-000000000002" / ".system_generated" / "logs"
            s1_logs.mkdir(parents=True)
            s2_logs.mkdir(parents=True)

            t1 = (now - dt.timedelta(minutes=40)).isoformat()
            t2 = (now - dt.timedelta(minutes=10)).isoformat()
            (s1_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 1,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": t1,
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 500_000,
                        "cache_read_tokens": 1_000_000,
                        "output_tokens": 10_000,
                        "thinking": "analyze architecture",
                        "tool_calls": [{"name": "read_file", "args": {}}, {"name": "edit_file", "args": {}}],
                    }),
                    json.dumps({
                        "step_index": 2,
                        "type": "PLANNER_RESPONSE",
                        "status": "DONE",
                        "created_at": t2,
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 200_000,
                        "cache_read_tokens": 1_500_000,
                        "output_tokens": 5_000,
                        "tool_calls": [{"name": "read_file", "args": {}}],
                    }),
                ]) + "\n",
                encoding="utf-8",
            )
            (s2_logs / "transcript.jsonl").write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": (now - dt.timedelta(hours=8)).isoformat(),
                    "model": "claude-opus-5-5",
                    "input_tokens": 100_000,
                    "cache_read_tokens": 0,
                    "output_tokens": 2_000,
                }) + "\n",
                encoding="utf-8",
            )

            # Local archive backfill for a deleted session
            arch_path = pathlib.Path(tmp) / "archive.json"
            arch_path.write_text(
                json.dumps({
                    "schema_version": 1,
                    "sessions": {
                        "ccc00003-0000-0000-0000-000000000003": {
                            "title": "Pruned Session",
                            "workspace": "C:/old",
                            "source": "ide",
                            "turns": [{
                                "step": 1,
                                "ts": (now - dt.timedelta(days=2)).isoformat(),
                                "m": "gemini-4-hydrogen",
                                "in": 50_000,
                                "cr": 100_000,
                                "out": 1_000,
                                "tools": ["run_command"],
                                "th": 40,
                            }],
                        }
                    },
                }),
                encoding="utf-8",
            )

            # Official statusline state snapshot
            sl_path = pathlib.Path(tmp) / "statusline_state.json"
            sl_path.write_text(
                json.dumps({
                    "schema_version": 1,
                    "captured_at": now.isoformat(),
                    "payload": {
                        "plan_tier": "Pro",
                        "model": {"display_name": "Gemini 4 Neon"},
                        "quota": {
                            "gemini-weekly": {
                                "remaining_fraction": 0.91,
                                "reset_time": (now + dt.timedelta(days=3)).isoformat(),
                            }
                        },
                    },
                }),
                encoding="utf-8",
            )

            rep = mod.scan_usage(
                cli_dir,
                tz=tz_utc,
                pricing_path=None,
                extra_dirs=[app_dir],
                archive_path=arch_path,
            )
            self.assertEqual(len(rep.sessions), 3)
            self.assertEqual(rep.archived_sessions_count, 1)
            agg = mod.aggregate_sessions(rep.sessions)
            self.assertEqual(agg["by_tool"]["read_file"], 2)
            self.assertEqual(agg["by_tool"]["run_command"], 1)
            self.assertEqual(agg["total_thinking_turns"], 2)
            self.assertIn("cli", agg["by_source"])
            self.assertIn("app", agg["by_source"])
            self.assertIn("ide", agg["by_source"])

            sl = mod.load_statusline_state(sl_path, now_utc=now)
            self.assertIsNotNone(sl)
            assert sl is not None
            self.assertEqual(sl["provenance"], "official")
            self.assertAlmostEqual(sl["buckets"][0]["remaining_pct"], 91.0, places=1)

            txt = mod.render_text(
                rep, agg, view="all", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                color=False, statusline=sl,
            )
            self.assertIn("Official Quota", txt)
            self.assertIn("By Tool Calls", txt)
            self.assertNotIn("Active 5h Block", txt)
            self.assertNotIn("5-Hour Rolling Blocks", txt)

            compact = mod.render_compact(rep, agg, tz=tz_utc, statusline=sl)
            self.assertIn("AGY today=", compact)
            self.assertNotIn("5h=", compact)
            self.assertIn("quota=gemini-weekly:91%", compact)

            js = json.loads(
                mod.render_json(
                    rep, agg, sort_by="time", tz_label="UTC", include_turns=False, statusline=sl
                )
            )
            self.assertNotIn("blocks_5h", js)
            self.assertIsNotNone(js["official_quota"])

            # Legacy budget.json with block_5h_usd is ignored safely
            legacy_budget = pathlib.Path(tmp) / "budget.json"
            legacy_budget.write_text(
                json.dumps({"block_5h_usd": 15.0, "daily_usd": 25.0, "weekly_usd": 100.0}),
                encoding="utf-8",
            )
            bcfg = mod.load_budget_config(legacy_budget)
            self.assertNotIn("block_5h_usd", bcfg)
            self.assertAlmostEqual(bcfg["daily_usd"], 25.0)
            self.assertAlmostEqual(bcfg["weekly_usd"], 100.0)

            # Budget check exit codes: warn (10) & critical (20) on daily budget
            code_crit, notes_crit = mod.evaluate_budget_exit_code(
                agg, tz=tz_utc, daily_usd=1.0, warn_pct=70.0, crit_pct=90.0
            )
            self.assertEqual(code_crit, 20)
            self.assertTrue(any("CRITICAL" in n for n in notes_crit))

    def test_claude_code_parser_dedup_synthetic_ttl_and_subagents(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            claude_root = pathlib.Path(tmp) / "Users" / "alice" / ".claude"
            proj_dir = claude_root / "projects" / "C--Users-alice-secret-app"
            parent_uuid = "11111111-1111-4111-8111-111111111111"
            sub_dir = proj_dir / parent_uuid / "subagents"
            sub_dir.mkdir(parents=True)

            # Decoy credential / settings / history files that must never be touched (both root and under projects/<proj>/)
            (claude_root / ".credentials.json").write_text("{broken json", encoding="utf-8")
            (claude_root / "settings.json").write_text("{broken json", encoding="utf-8")
            (claude_root / "history.jsonl").write_text("{broken json\n", encoding="utf-8")
            trap_line = json.dumps({
                "type": "assistant",
                "uuid": "trap-1",
                "sessionId": "history",
                "requestId": "req_trap",
                "timestamp": "2026-11-23T10:00:00Z",
                "message": {
                    "id": "msg_trap",
                    "model": "claude-sonnet-4-5-20250929",
                    "usage": {"input_tokens": 66_000_000, "output_tokens": 666_667},
                },
            }) + "\n"
            for decoy_name in ("history.jsonl", "settings.jsonl", "auth-tokens.jsonl", "my-credentials.jsonl", "debug-trace.jsonl", "agent-orphan.jsonl"):
                (proj_dir / decoy_name).write_text(trap_line, encoding="utf-8")

            (proj_dir / "sessions-index.json").write_text(
                json.dumps({
                    "version": 1,
                    "entries": [{
                        "sessionId": parent_uuid,
                        "summary": "Indexed Parent Title",
                        "projectPath": "C:/Users/alice/secret-app",
                    }],
                }),
                encoding="utf-8",
            )

            # Main session: streaming duplicates sharing (message.id, requestId),
            # synthetic error turn, isApiErrorMessage turn, ephemeral 1h cache write, custom-title
            main_lines = [
                json.dumps({
                    "type": "user",
                    "uuid": "u-1",
                    "sessionId": parent_uuid,
                    "cwd": "C:/Users/alice/secret-app",
                    "timestamp": "2026-11-23T10:00:00Z",
                    "message": {"role": "user", "content": "Refactor billing module"},
                }),
                # Streaming chunk 0: usage-less chunk sharing (msg_001, req_001) carries extra tool_use + thinking
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-1z",
                    "sessionId": parent_uuid,
                    "requestId": "req_001",
                    "timestamp": "2026-11-23T10:00:01Z",
                    "message": {
                        "id": "msg_001",
                        "model": "claude-sonnet-4-5-20250929",
                        "stop_reason": None,
                        "content": [
                            {"type": "thinking", "thinking": "Initial thought before usage arrives"},
                            {"type": "tool_use", "id": "toolu_0", "name": "Grep"},
                        ],
                    },
                }),
                # Streaming chunk 1: thinking + partial output_tokens=4 + stop_reason=null
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-1a",
                    "sessionId": parent_uuid,
                    "requestId": "req_001",
                    "timestamp": "2026-11-23T10:00:02Z",
                    "message": {
                        "id": "msg_001",
                        "model": "claude-sonnet-4-5-20250929",
                        "stop_reason": None,
                        "content": [{"type": "thinking", "thinking": "Plan changes carefully"}],
                        "usage": {
                            "input_tokens": 1000,
                            "cache_read_input_tokens": 2000,
                            "cache_creation_input_tokens": 1000,
                            "output_tokens": 4,
                            "cache_creation": {
                                "ephemeral_5m_input_tokens": 600,
                                "ephemeral_1h_input_tokens": 400,
                            },
                        },
                    },
                }),
                # Streaming chunk 2: same (msg_001, req_001), tool_use Read + partial output=25
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-1b",
                    "sessionId": parent_uuid,
                    "requestId": "req_001",
                    "timestamp": "2026-11-23T10:00:03Z",
                    "message": {
                        "id": "msg_001",
                        "model": "claude-sonnet-4-5-20250929",
                        "stop_reason": None,
                        "content": [{"type": "tool_use", "id": "toolu_1", "name": "Read"}],
                        "usage": {
                            "input_tokens": 1000,
                            "cache_read_input_tokens": 2000,
                            "cache_creation_input_tokens": 1000,
                            "output_tokens": 25,
                            "cache_creation": {
                                "ephemeral_5m_input_tokens": 600,
                                "ephemeral_1h_input_tokens": 400,
                            },
                        },
                    },
                }),
                # Streaming chunk 3: final chunk, repeats toolu_1 + adds toolu_2 Edit, output=200
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-1c",
                    "sessionId": parent_uuid,
                    "requestId": "req_001",
                    "timestamp": "2026-11-23T10:00:05Z",
                    "message": {
                        "id": "msg_001",
                        "model": "claude-sonnet-4-5-20250929",
                        "stop_reason": "tool_use",
                        "content": [
                            {"type": "tool_use", "id": "toolu_1", "name": "Read"},
                            {"type": "tool_use", "id": "toolu_2", "name": "Edit"},
                        ],
                        "usage": {
                            "input_tokens": 1000,
                            "cache_read_input_tokens": 2000,
                            "cache_creation_input_tokens": 1000,
                            "output_tokens": 200,
                            "cache_creation": {
                                "ephemeral_5m_input_tokens": 600,
                                "ephemeral_1h_input_tokens": 400,
                            },
                        },
                    },
                }),
                # Synthetic error turn must be skipped
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-err-1",
                    "sessionId": parent_uuid,
                    "requestId": "req_err1",
                    "isApiErrorMessage": False,
                    "timestamp": "2026-11-23T10:00:06Z",
                    "message": {
                        "id": "msg_syn",
                        "model": "<synthetic>",
                        "content": [{"type": "text", "text": "No response requested."}],
                        "usage": {
                            "input_tokens": 999,
                            "output_tokens": 999,
                            "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
                        },
                    },
                }),
                # isApiErrorMessage=true turn must be skipped even with non-synthetic model
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-err-2",
                    "sessionId": parent_uuid,
                    "requestId": "req_err2",
                    "isApiErrorMessage": True,
                    "timestamp": "2026-11-23T10:00:07Z",
                    "message": {
                        "id": "msg_apierr",
                        "model": "claude-sonnet-4-5-20250929",
                        "usage": {"input_tokens": 500, "output_tokens": 50},
                    },
                }),
                # isApiErrorMessage=true inside message object must also be skipped
                json.dumps({
                    "type": "assistant",
                    "uuid": "a-err-3",
                    "sessionId": parent_uuid,
                    "requestId": "req_err3",
                    "timestamp": "2026-11-23T10:00:07Z",
                    "message": {
                        "id": "msg_apierr2",
                        "model": "claude-sonnet-4-5-20250929",
                        "isApiErrorMessage": True,
                        "usage": {"input_tokens": 777, "output_tokens": 77},
                    },
                }),
                # User toolUseResult with nested subagent usage must NOT be counted on parent
                json.dumps({
                    "type": "user",
                    "uuid": "u-2",
                    "sessionId": parent_uuid,
                    "timestamp": "2026-11-23T10:00:08Z",
                    "toolUseResult": {"usage": {"input_tokens": 99999, "output_tokens": 88888}},
                    "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]},
                }),
                json.dumps({
                    "type": "custom-title",
                    "sessionId": parent_uuid,
                    "customTitle": "Billing Refactor Custom Title",
                }),
            ]
            (proj_dir / f"{parent_uuid}.jsonl").write_text("\n".join(main_lines) + "\n", encoding="utf-8")

            # Subagent file + meta.json
            (sub_dir / "agent-a1b2c3.meta.json").write_text(
                json.dumps({"agentType": "Explore", "description": "Scan billing handlers"}),
                encoding="utf-8",
            )
            (sub_dir / "agent-a1b2c3.jsonl").write_text(
                json.dumps({
                    "type": "assistant",
                    "uuid": "sub-1",
                    "sessionId": parent_uuid,
                    "isSidechain": True,
                    "agentId": "a1b2c3",
                    "requestId": "req_sub1",
                    "timestamp": "2026-11-23T10:00:04Z",
                    "message": {
                        "id": "msg_sub1",
                        "model": "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                        "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "Found 3 handlers"}],
                        "usage": {
                            "input_tokens": 500,
                            "cache_read_input_tokens": 1500,
                            "cache_creation_input_tokens": 0,
                            "output_tokens": 100,
                        },
                    },
                }) + "\n",
                encoding="utf-8",
            )

            spec = mod.SourceSpec("claude", claude_root, "claude")
            rep = mod.scan_usage(claude_root, tz=tz_utc, pricing_path=None, sources=[spec])
            self.assertEqual(len(rep.sessions), 2)

            main_s = next(s for s in rep.sessions if not s.is_subagent)
            sub_s = next(s for s in rep.sessions if s.is_subagent)

            self.assertEqual(main_s.conversation_id, parent_uuid)
            self.assertEqual(main_s.source, "claude")
            self.assertEqual(main_s.agent_family, "claude")
            self.assertEqual(main_s.title, "Billing Refactor Custom Title")
            self.assertEqual(main_s.workspace, "C:/Users/alice/secret-app")
            self.assertEqual(main_s.counts.calls, 1)
            self.assertEqual(main_s.counts.input_tokens, 1000)
            self.assertEqual(main_s.counts.cache_read_tokens, 2000)
            # cache_creation_input_tokens is 1000 (NOT 1000 + 600 + 400!)
            self.assertEqual(main_s.counts.cache_write_tokens, 1000)
            self.assertEqual(main_s.counts.output_tokens, 200)
            self.assertEqual(main_s.counts.total_tokens, 4200)
            self.assertEqual(main_s.thinking_turns, 1)
            self.assertEqual(main_s.turns[0].cache_write_1h_tokens, 400)
            self.assertEqual(main_s.turns[0].tool_names, ("Grep", "Read", "Edit"))

            # Verify claude-sonnet-4 pricing: in=$3/M, cr=$0.30/M, cw_5m=$3.75/M, cw_1h=$6.00/M (2x), out=$15/M
            # Expected: in=0.003 + cr=0.0006 + cw=(600*3.75 + 400*6.00)/1M=0.00465 + out=0.003 = 0.01125
            self.assertAlmostEqual(main_s.counts.total_cost_usd, 0.01125, places=8)

            # Subagent linkage and Bedrock prefix pricing
            self.assertEqual(sub_s.conversation_id, f"{parent_uuid}__a1b2c3")
            self.assertEqual(sub_s.parent_id, parent_uuid)
            self.assertEqual(sub_s.parent_title, "Billing Refactor Custom Title")
            self.assertEqual(sub_s.workspace, "C:/Users/alice/secret-app")
            self.assertEqual(sub_s.title, "[Explore] Scan billing handlers")
            # claude-haiku-4-5: in=$1/M, cr=$0.10/M, out=$5/M -> (500*1 + 1500*0.1 + 100*5)/1M = 0.00115
            self.assertAlmostEqual(sub_s.counts.total_cost_usd, 0.00115, places=8)

    def test_codex_cli_parser_deltas_dupes_reset_tools_and_subagents(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            codex_root = pathlib.Path(tmp) / "Users" / "alice" / ".codex"
            day_dir = codex_root / "sessions" / "2026" / "11" / "23"
            arch_dir = codex_root / "archived_sessions"
            day_dir.mkdir(parents=True)
            arch_dir.mkdir(parents=True)

            # Decoy SQLite & credential files that must never be touched
            (codex_root / "state_5.sqlite").write_bytes(b"SQLite format 3\x00")
            (codex_root / "state_5.sqlite-wal").write_bytes(b"wal")
            (codex_root / "auth.json").write_text('{"api_key": "sk-secret"}', encoding="utf-8")
            (codex_root / "config.toml").write_text("model = 'gpt-5.3-codex'\n", encoding="utf-8")

            parent_id = "22222222-2222-4222-8222-222222222222"
            child_id = "33333333-3333-4333-8333-333333333333"
            (codex_root / "session_index.jsonl").write_text(
                json.dumps({"id": parent_id, "thread_name": "Codex Main Refactor"}) + "\n",
                encoding="utf-8",
            )

            main_lines = [
                json.dumps({
                    "timestamp": "2026-11-23T11:00:00Z",
                    "type": "session_meta",
                    "payload": {"id": parent_id, "cwd": "C:/Users/alice/codex-repo", "source": "cli"},
                }),
                json.dumps({
                    "timestamp": "2026-11-23T11:00:01Z",
                    "type": "turn_context",
                    "payload": {"cwd": "C:/Users/alice/codex-repo", "model": "gpt-5.3-codex"},
                }),
                # Rate-limit-only event with info: null must be skipped
                json.dumps({
                    "timestamp": "2026-11-23T11:00:02Z",
                    "type": "event_msg",
                    "payload": {"type": "token_count", "info": None},
                }),
                json.dumps({
                    "timestamp": "2026-11-23T11:00:03Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "name": "shell_command"},
                }),
                # Turn 1: input=1000 (includes cached=400), output=300 (includes reasoning=120)
                json.dumps({
                    "timestamp": "2026-11-23T11:00:04Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 400,
                                "output_tokens": 300,
                                "reasoning_output_tokens": 120,
                                "total_tokens": 1300,
                            },
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 400,
                                "output_tokens": 300,
                                "reasoning_output_tokens": 120,
                                "total_tokens": 1300,
                            },
                        },
                    },
                }),
                # Exact duplicate token_count refresh must be skipped!
                json.dumps({
                    "timestamp": "2026-11-23T11:00:05Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 400,
                                "output_tokens": 300,
                                "reasoning_output_tokens": 120,
                                "total_tokens": 1300,
                            },
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 400,
                                "output_tokens": 300,
                                "reasoning_output_tokens": 120,
                                "total_tokens": 1300,
                            },
                        },
                    },
                }),
                # Switch model mid-session via turn_context
                json.dumps({
                    "timestamp": "2026-11-23T11:01:00Z",
                    "type": "turn_context",
                    "payload": {"model": "gpt-5.4"},
                }),
                json.dumps({
                    "timestamp": "2026-11-23T11:01:02Z",
                    "type": "response_item",
                    "payload": {"type": "custom_tool_call", "name": "apply_patch"},
                }),
                # Turn 2: cumulative total advances monotonically -> delta in=1500, cached=1200, out=500, reason=200
                json.dumps({
                    "timestamp": "2026-11-23T11:01:05Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 2500,
                                "cached_input_tokens": 1600,
                                "output_tokens": 800,
                                "reasoning_output_tokens": 320,
                                "total_tokens": 3300,
                            },
                            "last_token_usage": {
                                "input_tokens": 1500,
                                "cached_input_tokens": 1200,
                                "output_tokens": 500,
                                "reasoning_output_tokens": 200,
                                "total_tokens": 2000,
                            },
                        },
                    },
                }),
                # Turn 3: simulated reset (total smaller than prev_total) -> falls back to last_token_usage
                json.dumps({
                    "timestamp": "2026-11-23T11:02:00Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 200,
                                "cached_input_tokens": 50,
                                "output_tokens": 40,
                                "reasoning_output_tokens": 0,
                                "total_tokens": 240,
                            },
                            "last_token_usage": {
                                "input_tokens": 200,
                                "cached_input_tokens": 50,
                                "output_tokens": 40,
                                "reasoning_output_tokens": 0,
                                "total_tokens": 240,
                            },
                        },
                    },
                }),
            ]
            (day_dir / f"rollout-2026-11-23T11-00-00-{parent_id}.jsonl").write_text(
                "\n".join(main_lines) + "\n", encoding="utf-8"
            )

            # Subagent rollout in archived_sessions linked via thread_spawn.parent_thread_id
            sub_lines = [
                json.dumps({
                    "timestamp": "2026-11-23T11:00:30Z",
                    "type": "session_meta",
                    "payload": {
                        "id": child_id,
                        "source": {
                            "subagent": {
                                "thread_spawn": {
                                    "parent_thread_id": parent_id,
                                    "depth": 1,
                                    "agent_nickname": "auditor",
                                    "agent_role": "explorer",
                                }
                            }
                        },
                    },
                }),
                json.dumps({
                    "timestamp": "2026-11-23T11:00:31Z",
                    "type": "turn_context",
                    "payload": {"model": "gpt-5.1-codex-mini"},
                }),
                json.dumps({
                    "timestamp": "2026-11-23T11:00:35Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 500,
                                "cached_input_tokens": 100,
                                "output_tokens": 100,
                                "reasoning_output_tokens": 30,
                            }
                        },
                    },
                }),
            ]
            (arch_dir / f"rollout-2026-11-23T11-00-30-{child_id}.jsonl").write_text(
                "\n".join(sub_lines) + "\n", encoding="utf-8"
            )

            spec = mod.SourceSpec("codex", codex_root, "codex")
            rep = mod.scan_usage(codex_root, tz=tz_utc, pricing_path=None, sources=[spec])
            self.assertEqual(len(rep.sessions), 2)

            main_s = next(s for s in rep.sessions if s.conversation_id == parent_id)
            sub_s = next(s for s in rep.sessions if s.conversation_id == child_id)

            self.assertEqual(main_s.source, "codex")
            self.assertEqual(main_s.title, "Codex Main Refactor")
            self.assertEqual(main_s.counts.calls, 3)
            # Turn 1: uncached=600, cached=400, out=300, reason=120
            # Turn 2: uncached=300, cached=1200, out=500, reason=200
            # Turn 3: uncached=150, cached=50, out=40, reason=0
            self.assertEqual(main_s.counts.input_tokens, 1050)
            self.assertEqual(main_s.counts.cache_read_tokens, 1650)
            self.assertEqual(main_s.counts.cache_write_tokens, 0)
            self.assertEqual(main_s.counts.output_tokens, 840)
            self.assertEqual(main_s.counts.total_tokens, 3540)
            self.assertEqual(main_s.turns[0].model, "gpt-5.3-codex")
            self.assertEqual(main_s.turns[0].tool_names, ("shell_command",))
            self.assertEqual(main_s.turns[0].reasoning_tokens, 120)
            self.assertEqual(main_s.turns[1].model, "gpt-5.4")
            self.assertEqual(main_s.turns[1].tool_names, ("apply_patch",))
            self.assertEqual(main_s.turns[1].reasoning_tokens, 200)
            self.assertEqual(main_s.thinking_turns, 2)

            # Subagent inherits parent title & workspace
            self.assertTrue(sub_s.is_subagent)
            self.assertEqual(sub_s.parent_id, parent_id)
            self.assertEqual(sub_s.parent_title, "Codex Main Refactor")
            self.assertEqual(sub_s.workspace, "C:/Users/alice/codex-repo")
            self.assertEqual(sub_s.title, "auditor · explorer")
            self.assertEqual(sub_s.counts.input_tokens, 400)
            self.assertEqual(sub_s.counts.cache_read_tokens, 100)
            self.assertEqual(sub_s.counts.output_tokens, 100)
            self.assertEqual(sub_s.thinking_turns, 1)

    def test_gemini_cli_parser_thoughts_tool_tokens_and_jsonl_rewind(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            gem_home = pathlib.Path(tmp) / "Users" / "alice" / ".gemini"
            tmp_dir = gem_home / "tmp"
            proj_a_chats = tmp_dir / "proj-alpha" / "chats"
            proj_b_chats = tmp_dir / "proj-beta" / "chats"
            proj_a_chats.mkdir(parents=True)
            proj_b_chats.mkdir(parents=True)

            # Decoy OAuth/settings files that must never be touched
            (gem_home / "oauth_creds.json").write_text("{broken", encoding="utf-8")
            (gem_home / "google_accounts.json").write_text("{broken", encoding="utf-8")
            (gem_home / "jetski-standalone-oauth-token").write_text("secret-token", encoding="utf-8")
            (gem_home / "settings.json").write_text("{broken", encoding="utf-8")

            # Workspace via .project_root for proj-alpha, projects.json for proj-beta
            (tmp_dir / "proj-alpha" / ".project_root").write_text("C:/Users/alice/alpha-repo\n", encoding="utf-8")
            (gem_home / "projects.json").write_text(
                json.dumps({"projects": {"C:/Users/alice/beta-repo": "proj-beta"}}),
                encoding="utf-8",
            )

            # Session 1: classic .json with thoughts + tool tokens + duplicate message.id
            (proj_a_chats / "session-2026-11-23-s1.json").write_text(
                json.dumps({
                    "sessionId": "gem-sess-001",
                    "summary": "Gemini Reasoning Session",
                    "messages": [
                        {"id": "u1", "type": "user", "content": "Explain cache math"},
                        {
                            "id": "g1",
                            "type": "gemini",
                            "timestamp": "2026-11-23T12:00:00Z",
                            "model": "gemini-2.5-pro",
                            "tokens": {"input": 1000, "cached": 600, "tool": 100, "output": 50, "thoughts": 10},
                        },
                        # Duplicate id "g1" should override earlier partial record
                        {
                            "id": "g1",
                            "type": "gemini",
                            "timestamp": "2026-11-23T12:00:05Z",
                            "model": "gemini-2.5-pro",
                            "thoughts": [{"subject": "Plan", "description": "Check formula"}],
                            "toolCalls": [{"id": "tc1", "name": "read_file", "status": "ok"}],
                            "tokens": {"input": 1000, "cached": 600, "tool": 150, "output": 200, "thoughts": 800},
                        },
                    ],
                }),
                encoding="utf-8",
            )

            # Session 2: append-only .jsonl with $set and $rewindTo rolling back turn 2
            jsonl_lines = [
                json.dumps({"sessionId": "gem-sess-002", "projectHash": "proj-beta"}),
                json.dumps({"id": "m1", "type": "user", "content": "Initial question"}),
                json.dumps({
                    "id": "m2",
                    "type": "gemini",
                    "timestamp": "2026-11-23T13:00:00Z",
                    "model": "gemini-2.0-flash",
                    "tokens": {"input": 500, "cached": 200, "tool": 0, "output": 100, "thoughts": 50},
                }),
                json.dumps({"id": "m3", "type": "user", "content": "Wrong branch prompt"}),
                json.dumps({
                    "id": "m4",
                    "type": "gemini",
                    "timestamp": "2026-11-23T13:01:00Z",
                    "model": "gemini-2.0-flash",
                    "tokens": {"input": 99999, "cached": 0, "tool": 0, "output": 99999, "thoughts": 99999},
                }),
                # Rewind to m3 -> drops m3 and m4!
                json.dumps({"$rewindTo": "m3"}),
                # Update summary via $set
                json.dumps({"$set": {"summary": "Rewound Gemini Session"}}),
                # New branch after rewind
                json.dumps({
                    "id": "m5",
                    "type": "gemini",
                    "timestamp": "2026-11-23T13:02:00Z",
                    "model": "gemini-2.0-flash-lite",
                    "tokens": {"input": 400, "cached": 100, "tool": 50, "output": 80, "thoughts": 20},
                }),
            ]
            (proj_b_chats / "session-2026-11-23-s2.jsonl").write_text(
                "\n".join(jsonl_lines) + "\n", encoding="utf-8"
            )

            spec = mod.SourceSpec("gemini", tmp_dir, "gemini")
            rep = mod.scan_usage(tmp_dir, tz=tz_utc, pricing_path=None, sources=[spec])
            self.assertEqual(len(rep.sessions), 2)

            s1 = next(s for s in rep.sessions if s.conversation_id == "gem-sess-001")
            s2 = next(s for s in rep.sessions if s.conversation_id == "gem-sess-002")

            self.assertEqual(s1.source, "gemini")
            self.assertEqual(s1.workspace, "C:/Users/alice/alpha-repo")
            self.assertEqual(s1.counts.calls, 1)
            # uncached_in = (1000 - 600) + 150 = 550; cache_read = 600; output = 200 + 800 = 1000
            self.assertEqual(s1.counts.input_tokens, 550)
            self.assertEqual(s1.counts.cache_read_tokens, 600)
            self.assertEqual(s1.counts.cache_write_tokens, 0)
            self.assertEqual(s1.counts.output_tokens, 1000)
            self.assertEqual(s1.turns[0].reasoning_tokens, 800)
            self.assertEqual(s1.turns[0].tool_names, ("read_file",))
            self.assertEqual(s1.thinking_turns, 1)

            self.assertEqual(s2.workspace, "C:/Users/alice/beta-repo")
            self.assertEqual(s2.title, "Rewound Gemini Session")
            # m4 (99999 tokens) must be completely gone after $rewindTo m3!
            self.assertEqual(s2.counts.calls, 2)
            # m2: uncached=300, cr=200, out=150; m5: uncached=350, cr=100, out=100
            self.assertEqual(s2.counts.input_tokens, 650)
            self.assertEqual(s2.counts.cache_read_tokens, 300)
            self.assertEqual(s2.counts.output_tokens, 250)

    def test_pricing_parity_codex_gemini_bedrock_vertex_and_1h_ttl(self) -> None:
        cat = mod.PricingCatalog(None)
        d = dt.date(2026, 11, 24)

        # OpenAI / Codex models must NOT be shadowed by "gpt-5"
        expected_rates = {
            "gpt-5.4": (2.50, 0.25, 2.50, 15.00),
            "gpt-5.4-high": (2.50, 0.25, 2.50, 15.00),
            "gpt-5.3-codex": (1.75, 0.175, 1.75, 14.00),
            "gpt-5.2-codex": (1.75, 0.175, 1.75, 14.00),
            "gpt-5.2": (1.75, 0.175, 1.75, 14.00),
            "gpt-5.1-codex-mini": (0.25, 0.025, 0.25, 2.00),
            "gpt-5.1-codex": (1.25, 0.125, 1.25, 10.00),
            "gpt-5.1": (1.25, 0.125, 1.25, 10.00),
            "gpt-5-codex": (1.25, 0.125, 1.25, 10.00),
            "gpt-5-nano": (0.05, 0.005, 0.05, 0.40),
            "codex-mini-latest": (1.50, 0.375, 1.50, 6.00),
            "gpt-4.1": (2.00, 0.50, 2.00, 8.00),
            "gpt-4.1-mini": (0.40, 0.10, 0.40, 1.60),
            "gpt-4.1-nano": (0.10, 0.025, 0.10, 0.40),
            "gemini-2.0-flash": (0.10, 0.025, 0.10, 0.40),
            "gemini-2.0-flash-lite": (0.075, 0.01875, 0.075, 0.30),
        }
        for model_name, (ri, rcr, rcw, ro) in expected_rates.items():
            rate = cat.resolve_rate(model_name, 10_000, d)
            self.assertIsNotNone(rate, f"Missing rate for {model_name}")
            assert rate is not None
            self.assertAlmostEqual(rate.input, ri, places=6, msg=model_name)
            self.assertAlmostEqual(rate.cache_read, rcr, places=6, msg=model_name)
            self.assertAlmostEqual(rate.cache_write, rcw, places=6, msg=model_name)
            self.assertAlmostEqual(rate.output, ro, places=6, msg=model_name)

        # Bedrock & Vertex prefix stripping + '@' date normalization
        for raw_m, exp_in in (
            ("us.anthropic.claude-sonnet-4-5-20250929-v1:0", 3.00),
            ("eu.anthropic.claude-opus-4-6-v1", 5.00),
            ("global.anthropic.claude-haiku-4-5-20251001-v1:0", 1.00),
            ("anthropic.claude-3-5-sonnet-20241022-v2:0", 3.00),
            ("claude-opus-4-6@20260205", 5.00),
            ("projects/p/locations/us/models/claude-3-7-sonnet@20250219", 3.00),
        ):
            r = cat.resolve_rate(raw_m, 10_000, d)
            self.assertIsNotNone(r, f"Unpriced provider-prefixed model: {raw_m}")
            assert r is not None
            self.assertAlmostEqual(r.input, exp_in, places=6, msg=raw_m)

        # 1h cache write pricing at 2.0x base input (5m at normal 1.25x cache_write)
        cost_1h = cat.price_turn("claude-sonnet-4-6", 0, 0, 1_000_000, 0, d, cache_write_1h_tokens=400_000)
        # 600K * $3.75/M + 400K * $6.00/M = $2.25 + $2.40 = $4.65
        self.assertAlmostEqual(cost_1h.cache_write_usd, 4.65, places=6)
        # Clamping when cache_write_1h > cache_write
        cost_clamped = cat.price_turn("claude-sonnet-4-6", 0, 0, 500_000, 0, d, cache_write_1h_tokens=900_000)
        self.assertAlmostEqual(cost_clamped.cache_write_usd, 3.00, places=6)

    def test_multi_agent_cli_filters_sources_view_env_discovery_and_redaction(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp) / "Users" / "alice"
            agy_dir = base / ".gemini" / "antigravity-cli"
            claude_dir = base / ".claude"
            claude_cfg_dir = base / ".config" / "claude"
            codex_dir = base / ".codex"
            gemini_home = base / "custom-gemini-home"
            gemini_tmp = gemini_home / ".gemini" / "tmp"

            # 1. Antigravity fixture
            agy_logs = agy_dir / "brain" / "aaa00001-0000-0000-0000-000000000001" / ".system_generated" / "logs"
            agy_logs.mkdir(parents=True)
            (agy_logs / "transcript.jsonl").write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": "2026-11-24T01:00:00Z",
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1000,
                    "cache_read_tokens": 2000,
                    "output_tokens": 200,
                }) + "\n",
                encoding="utf-8",
            )

            # 2. Claude fixture
            cl_proj = claude_dir / "projects" / "C--Users-alice-secret-claude"
            cl_proj.mkdir(parents=True)
            (cl_proj / "bbb00002-0000-0000-0000-000000000002.jsonl").write_text(
                json.dumps({
                    "type": "assistant",
                    "uuid": "c1",
                    "sessionId": "bbb00002-0000-0000-0000-000000000002",
                    "cwd": "C:/Users/alice/secret-claude",
                    "requestId": "r1",
                    "timestamp": "2026-11-24T02:00:00Z",
                    "message": {
                        "id": "m1",
                        "model": "claude-opus-4-6",
                        "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "done"}],
                        "usage": {
                            "input_tokens": 500,
                            "cache_read_input_tokens": 1000,
                            "cache_creation_input_tokens": 200,
                            "output_tokens": 100,
                        },
                    },
                }) + "\n",
                encoding="utf-8",
            )

            # 3. Codex fixture
            cx_day = codex_dir / "sessions" / "2026" / "11" / "24"
            cx_day.mkdir(parents=True)
            (cx_day / "rollout-2026-11-24T03-00-00-ccc00003-0000-0000-0000-000000000003.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "timestamp": "2026-11-24T03:00:00Z",
                        "type": "session_meta",
                        "payload": {"id": "ccc00003-0000-0000-0000-000000000003", "cwd": "C:/Users/alice/secret-codex"},
                    }),
                    json.dumps({
                        "timestamp": "2026-11-24T03:00:01Z",
                        "type": "turn_context",
                        "payload": {"model": "gpt-5.3-codex"},
                    }),
                    json.dumps({
                        "timestamp": "2026-11-24T03:00:05Z",
                        "type": "event_msg",
                        "payload": {
                            "type": "token_count",
                            "info": {
                                "total_token_usage": {
                                    "input_tokens": 800,
                                    "cached_input_tokens": 300,
                                    "output_tokens": 150,
                                    "reasoning_output_tokens": 40,
                                }
                            },
                        },
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            # 4. Gemini CLI fixture (under $GEMINI_CLI_HOME/.gemini/tmp)
            gm_chats = gemini_tmp / "secret-gem-slug" / "chats"
            gm_chats.mkdir(parents=True)
            (gemini_tmp / "secret-gem-slug" / ".project_root").write_text(
                "C:/Users/alice/secret-gemini\n", encoding="utf-8"
            )
            (gm_chats / "session-2026-11-24-g1.json").write_text(
                json.dumps({
                    "sessionId": "ddd00004-0000-0000-0000-000000000004",
                    "summary": "Secret Gemini Launch Plan",
                    "messages": [{
                        "id": "gm1",
                        "type": "gemini",
                        "timestamp": "2026-11-24T04:00:00Z",
                        "model": "gemini-2.5-flash",
                        "tokens": {"input": 600, "cached": 200, "tool": 0, "output": 100, "thoughts": 50},
                    }],
                }),
                encoding="utf-8",
            )

            # Env discovery checks
            with mock.patch.dict(
                os.environ,
                {
                    "CLAUDE_CONFIG_DIR": f"{claude_dir},{claude_cfg_dir}",
                    "CODEX_HOME": str(codex_dir),
                    "GEMINI_CLI_HOME": str(gemini_home),
                },
                clear=False,
            ):
                self.assertEqual(mod.discover_claude_dirs(), [claude_dir])
                self.assertEqual(mod.discover_codex_dirs(), [codex_dir])
                self.assertEqual(mod.discover_gemini_dirs(), [gemini_tmp])

            all_sources = [
                mod.SourceSpec("antigravity", agy_dir, "cli"),
                mod.SourceSpec("claude", claude_dir, "claude"),
                mod.SourceSpec("codex", codex_dir, "codex"),
                mod.SourceSpec("gemini", gemini_tmp, "gemini"),
            ]

            # Incremental scanner caches all 4 agents cleanly
            scanner = mod.IncrementalScanner(agy_dir, pricing_path=None, sources=all_sources)
            rep1 = scanner.scan(tz=tz_utc)
            self.assertEqual(len(rep1.sessions), 4)
            self.assertEqual(rep1.files_reparsed, 4)
            self.assertEqual(rep1.files_cached, 0)
            rep2 = scanner.scan(tz=tz_utc)
            self.assertEqual(rep2.files_reparsed, 0)
            self.assertEqual(rep2.files_cached, 4)

            agg = mod.aggregate_sessions(rep1.sessions)
            self.assertEqual(set(agg["by_source"].keys()), {"cli", "claude", "codex", "gemini"})
            self.assertEqual(set(agg["by_agent"].keys()), {"antigravity", "claude", "codex", "gemini"})

            # --view sources & --view all & CSV sources & JSON by_source/by_agent
            txt_sources = mod.render_text(
                rep1, agg, view="sources", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None, color=False,
            )
            self.assertIn("By Source", txt_sources)
            self.assertIn("Claude Code", txt_sources)
            self.assertIn("Codex CLI", txt_sources)
            self.assertIn("Gemini CLI", txt_sources)

            csv_sources = mod.render_csv(rep1, agg, view="sources", sort_by="time")
            self.assertIn("source,agent,sessions", csv_sources.splitlines()[0])
            self.assertEqual(len(csv_sources.splitlines()), 5)

            # --redact zero-PII check across all 4 agents (text, JSON, warnings)
            rep1.warnings.append(
                f"Could not read {claude_dir / 'projects' / 'C--Users-alice-secret-claude' / 'bbb00002.jsonl'}: perm"
            )
            secrets = (
                "alice",
                "secret-claude",
                "secret-codex",
                "secret-gemini",
                "Secret Gemini Launch Plan",
                "custom-gemini-home",
                "aaa00001",
                "bbb00002",
                "ccc00003",
                "ddd00004",
            )
            txt_red = mod.render_text(
                rep1, agg, view="all", sort_by="time", limit=10, human=True,
                tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                color=False, redact=True,
            )
            js_red_raw = mod.render_json(rep1, agg, sort_by="time", tz_label="UTC", include_turns=True, redact=True)
            for sec in secrets:
                self.assertNotIn(sec, txt_red)
                self.assertNotIn(sec, js_red_raw)
            self.assertIn("~/.claude", txt_red)
            self.assertIn("~/.codex", txt_red)
            self.assertIn("~/.gemini/tmp", txt_red)
            self.assertIn("By Source", txt_red)
            js_red = json.loads(js_red_raw)
            self.assertEqual(set(js_red["by_agent"].keys()), {"antigravity", "claude", "codex", "gemini"})
            self.assertIn("~/.claude/<redacted-path>", js_red["warnings"][0])
            self.assertEqual(mod._redact_data_dir(claude_cfg_dir), "~/.config/claude")

            # CLI flag parsing: --agy-only and --agent / --agents
            self.assertEqual(mod.parse_agent_filter(["agy,claude"]), {"antigravity", "claude"})
            self.assertEqual(mod.parse_agent_filter(["codex", "gemini"]), {"codex", "gemini"})
            with self.assertRaises(ValueError):
                mod.parse_agent_filter(["unknown-agent"])

            with mock.patch.object(mod, "discover_default_sources", side_effect=lambda f: [
                s for s in all_sources if f is None or s.kind in f
            ]):
                buf_agy = io.StringIO()
                with mock.patch("sys.stdout", buf_agy):
                    rc = mod.main(["--agy-only", "--no-archive", "--format", "json", "--utc"])
                self.assertEqual(rc, 0)
                js_agy = json.loads(buf_agy.getvalue())
                self.assertEqual(js_agy["session_count"], 1)
                self.assertEqual(js_agy["sessions"][0]["source"], "cli")

                buf_cc = io.StringIO()
                with mock.patch("sys.stdout", buf_cc):
                    rc2 = mod.main(["--agents", "claude,codex", "--no-archive", "--format", "json", "--utc"])
                self.assertEqual(rc2, 0)
                js_cc = json.loads(buf_cc.getvalue())
                self.assertEqual(js_cc["session_count"], 2)
                self.assertEqual(set(js_cc["by_agent"].keys()), {"claude", "codex"})

    def test_rust_multi_agent_archive_backfill_and_zero_double_count(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp) / "Users" / "alice"
            claude_dir = base / ".claude"
            cl_proj = claude_dir / "projects" / "C--Users-alice-proj"
            cl_proj.mkdir(parents=True)
            live_claude_id = "11111111-1111-4111-8111-111111111111"
            (cl_proj / f"{live_claude_id}.jsonl").write_text(
                json.dumps({
                    "type": "assistant",
                    "uuid": "live-1",
                    "sessionId": live_claude_id,
                    "cwd": "C:/Users/alice/proj",
                    "requestId": "req-live",
                    "timestamp": "2026-11-24T05:00:00Z",
                    "message": {
                        "id": "msg-live",
                        "model": "claude-sonnet-4-5-20250929",
                        "stop_reason": "end_turn",
                        "content": [{"type": "text", "text": "ok"}],
                        "usage": {
                            "input_tokens": 1000,
                            "cache_read_input_tokens": 2000,
                            "cache_creation_input_tokens": 500,
                            "output_tokens": 100,
                            "cache_creation": {"ephemeral_1h_input_tokens": 200},
                        },
                    },
                }) + "\n",
                encoding="utf-8",
            )

            # Rust archive.json uses "<family>:<cid>" keys + "conversation_id" + "claude-code"/"gemini-cli" sources
            arch_path = base / "archive.json"
            arch_path.write_text(
                json.dumps({
                    "schema_version": 1,
                    "sessions": {
                        # Duplicate of live Claude session -> MUST be deduplicated (zero double-count!)
                        f"claude-code:{live_claude_id}": {
                            "conversation_id": live_claude_id,
                            "title": "Live Claude Archived Copy",
                            "workspace": "C:/Users/alice/proj",
                            "source": "claude-code",
                            "turns": [{
                                "step": 1,
                                "ts": "2026-11-24T05:00:00Z",
                                "m": "claude-sonnet-4-5-20250929",
                                "in": 1000,
                                "cr": 2000,
                                "cw": 500,
                                "cw1h": 200,
                                "out": 100,
                            }],
                        },
                        # Pruned Claude session (with key prefix fallback, no explicit conversation_id) -> backfilled & normalized to "claude"
                        "claude-code:22222222-2222-4222-8222-222222222222": {
                            "title": "Pruned Claude Session",
                            "workspace": "C:/Users/alice/old-claude",
                            "source": "claude-code",
                            "turns": [{
                                "step": 1,
                                "ts": "2026-11-22T05:00:00Z",
                                "m": "claude-opus-4-6",
                                "in": 200,
                                "cr": 800,
                                "cw": 100,
                                "cw1h": 100,
                                "out": 50,
                            }],
                        },
                        # Pruned Gemini CLI session -> backfilled & normalized to "gemini" with reasoning tokens
                        "gemini-cli:gem-pruned-001": {
                            "conversation_id": "gem-pruned-001",
                            "title": "Pruned Gemini Session",
                            "workspace": "C:/Users/alice/old-gemini",
                            "source": "gemini-cli",
                            "turns": [{
                                "step": 1,
                                "ts": "2026-11-22T06:00:00Z",
                                "m": "gemini-2.5-pro",
                                "in": 300,
                                "cr": 700,
                                "out": 400,
                                "rt": 250,
                            }],
                        },
                    },
                }),
                encoding="utf-8",
            )

            spec = mod.SourceSpec("claude", claude_dir, "claude")
            rep = mod.scan_usage(
                claude_dir,
                tz=tz_utc,
                pricing_path=None,
                sources=[spec],
                archive_path=arch_path,
            )
            # 1 live Claude + 2 pruned archived (1 claude + 1 gemini), ZERO double-count of live Claude!
            self.assertEqual(len(rep.sessions), 3)
            self.assertEqual(rep.archived_sessions_count, 2)
            agg = mod.aggregate_sessions(rep.sessions)
            self.assertEqual(set(agg["by_source"].keys()), {"claude", "gemini"})
            self.assertNotIn("claude-code", agg["by_source"])
            self.assertNotIn("gemini-cli", agg["by_source"])
            self.assertEqual(agg["source_sessions"]["claude"], 2)
            self.assertEqual(agg["source_sessions"]["gemini"], 1)

            pruned_cl = next(s for s in rep.sessions if s.conversation_id == "22222222-2222-4222-8222-222222222222")
            self.assertEqual(pruned_cl.source, "claude")
            self.assertEqual(pruned_cl.turns[0].cache_write_1h_tokens, 100)
            pruned_gm = next(s for s in rep.sessions if s.conversation_id == "gem-pruned-001")
            self.assertEqual(pruned_gm.source, "gemini")
            self.assertEqual(pruned_gm.turns[0].reasoning_tokens, 250)
            self.assertEqual(pruned_gm.thinking_turns, 1)

            # Agent filter on Claude excludes pruned Gemini archive backfill
            rep_cl_only = mod.scan_usage(
                claude_dir,
                tz=tz_utc,
                pricing_path=None,
                sources=[spec],
                archive_path=arch_path,
                agent_filter={"claude"},
            )
            self.assertEqual(len(rep_cl_only.sessions), 2)
            self.assertTrue(all(s.source == "claude" for s in rep_cl_only.sessions))

    def test_mtime_pruning_future_guard_subagent_parent_and_codex_dir_prune(self) -> None:
        tz_utc = dt.timezone.utc
        now = dt.datetime.now(tz_utc).replace(microsecond=0)
        today = now.date()
        old_dt = now - dt.timedelta(days=5)
        old_ts = old_dt.timestamp()

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            # 1. Antigravity: 1 old file (mtime 5 days ago) + 1 today file
            agy_root = root / "antigravity-cli"
            old_cid = "aaaa0001-0000-0000-0000-000000000001"
            new_cid = "aaaa0002-0000-0000-0000-000000000002"
            old_log = agy_root / "brain" / old_cid / ".system_generated" / "logs" / "transcript.jsonl"
            new_log = agy_root / "brain" / new_cid / ".system_generated" / "logs" / "transcript.jsonl"
            old_log.parent.mkdir(parents=True)
            new_log.parent.mkdir(parents=True)
            old_log.write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": old_dt.isoformat(),
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 900,
                    "cache_read_tokens": 0,
                    "output_tokens": 100,
                }) + "\n",
                encoding="utf-8",
            )
            os.utime(old_log, (old_ts, old_ts))
            new_log.write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": now.isoformat(),
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 500,
                    "cache_read_tokens": 1000,
                    "output_tokens": 200,
                }) + "\n",
                encoding="utf-8",
            )

            # Narrow scan (--today) prunes old_log without parsing it
            scanner = mod.IncrementalScanner(agy_root, pricing_path=None)
            rep_today = scanner.scan(tz=tz_utc, since=today, until=today)
            self.assertEqual(len(rep_today.sessions), 1)
            self.assertEqual(rep_today.sessions[0].conversation_id, new_cid)
            self.assertEqual(rep_today.files_reparsed, 1)
            self.assertEqual(rep_today.skipped_empty_sessions, 1)

            # Subsequent wide scan on same IncrementalScanner still reads old_log (and caches new_log)
            rep_all = scanner.scan(tz=tz_utc)
            self.assertEqual(len(rep_all.sessions), 2)
            self.assertEqual(rep_all.files_cached, 1)
            self.assertEqual(rep_all.files_reparsed, 1)

            # Wide-then-narrow must NOT evict old_log from cache!
            rep_today2 = scanner.scan(tz=tz_utc, since=today, until=today)
            self.assertEqual(len(rep_today2.sessions), 1)
            rep_all2 = scanner.scan(tz=tz_utc)
            self.assertEqual(len(rep_all2.sessions), 2)
            self.assertEqual(rep_all2.files_cached, 2)
            self.assertEqual(rep_all2.files_reparsed, 0)

            # Future-dated fixture guard: when `since` is in the future relative to host clock, mtime pruning disables
            future_date = today + dt.timedelta(days=30)
            fut_cid = "aaaa0003-0000-0000-0000-000000000003"
            fut_log = agy_root / "brain" / fut_cid / ".system_generated" / "logs" / "transcript.jsonl"
            fut_log.parent.mkdir(parents=True)
            fut_log.write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": f"{future_date.isoformat()}T12:00:00Z",
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 300,
                    "output_tokens": 50,
                }) + "\n",
                encoding="utf-8",
            )
            rep_fut = mod.scan_usage(agy_root, tz=tz_utc, since=future_date, until=future_date, pricing_path=None)
            self.assertEqual(len(rep_fut.sessions), 1)
            self.assertEqual(rep_fut.sessions[0].conversation_id, fut_cid)

            # 2. Claude subagent active today whose parent was mtime-pruned (and no sessions-index.json)
            claude_root = root / ".claude"
            proj_dir = claude_root / "projects" / "proj-x"
            parent_uuid = "bbbb0001-0000-4000-8000-000000000001"
            sub_dir = proj_dir / parent_uuid / "subagents"
            sub_dir.mkdir(parents=True)
            parent_file = proj_dir / f"{parent_uuid}.jsonl"
            parent_file.write_text(
                json.dumps({
                    "type": "user",
                    "uuid": "u0",
                    "sessionId": parent_uuid,
                    "cwd": "C:/Users/alice/lazy-parent",
                    "timestamp": old_dt.isoformat(),
                    "message": {"role": "user", "content": "Old Parent Task"},
                }) + "\n",
                encoding="utf-8",
            )
            os.utime(parent_file, (old_ts, old_ts))
            (sub_dir / "agent-sub1.jsonl").write_text(
                json.dumps({
                    "type": "assistant",
                    "uuid": "a1",
                    "sessionId": parent_uuid,
                    "agentId": "sub1",
                    "requestId": "r1",
                    "timestamp": now.isoformat(),
                    "message": {
                        "id": "m1",
                        "model": "claude-sonnet-4-5-20250929",
                        "usage": {"input_tokens": 100, "output_tokens": 50},
                    },
                }) + "\n",
                encoding="utf-8",
            )
            rep_cl = mod.scan_usage(
                claude_root,
                tz=tz_utc,
                since=today,
                until=today,
                pricing_path=None,
                sources=[mod.SourceSpec("claude", claude_root, "claude")],
            )
            self.assertEqual(len(rep_cl.sessions), 1)
            self.assertEqual(rep_cl.sessions[0].parent_title, "Old Parent Task")
            self.assertEqual(rep_cl.sessions[0].workspace, "C:/Users/alice/lazy-parent")

            # 3. Codex directory pruning skips YYYY/MM/DD dirs strictly after until + 1 day
            codex_root = root / ".codex"
            day_ok = codex_root / "sessions" / f"{old_dt.year:04d}" / f"{old_dt.month:02d}" / f"{old_dt.day:02d}"
            day_skip_dt = old_dt + dt.timedelta(days=3)
            day_skip = codex_root / "sessions" / f"{day_skip_dt.year:04d}" / f"{day_skip_dt.month:02d}" / f"{day_skip_dt.day:02d}"
            day_ok.mkdir(parents=True)
            day_skip.mkdir(parents=True)
            (day_ok / "rollout-2026-ok.jsonl").write_text(
                json.dumps({
                    "timestamp": old_dt.isoformat(),
                    "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 100, "output_tokens": 20}}},
                }) + "\n",
                encoding="utf-8",
            )
            (day_skip / "rollout-2026-skip.jsonl").write_text(
                json.dumps({
                    "timestamp": day_skip_dt.isoformat(),
                    "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 999, "output_tokens": 99}}},
                }) + "\n",
                encoding="utf-8",
            )
            rep_cx = mod.scan_usage(
                codex_root,
                tz=tz_utc,
                until=old_dt.date(),
                pricing_path=None,
                sources=[mod.SourceSpec("codex", codex_root, "codex")],
            )
            self.assertEqual(len(rep_cx.sessions), 1)
            self.assertEqual(rep_cx.files_reparsed, 1)

    def test_archive_discovered_live_keys_dedup_under_today_and_streaming_parity(self) -> None:
        tz_utc = dt.timezone.utc
        now = dt.datetime.now(tz_utc).replace(microsecond=0)
        today = now.date()
        old_dt = now - dt.timedelta(days=4)
        old_ts = old_dt.timestamp()

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            agy_root = root / "antigravity-cli"
            live_old_cid = "cccc0001-0000-0000-0000-000000000001"
            deleted_today_cid = "cccc0002-0000-0000-0000-000000000002"
            deleted_old_cid = "cccc0003-0000-0000-0000-000000000003"

            live_log = agy_root / "brain" / live_old_cid / ".system_generated" / "logs" / "transcript.jsonl"
            live_log.parent.mkdir(parents=True)
            live_log.write_text(
                json.dumps({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "created_at": old_dt.isoformat(),
                    "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1000,
                    "output_tokens": 100,
                }) + "\n",
                encoding="utf-8",
            )
            os.utime(live_log, (old_ts, old_ts))

            # Archive contains:
            # 1) live_old_cid (shadowed by live file on disk -> MUST be deduped even under --today!)
            # 2) deleted_today_cid (deleted from disk, has today turn + CJK/emoji title -> MUST backfill!)
            # 3) deleted_old_cid (deleted from disk, only old turn -> skipped by date pre-check under --today)
            arch_payload = {
                "schema_version": 12345,
                "extra_meta": {"note": "before sessions"},
                "sessions": {
                    live_old_cid: {
                        "source": "cli",
                        "title": "Live Old Shadowed",
                        "workspace": "C:/w",
                        "turns": [{"step": 1, "ts": now.isoformat(), "m": "gemini-4-neon-xhigh", "in": 77777, "out": 77}],
                    },
                    deleted_today_cid: {
                        "source": "cli",
                        "title": "已归档会话 🚀 " + ("x" * 200),
                        "workspace": "C:/archived",
                        "turns": [{"step": 1, "ts": now.isoformat(), "m": "gemini-4-neon-xhigh", "in": 2000, "cr": 4000, "out": 300, "tools": ["read_file"]}],
                    },
                    deleted_old_cid: {
                        "source": "cli",
                        "title": "Old Pruned",
                        "workspace": "C:/old",
                        "turns": [{"step": 1, "ts": old_dt.isoformat(), "m": "gemini-4-neon-xhigh", "in": 500, "out": 50}],
                    },
                },
                "trailing_field": 99,
            }
            arch_path = root / "archive.json"
            # Test both compact and pretty-printed streaming with tiny chunk_size=19 to stress chunk boundaries
            for indent in (None, 2):
                arch_path.write_text(json.dumps(arch_payload, ensure_ascii=False, indent=indent), encoding="utf-8")
                streamed = dict(mod._iter_archive_sessions(arch_path, chunk_size=19))
                self.assertEqual(set(streamed.keys()), set(arch_payload["sessions"].keys()))
                self.assertEqual(streamed[deleted_today_cid]["title"], arch_payload["sessions"][deleted_today_cid]["title"])

            rep_today = mod.scan_usage(
                agy_root, tz=tz_utc, since=today, until=today, pricing_path=None, archive_path=arch_path
            )
            self.assertEqual(len(rep_today.sessions), 1)
            self.assertEqual(rep_today.archived_sessions_count, 1)
            self.assertEqual(rep_today.sessions[0].conversation_id, deleted_today_cid)
            self.assertEqual(rep_today.sessions[0].counts.total_tokens, 6300)

    def test_compact_scoping_with_and_without_weekly_budget(self) -> None:
        tz_utc = dt.timezone.utc
        now = dt.datetime.now(tz_utc).replace(microsecond=0)
        today = now.date()
        iso = today.isocalendar()
        week_monday = dt.date.fromisocalendar(iso.year, iso.week, 1)
        prev_week_day = week_monday - dt.timedelta(days=2)

        with tempfile.TemporaryDirectory() as tmp:
            agy_root = pathlib.Path(tmp) / "antigravity-cli"
            c_today = "dddd0001-0000-0000-0000-000000000001"
            c_prev_wk = "dddd0002-0000-0000-0000-000000000002"
            p_today = agy_root / "brain" / c_today / ".system_generated" / "logs" / "transcript.jsonl"
            p_prev_wk = agy_root / "brain" / c_prev_wk / ".system_generated" / "logs" / "transcript.jsonl"
            p_today.parent.mkdir(parents=True)
            p_prev_wk.parent.mkdir(parents=True)

            # Today: 100k input of gemini-4-neon ($3/M = $0.30)
            p_today.write_text(
                json.dumps({
                    "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": now.isoformat(), "model": "gemini-4-neon-xhigh",
                    "input_tokens": 100_000, "output_tokens": 0,
                }) + "\n",
                encoding="utf-8",
            )
            # Previous ISO week: 2M input ($6.00) with old mtime
            p_prev_wk.write_text(
                json.dumps({
                    "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": f"{prev_week_day.isoformat()}T12:00:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 2_000_000, "output_tokens": 0,
                }) + "\n",
                encoding="utf-8",
            )
            old_ts = dt.datetime.combine(prev_week_day, dt.time(12, 0), tzinfo=tz_utc).timestamp()
            os.utime(p_prev_wk, (old_ts, old_ts))

            # Optional earlier day in SAME ISO week (if today is not Monday): 1M input ($3.00)
            has_earlier_in_week = today > week_monday
            if has_earlier_in_week:
                c_wk = "dddd0003-0000-0000-0000-000000000003"
                p_wk = agy_root / "brain" / c_wk / ".system_generated" / "logs" / "transcript.jsonl"
                p_wk.parent.mkdir(parents=True)
                p_wk.write_text(
                    json.dumps({
                        "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": f"{week_monday.isoformat()}T12:00:00Z", "model": "gemini-4-neon-xhigh",
                        "input_tokens": 1_000_000, "output_tokens": 0,
                    }) + "\n",
                    encoding="utf-8",
                )
                wk_ts = dt.datetime.combine(week_monday, dt.time(12, 0), tzinfo=tz_utc).timestamp()
                os.utime(p_wk, (wk_ts, wk_ts))

            # 1. --compact alone prints today's $0.30 and returns 0
            buf = io.StringIO()
            with mock.patch("sys.stdout", buf):
                rc = mod.main(["--data-dir", str(agy_root), "--compact", "--no-archive", "--utc", "--pricing", ""])
            self.assertEqual(rc, 0)
            self.assertIn("AGY today=100.0K ($0.30)", buf.getvalue())

            # 2. --compact --check-budget --budget-weekly 1.0 triggers CRITICAL (20) when earlier day in week exists
            buf2, err2 = io.StringIO(), io.StringIO()
            with mock.patch("sys.stdout", buf2), mock.patch("sys.stderr", err2):
                rc2 = mod.main([
                    "--data-dir", str(agy_root), "--compact", "--no-archive", "--utc",
                    "--check-budget", "--budget-weekly", "1.0",
                ])
            if has_earlier_in_week:
                self.assertEqual(rc2, 20)
                self.assertIn("CRITICAL: weekly", err2.getvalue())
            else:
                self.assertEqual(rc2, 0)

    def test_prefilter_edge_cases_claude_codex_gemini_and_pricing_cache_invalidation(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            # 1. Claude prefilter: isMeta user, huge progress/file-history lines, malformed trailing line, late custom-title
            cl_file = root / "claude-edge.jsonl"
            cl_lines = [
                json.dumps({"type": "progress", "cwd": "C:/cl-proj", "sessionId": "cl-edge-1", "blob": "x" * 4096}),
                json.dumps({"type": "user", "isMeta": True, "message": {"role": "user", "content": "meta noise"}}),
                json.dumps({"type": "user", "isMeta": False, "message": {"role": "user", "content": "Real prompt"}}),
                json.dumps({"type": "file-history-snapshot", "files": ["a" * 2048]}),
                json.dumps({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "y" * 4096}]}}),
                json.dumps({
                    "type": "assistant", "uuid": "a1", "requestId": "r1", "timestamp": "2026-11-24T10:00:00Z",
                    "message": {"id": "m1", "model": "claude-sonnet-4-5-20250929", "usage": {"input_tokens": 100, "output_tokens": 20}},
                }),
                json.dumps({"type": "custom-title", "customTitle": "Late Custom Title"}),
                '{"type": "progress", "truncated": ',  # malformed line MUST still count in skipped_lines
            ]
            cl_file.write_text("\n".join(cl_lines) + "\n", encoding="utf-8")
            cl_res = mod._parse_claude_jsonl(cl_file, fallback_cid="fb", is_subagent=False)
            self.assertEqual(cl_res["cid"], "cl-edge-1")
            self.assertEqual(cl_res["cwd"], "C:/cl-proj")
            self.assertEqual(cl_res["title_hint"], "Late Custom Title")
            self.assertEqual(len(cl_res["turns"]), 1)
            self.assertEqual(cl_res["skipped_lines"], 1)

            # 2. Codex prefilter: huge function_call_output/custom_tool_call_output skipped, real calls + truncated line kept
            cx_file = root / "rollout-2026-11-24-cx1.jsonl"
            cx_lines = [
                json.dumps({"timestamp": "2026-11-24T10:00:00Z", "type": "session_meta", "payload": {"id": "cx-edge-1", "cwd": "C:/cx"}}),
                json.dumps({"timestamp": "2026-11-24T10:00:01Z", "type": "turn_context", "payload": {"model": "gpt-5.4"}}),
                json.dumps({"timestamp": "2026-11-24T10:00:02Z", "type": "response_item", "payload": {"type": "function_call", "name": "shell"}}),
                json.dumps({"timestamp": "2026-11-24T10:00:03Z", "type": "response_item", "payload": {"type": "function_call_output", "output": "z" * 8192}}),
                json.dumps({"timestamp": "2026-11-24T10:00:03Z", "type": "response_item", "payload": {"type": "custom_tool_call_output", "output": "w" * 8192}}),
                json.dumps({"timestamp": "2026-11-24T10:00:04Z", "type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 200, "cached_input_tokens": 50, "output_tokens": 40}}}}),
                '{"type": "response_item", "broken": ',
            ]
            cx_file.write_text("\n".join(cx_lines) + "\n", encoding="utf-8")
            cx_res = mod._parse_codex_jsonl(cx_file)
            self.assertEqual(cx_res["cid"], "cx-edge-1")
            self.assertEqual(len(cx_res["turns"]), 1)
            self.assertEqual(cx_res["turns"][0].tool_names, ("shell",))
            self.assertEqual(cx_res["skipped_lines"], 1)

            # 3. Gemini O(N) replay: message_update, $set by id, int $rewindTo, duplicate id
            gm_file = root / "session-edge.jsonl"
            gm_lines = [
                json.dumps({"sessionId": "gm-edge-1"}),
                json.dumps({"id": "m1", "type": "gemini", "timestamp": "2026-11-24T10:00:00Z", "model": "gemini-2.5-flash", "tokens": {"input": 100, "cached": 0, "output": 10, "thoughts": 0}}),
                json.dumps({"type": "message_update", "id": "m1", "patch": {"tokens": {"input": 200, "cached": 50, "output": 30, "thoughts": 10}}}),
                json.dumps({"id": "m2", "type": "gemini", "timestamp": "2026-11-24T10:01:00Z", "model": "gemini-2.5-flash", "tokens": {"input": 999, "cached": 0, "output": 999, "thoughts": 0}}),
                json.dumps({"$rewindTo": 1}),
                json.dumps({"id": "m1", "$set": {"model": "gemini-2.5-pro"}}),
            ]
            gm_file.write_text("\n".join(gm_lines) + "\n", encoding="utf-8")
            gm_res = mod._parse_gemini_session_file(gm_file)
            self.assertEqual(gm_res["cid"], "gm-edge-1")
            self.assertEqual(len(gm_res["turns"]), 1)
            self.assertEqual(gm_res["turns"][0].model, "gemini-2.5-pro")
            self.assertEqual(gm_res["turns"][0].input_tokens, 150)
            self.assertEqual(gm_res["turns"][0].cache_read_tokens, 50)
            self.assertEqual(gm_res["turns"][0].output_tokens, 40)

            # 4. PricingCatalog cache invalidates when pricing.json changes
            p_path = root / "pricing.json"
            p_path.write_text(json.dumps({"models": {"gemini-4-neon": {"input": 9.0, "output": 18.0}}}), encoding="utf-8")
            cat = mod.PricingCatalog(p_path)
            r1 = cat.resolve_rate("gemini-4-neon-xhigh", 0, None)
            self.assertIsNotNone(r1)
            assert r1 is not None
            self.assertAlmostEqual(r1.input, 9.0)
            p_path.write_text(json.dumps({"models": {"gemini-4-neon": {"input": 12.5, "output": 25.0}}}), encoding="utf-8")
            # Bump mtime explicitly so (mtime_ns, size) always differs even on coarse FS
            st_p = p_path.stat()
            os.utime(p_path, ns=(st_p.st_atime_ns, st_p.st_mtime_ns + 1_000_000_000))
            self.assertTrue(cat.refresh())
            r2 = cat.resolve_rate("gemini-4-neon-xhigh", 0, None)
            self.assertIsNotNone(r2)
            assert r2 is not None
            self.assertAlmostEqual(r2.input, 12.5)

    def test_retain_turns_false_digest_parity_across_all_views_and_agents(self) -> None:
        tz_utc = dt.timezone.utc
        now = dt.datetime(2026, 11, 26, 14, 30, 0, tzinfo=tz_utc)
        earlier = dt.datetime(2026, 11, 25, 9, 15, 0, tzinfo=tz_utc)

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            # 1. Antigravity root session + subagent + checkpoint + unpriced model
            agy_root = root / "antigravity-cli"
            parent_cid = "eeee0001-0000-4000-8000-000000000001"
            sub_cid = "eeee0002-0000-4000-8000-000000000002"
            p_logs = agy_root / "brain" / parent_cid / ".system_generated" / "logs"
            s_logs = agy_root / "brain" / sub_cid / ".system_generated" / "logs"
            p_logs.mkdir(parents=True)
            s_logs.mkdir(parents=True)
            (agy_root / "cache").mkdir(parents=True)
            (agy_root / "cache" / "conversation_metadata.json").write_text(
                json.dumps({
                    parent_cid: {
                        "title": "Digest Parent 任务",
                        "workspace": "C:/Users/alice/digest-repo",
                        "is_internal": False,
                    },
                    sub_cid: {
                        "title": "Digest Subagent",
                        "is_internal": True,
                    },
                }),
                encoding="utf-8",
            )
            (p_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": earlier.isoformat().replace("+00:00", "Z"),
                        "model": "gemini-4-neon-xhigh",
                        "input_tokens": 10000, "cache_read_tokens": 20000, "output_tokens": 500,
                        "thinking": "analyze", "tool_calls": [{"name": "read_file", "args": {}}],
                    }, separators=(",", ":")),
                    json.dumps({
                        "step_index": 2, "type": "CHECKPOINT", "status": "DONE",
                        "created_at": now.isoformat().replace("+00:00", "Z"),
                        "model": "",
                        "input_tokens": 4000, "cache_read_tokens": 0, "output_tokens": 200,
                    }, separators=(",", ":")),
                    json.dumps({
                        "step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": (now + dt.timedelta(minutes=3)).isoformat().replace("+00:00", "Z"),
                        "model": "mystery-unpriced-v9",
                        "input_tokens": 1200, "cache_read_tokens": 300, "output_tokens": 100,
                    }, separators=(",", ":")),
                ]) + "\n",
                encoding="utf-8",
            )
            (s_logs / "transcript.jsonl").write_text(
                "\n".join([
                    json.dumps({
                        "step_index": 0, "type": "SYSTEM_MESSAGE",
                        "content": f"Spawned by parent conversation {parent_cid} to inspect tests",
                    }),
                    json.dumps({
                        "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": (now + dt.timedelta(minutes=1)).isoformat().replace("+00:00", "Z"),
                        "model": "claude-opus-5-5",
                        "input_tokens": 5000, "cache_read_tokens": 15000, "output_tokens": 800,
                        "tool_calls": [{"name": "grep_search", "args": {}}],
                    }, separators=(",", ":")),
                ]) + "\n",
                encoding="utf-8",
            )

            # 2. Claude Code session with 1h cache write
            cl_root = root / ".claude"
            cl_proj = cl_root / "projects" / "proj-a"
            cl_proj.mkdir(parents=True)
            cl_cid = "ffff0001-0000-4000-8000-000000000001"
            (cl_proj / f"{cl_cid}.jsonl").write_text(
                json.dumps({
                    "type": "assistant", "uuid": "a1", "sessionId": cl_cid, "cwd": "C:/Users/alice/digest-repo",
                    "requestId": "req1", "timestamp": now.isoformat().replace("+00:00", "Z"),
                    "message": {
                        "id": "m1", "model": "claude-sonnet-4-5-20250929", "stop_reason": "end_turn",
                        "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "tool_use", "id": "t1", "name": "Read"}],
                        "usage": {
                            "input_tokens": 1000, "cache_read_input_tokens": 3000,
                            "cache_creation_input_tokens": 1000, "output_tokens": 250,
                            "cache_creation": {"ephemeral_5m_input_tokens": 600, "ephemeral_1h_input_tokens": 400},
                        },
                    },
                }) + "\n",
                encoding="utf-8",
            )

            # 3. Codex CLI session with reasoning tokens
            cx_root = root / ".codex"
            cx_day = cx_root / "sessions" / "2026" / "11" / "26"
            cx_day.mkdir(parents=True)
            cx_cid = "aaaa0001-0000-4000-8000-000000000001"
            (cx_day / f"rollout-2026-11-26-{cx_cid}.jsonl").write_text(
                "\n".join([
                    json.dumps({"timestamp": now.isoformat().replace("+00:00", "Z"), "type": "session_meta", "payload": {"id": cx_cid, "cwd": "C:/cx"}}),
                    json.dumps({"timestamp": now.isoformat().replace("+00:00", "Z"), "type": "turn_context", "payload": {"model": "gpt-5.4"}}),
                    json.dumps({"timestamp": now.isoformat().replace("+00:00", "Z"), "type": "response_item", "payload": {"type": "function_call", "name": "shell"}}),
                    json.dumps({
                        "timestamp": now.isoformat().replace("+00:00", "Z"), "type": "event_msg",
                        "payload": {"type": "token_count", "info": {"last_token_usage": {
                            "input_tokens": 2000, "cached_input_tokens": 500, "output_tokens": 400, "reasoning_output_tokens": 150,
                        }}},
                    }),
                ]) + "\n",
                encoding="utf-8",
            )

            # 4. Gemini CLI session + archive backfill
            gm_root = root / ".gemini"
            gm_chats = gm_root / "tmp" / "proj-g" / "chats"
            gm_chats.mkdir(parents=True)
            (gm_chats / "session-gm1.json").write_text(
                json.dumps({
                    "sessionId": "gm-sess-1", "summary": "Gemini Digest",
                    "messages": [{
                        "id": "g1", "type": "gemini", "timestamp": now.isoformat().replace("+00:00", "Z"),
                        "model": "gemini-2.5-pro",
                        "tokens": {"input": 1500, "cached": 500, "output": 200, "thoughts": 80},
                        "toolCalls": [{"name": "run_shell"}],
                    }],
                }),
                encoding="utf-8",
            )
            arch_path = root / "archive.json"
            arch_path.write_text(
                json.dumps({
                    "schema_version": 1,
                    "sessions": {
                        "arch-001": {
                            "source": "ide", "title": "Archived Digest", "workspace": "C:/arch",
                            "turns": [
                                {"step": 1, "ts": earlier.isoformat().replace("+00:00", "Z"), "m": "gemini-4-hydrogen", "in": 3000, "cr": 9000, "out": 300, "tools": ["view_file"], "th": 20},
                                {"step": 2, "ts": now.isoformat().replace("+00:00", "Z"), "cp": True, "m": "", "in": 1000, "cr": 0, "out": 50},
                            ],
                        }
                    },
                }),
                encoding="utf-8",
            )

            specs = [
                mod.SourceSpec("antigravity", agy_root, "cli"),
                mod.SourceSpec("claude", cl_root, "claude"),
                mod.SourceSpec("codex", cx_root, "codex"),
                mod.SourceSpec("gemini", gm_root, "gemini"),
            ]
            rep_full = mod.scan_usage(agy_root, tz=tz_utc, pricing_path=None, archive_path=arch_path, sources=specs, retain_turns=True)
            rep_fast = mod.scan_usage(agy_root, tz=tz_utc, pricing_path=None, archive_path=arch_path, sources=specs, retain_turns=False)

            self.assertEqual(len(rep_full.sessions), 6)
            self.assertEqual(len(rep_fast.sessions), 6)
            self.assertTrue(all(len(s.turns) == 0 for s in rep_fast.sessions))
            self.assertTrue(all(s._digest is not None for s in rep_fast.sessions))

            agg_full = mod.aggregate_sessions(rep_full.sessions)
            agg_fast = mod.aggregate_sessions(rep_fast.sessions)

            for view in ("summary", "all", "daily", "weekly", "monthly", "hourly", "model", "session", "workspace", "tools", "sources"):
                for show_sub in (False, True):
                    for redact in (False, True):
                        t_full = mod.render_text(
                            rep_full, agg_full, view=view, sort_by="cost", limit=15, human=True,
                            tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                            color=False, show_subagents=show_sub, redact=redact,
                        )
                        t_fast = mod.render_text(
                            rep_fast, agg_fast, view=view, sort_by="cost", limit=15, human=True,
                            tz_label="UTC", since=None, until=None, model_filter=None, session_filter=None,
                            color=False, show_subagents=show_sub, redact=redact,
                        )
                        self.assertEqual(t_fast, t_full, f"Mismatch in render_text(view={view}, show_sub={show_sub}, redact={redact})")

            for redact in (False, True):
                j_full = mod.render_json(rep_full, agg_full, sort_by="time", tz_label="UTC", include_turns=False, redact=redact)
                j_fast = mod.render_json(rep_fast, agg_fast, sort_by="time", tz_label="UTC", include_turns=False, redact=redact)
                self.assertEqual(j_fast, j_full, f"Mismatch in render_json(redact={redact})")

            for view in ("summary", "model", "daily", "weekly", "monthly", "hourly", "sources", "workspace"):
                c_full = mod.render_csv(rep_full, agg_full, view=view, sort_by="total", redact=False)
                c_fast = mod.render_csv(rep_fast, agg_fast, view=view, sort_by="total", redact=False)
                self.assertEqual(c_fast, c_full, f"Mismatch in render_csv(view={view})")

            self.assertEqual(
                mod.render_compact(rep_fast, agg_fast, tz=tz_utc),
                mod.render_compact(rep_full, agg_full, tz=tz_utc),
            )
            self.assertEqual(
                mod.evaluate_budget_exit_code(agg_fast, tz=tz_utc, daily_usd=0.01, weekly_usd=0.01),
                mod.evaluate_budget_exit_code(agg_full, tz=tz_utc, daily_usd=0.01, weekly_usd=0.01),
            )

            # Cached IncrementalScanner(use_cache=True) must never pollute cache with scalar_only/skip_meta
            cached_scanner = mod.IncrementalScanner(agy_root, pricing_path=None, sources=[specs[0]], use_cache=True)
            _ = cached_scanner.scan(tz=tz_utc, retain_turns=False, scalar_only=True, skip_meta=True)
            rep_after = cached_scanner.scan(tz=tz_utc, retain_turns=True)
            agg_after = mod.aggregate_sessions(rep_after.sessions)
            self.assertEqual(agg_after["by_tool"], {"read_file": 1, "grep_search": 1})
            self.assertEqual(agg_after["total_thinking_turns"], 1)

    def test_agy_prefix_regex_fast_path_parity_and_fallbacks(self) -> None:
        tz_utc = dt.timezone.utc
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            agy_root = root / "antigravity-cli"
            cid = "ffff1111-0000-4000-8000-000000000001"
            logs = agy_root / "brain" / cid / ".system_generated" / "logs"
            logs.mkdir(parents=True)
            tpath = logs / "transcript.jsonl"

            lines = [
                # Day 1: standard compact with tool_calls (skipped by out_of_date under Day 2 bounds, kept for last_real_model)
                json.dumps({
                    "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-24T10:00:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 5000, "cache_read_tokens": 10000, "output_tokens": 300,
                    "tool_calls": [{"name": "run_command", "args": {"cmd": "echo hi"}}],
                }, separators=(",", ":")),
                # Day 2: standard compact with NO trailing fields (hits fast path)
                json.dumps({
                    "step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-26T10:00:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1000, "cache_read_tokens": 2000, "output_tokens": 100,
                }, separators=(",", ":")),
                # Day 2: standard compact with empty thinking (hits retain_thinking_len=False fast path -> 0)
                json.dumps({
                    "step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-26T10:01:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1100, "cache_read_tokens": 2100, "output_tokens": 110,
                    "thinking": "",
                }, separators=(",", ":")),
                # Day 2: standard compact with non-empty thinking (hits retain_thinking_len=False fast path -> 1)
                json.dumps({
                    "step_index": 4, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-26T10:02:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1200, "cache_read_tokens": 2200, "cache_write_tokens": 500, "output_tokens": 120,
                    "thinking": "step-by-step reasoning",
                }, separators=(",", ":")),
                # Day 2: content before thinking (falls back safely to json decoder)
                json.dumps({
                    "step_index": 5, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-26T10:03:00Z", "model": "gemini-4-neon-xhigh",
                    "input_tokens": 1300, "cache_read_tokens": 2300, "output_tokens": 130,
                    "content": "answer", "thinking": "hidden thought",
                }, separators=(",", ":")),
                # Day 2: cache_creation_tokens (must fall back to json decoder so cache_write is captured!)
                json.dumps({
                    "step_index": 6, "type": "PLANNER_RESPONSE", "status": "DONE",
                    "created_at": "2026-11-26T10:04:00Z", "model": "claude-sonnet-4-5-20250929",
                    "input_tokens": 1400, "cache_read_tokens": 2400, "output_tokens": 140,
                    "cache_creation_tokens": 750,
                }, separators=(",", ":")),
                # Day 2: out-of-order keys (falls back safely)
                json.dumps({
                    "model": "gemini-4-hydrogen", "output_tokens": 150, "input_tokens": 1500,
                    "cache_read_tokens": 2500, "step_index": 7, "type": "PLANNER_RESPONSE",
                    "status": "DONE", "created_at": "2026-11-26T10:05:00Z",
                }, separators=(",", ":")),
                # Day 2: CHECKPOINT with empty model inherits last_real_model
                json.dumps({
                    "step_index": 8, "type": "CHECKPOINT", "status": "DONE",
                    "created_at": "2026-11-26T10:06:00Z", "model": "",
                    "input_tokens": 2000, "cache_read_tokens": 0, "output_tokens": 100,
                }, separators=(",", ":")),
                # Malformed trailing line MUST still increment skipped_lines
                '{"step_index":9,"type":"PLANNER_RESPONSE","input_tokens":100,"broken":',
            ]
            tpath.write_text("\n".join(lines) + "\n", encoding="utf-8")

            t_full, _, _, _, _, sk_full = mod._parse_transcript_file(
                tpath, cid, scalar_only=False, skip_meta=False, retain_thinking_len=True
            )
            t_bool, _, _, _, _, sk_bool = mod._parse_transcript_file(
                tpath, cid, scalar_only=False, skip_meta=False, retain_thinking_len=False
            )
            t_scal, _, _, _, _, sk_scal = mod._parse_transcript_file(
                tpath, cid, scalar_only=True, skip_meta=True, retain_thinking_len=False
            )

            self.assertEqual(sk_full, 1)
            self.assertEqual(sk_bool, 1)
            self.assertEqual(sk_scal, 1)
            self.assertEqual(len(t_full), 8)
            self.assertEqual(len(t_bool), 8)
            self.assertEqual(len(t_scal), 8)

            # Verify cache_creation_tokens fallback captured 750 in ALL modes
            self.assertEqual(t_full[5].cache_write_tokens, 750)
            self.assertEqual(t_bool[5].cache_write_tokens, 750)
            self.assertEqual(t_scal[5].cache_write_tokens, 750)

            for a, b, c in zip(t_full, t_bool, t_scal):
                self.assertEqual((a.step_index, a.timestamp, a.model, a.step_type, a.status),
                                 (b.step_index, b.timestamp, b.model, b.step_type, b.status))
                self.assertEqual((a.step_index, a.timestamp, a.model, a.step_type, a.status),
                                 (c.step_index, c.timestamp, c.model, c.step_type, c.status))
                self.assertEqual((a.input_tokens, a.cache_read_tokens, a.cache_write_tokens, a.output_tokens),
                                 (b.input_tokens, b.cache_read_tokens, b.cache_write_tokens, b.output_tokens))
                self.assertEqual((a.input_tokens, a.cache_read_tokens, a.cache_write_tokens, a.output_tokens),
                                 (c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.output_tokens))
                self.assertEqual(a.tool_names, b.tool_names)
                self.assertEqual(a.has_thinking, b.has_thinking)

            # Day-2 filtered scan via utc_date_bounds + retain_turns=False must match retain_turns=True
            day2 = dt.date(2026, 11, 26)
            rep_d2_full = mod.scan_usage(agy_root, tz=tz_utc, since=day2, until=day2, pricing_path=None, retain_turns=True)
            rep_d2_fast = mod.scan_usage(agy_root, tz=tz_utc, since=day2, until=day2, pricing_path=None, retain_turns=False)
            self.assertEqual(rep_d2_fast.sessions[0].counts, rep_d2_full.sessions[0].counts)
            self.assertEqual(rep_d2_fast.sessions[0].thinking_turns, rep_d2_full.sessions[0].thinking_turns)
            self.assertEqual(rep_d2_fast.sessions[0].tool_counts(), rep_d2_full.sessions[0].tool_counts())

            # CLI --view pricing and --format csv fast paths work cleanly
            buf_p = io.StringIO()
            with mock.patch("sys.stdout", buf_p):
                rc_p = mod.main(["--data-dir", str(agy_root), "--view", "pricing", "--no-archive", "--utc", "--pricing", ""])
            self.assertEqual(rc_p, 0)
            self.assertIn("gemini-4-neon-xhigh", buf_p.getvalue())

            buf_c = io.StringIO()
            with mock.patch("sys.stdout", buf_c):
                rc_c = mod.main(["--data-dir", str(agy_root), "--format", "csv", "--view", "model", "--no-archive", "--utc", "--pricing", ""])
            self.assertEqual(rc_c, 0)
            self.assertIn("gemini-4-neon-xhigh", buf_c.getvalue())

    def test_explicit_slots_classes_and_lazy_imports(self) -> None:
        spec = mod.SourceSpec("antigravity", pathlib.Path("/tmp/a"), "cli")
        self.assertFalse(hasattr(spec, "__dict__"))
        self.assertEqual(spec, mod.SourceSpec("antigravity", pathlib.Path("/tmp/a"), "cli"))
        self.assertEqual(hash(spec), hash(mod.SourceSpec("antigravity", pathlib.Path("/tmp/a"), "cli")))
        with self.assertRaises(AttributeError):
            spec.kind = "claude"  # type: ignore[misc]

        rate = mod.ModelRate(3.0, 0.3, 3.75, 15.0)
        self.assertFalse(hasattr(rate, "__dict__"))
        self.assertEqual(rate, mod.ModelRate(3.0, 0.3, 3.75, 15.0))
        self.assertEqual(hash(rate), hash(mod.ModelRate(3.0, 0.3, 3.75, 15.0)))
        with self.assertRaises(AttributeError):
            rate.input = 5.0  # type: ignore[misc]

        cb = rate.calculate(1000, 2000, 500, 300, cache_write_1h_tokens=200)
        self.assertFalse(hasattr(cb, "__dict__"))
        tc_std = mod.TokenCounts()
        tc_std.add_call(1000, 2000, 500, 300, cb)
        tc_inl = mod.TokenCounts()
        tc_inl.add_call_inline(
            1000, 2000, 500, 300, 3500,
            cb.input_usd, cb.cache_read_usd, cb.cache_write_usd, cb.output_usd, cb.cache_saved_usd, cb.unpriced_calls,
        )
        self.assertFalse(hasattr(tc_std, "__dict__"))
        self.assertEqual(tc_inl, tc_std)

        tr = mod.TurnRecord("c1", 1, None, "m1", "PLANNER_RESPONSE", "DONE", 10, 20, 0, 5)
        self.assertFalse(hasattr(tr, "__dict__"))
        si = mod.SessionInfo("c1")
        self.assertFalse(hasattr(si, "__dict__"))
        sr = mod.ScanReport(pathlib.Path("."), [], [])
        self.assertFalse(hasattr(sr, "__dict__"))

        # Verify lazy-imported argparse / unicodedata paths
        import argparse

        with self.assertRaises(argparse.ArgumentTypeError):
            mod._parse_date_arg("not-a-date")
        self.assertEqual(mod._display_width("中文ab"), 6)
        self.assertEqual(mod._truncate_display("中文测试标题", 7), "中文测…")

    def test_no_file_writes_in_cli_source(self) -> None:
        base_dir = pathlib.Path(mod.__file__).parent
        forbidden_imports = {
            "sqlite3",
            "subprocess",
            "urllib.request",
            "socket",
            "http",
            "http.client",
            "ftplib",
            "smtplib",
            "ssl",
            "shutil",
        }
        forbidden_attrs = {
            "write_text",
            "write_bytes",
            "unlink",
            "rmdir",
            "mkdir",
            "makedirs",
            "remove",
            "rename",
            "replace_file",
        }

        for filename in ("gauge_stats.py",):
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
