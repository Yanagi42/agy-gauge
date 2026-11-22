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
