#!/usr/bin/env python3
"""Strictly read-only multi-agent token usage analyzer for Gauge.

Reads only whitelisted plain-text local transcript and metadata files:
  - Antigravity (~/.gemini/antigravity{,-cli,-ide}):
      brain/<conversation-id>/.system_generated/logs/transcript.jsonl
      (falls back to transcript_full.jsonl only if transcript.jsonl is absent),
      annotations/<conversation-id>.pbtxt, cache/conversation_metadata.json
  - Claude Code ($CLAUDE_CONFIG_DIR or ~/.claude, ~/.config/claude):
      projects/<proj>/<uuid>.jsonl, projects/<proj>/<uuid>/subagents/agent-*.jsonl,
      optional sessions-index.json and agent-*.meta.json
  - OpenAI Codex CLI ($CODEX_HOME or ~/.codex):
      sessions/YYYY/MM/DD/rollout-*.jsonl, archived_sessions/rollout-*.jsonl,
      optional session_index.jsonl
  - Gemini CLI ($GEMINI_CLI_HOME or ~/.gemini):
      tmp/<proj>/chats/session-*.{json,jsonl}, optional .project_root and projects.json

Safety guarantees:
  - Strictly read-only: performs zero file writes, creates no temp/cache files.
  - Never opens SQLite databases (avoids WAL/SHM creation or lock contention
    with live Antigravity / Codex CLI processes).
  - Never reads credentials, OAuth tokens, auth/settings files, history, or network endpoints.
  - Rejects symlinks and verifies canonical root containment before every read.
  - Never invokes subprocesses; uses only the Python standard library.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import io
import json
import os
import pathlib
import re
import sys
import time
import unicodedata
import urllib.parse
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Any, Sequence


DEFAULT_DATA_DIR = pathlib.Path.home() / ".gemini" / "antigravity-cli"
DEFAULT_PRICING_PATH = pathlib.Path(__file__).resolve().parent / "pricing.json"
DEFAULT_STATUSLINE_PATH = pathlib.Path(__file__).resolve().parent / "statusline_state.json"
DEFAULT_ARCHIVE_PATH = pathlib.Path(__file__).resolve().parent / "archive.json"
DEFAULT_BUDGET_PATH = pathlib.Path(__file__).resolve().parent / "budget.json"

TIER1_AGENTS: tuple[str, ...] = ("antigravity", "claude", "codex", "gemini")

_AGENT_ALIASES: dict[str, str] = {
    "agy": "antigravity",
    "antigravity": "antigravity",
    "antigravity-cli": "antigravity",
    "antigravity-app": "antigravity",
    "antigravity-ide": "antigravity",
    "cli": "antigravity",
    "app": "antigravity",
    "ide": "antigravity",
    "claude": "claude",
    "claude-code": "claude",
    "claude_code": "claude",
    "cc": "claude",
    "codex": "codex",
    "codex-cli": "codex",
    "codex_cli": "codex",
    "openai": "codex",
    "gemini": "gemini",
    "gemini-cli": "gemini",
    "gemini_cli": "gemini",
}


@dataclass(frozen=True)
class SourceSpec:
    kind: str  # "antigravity" | "claude" | "codex" | "gemini"
    root: pathlib.Path
    source_name: str  # "cli" | "app" | "ide" | "custom" | "claude" | "codex" | "gemini"


def _infer_source_name(path: pathlib.Path) -> str:
    low = path.name.lower()
    if low == "antigravity-cli":
        return "cli"
    if low == "antigravity":
        return "app"
    if low == "antigravity-ide":
        return "ide"
    return "custom"


def _normalize_source_name(source: str) -> str:
    low = source.strip().lower()
    if low in ("cli", "antigravity-cli"):
        return "cli"
    if low in ("app", "antigravity"):
        return "app"
    if low in ("ide", "antigravity-ide"):
        return "ide"
    if low in ("claude", "claude-code", "claude_code"):
        return "claude"
    if low in ("codex", "codex-cli", "codex_cli"):
        return "codex"
    if low in ("gemini", "gemini-cli", "gemini_cli"):
        return "gemini"
    return "custom"


def source_to_agent_family(source: str) -> str:
    low = source.strip().lower()
    if low in ("claude", "claude-code", "claude_code"):
        return "claude"
    if low in ("codex", "codex-cli", "codex_cli"):
        return "codex"
    if low in ("gemini", "gemini-cli", "gemini_cli"):
        return "gemini"
    return "antigravity"


def source_badge(source: str) -> str:
    low = source.strip().lower()
    return {
        "cli": "AGY",
        "app": "App",
        "ide": "IDE",
        "custom": "Custom",
        "claude": "Claude",
        "codex": "Codex",
        "gemini": "Gemini",
    }.get(low, source or "AGY")


def source_display_label(source: str) -> str:
    low = source.strip().lower()
    return {
        "cli": "Antigravity CLI",
        "app": "Antigravity App",
        "ide": "Antigravity IDE",
        "custom": "Antigravity Custom",
        "claude": "Claude Code",
        "codex": "Codex CLI",
        "gemini": "Gemini CLI",
    }.get(low, source or "Antigravity")


def agent_display_label(family: str) -> str:
    return {
        "antigravity": "Antigravity",
        "claude": "Claude Code",
        "codex": "Codex CLI",
        "gemini": "Gemini CLI",
    }.get(family, family)


def parse_agent_filter(raw_values: Sequence[str] | None) -> set[str] | None:
    if not raw_values:
        return None
    selected: set[str] = set()
    for raw in raw_values:
        for tok in raw.split(","):
            key = tok.strip().lower()
            if not key:
                continue
            if key == "all":
                selected.update(TIER1_AGENTS)
                continue
            mapped = _AGENT_ALIASES.get(key)
            if mapped is None:
                raise ValueError(
                    f"Unknown agent '{tok.strip()}'. Expected one of: agy, antigravity, claude, codex, gemini, all."
                )
            selected.add(mapped)
    return selected or None


def _dedup_paths(paths: Sequence[pathlib.Path]) -> list[pathlib.Path]:
    out: list[pathlib.Path] = []
    seen: set[str] = set()
    for p in paths:
        try:
            key = str(p.resolve(strict=False)).lower()
        except OSError:
            key = str(p).lower()
        if key not in seen:
            seen.add(key)
            out.append(p)
    return out


def default_data_dirs() -> list[pathlib.Path]:
    base = pathlib.Path.home() / ".gemini"
    candidates = [
        base / "antigravity-cli",
        base / "antigravity",
        base / "antigravity-ide",
    ]
    out: list[pathlib.Path] = []
    for idx, p in enumerate(candidates):
        if idx == 0 or (p / "brain").is_dir():
            out.append(p)
    return out


def discover_claude_dirs() -> list[pathlib.Path]:
    env_val = os.environ.get("CLAUDE_CONFIG_DIR", "").strip()
    if env_val:
        candidates = [pathlib.Path(part.strip()).expanduser() for part in env_val.split(",") if part.strip()]
    else:
        home = pathlib.Path.home()
        candidates = [home / ".claude", home / ".config" / "claude"]
    out: list[pathlib.Path] = []
    for p in _dedup_paths(candidates):
        if not p.is_symlink() and (p / "projects").is_dir() and not (p / "projects").is_symlink():
            out.append(p)
    return out


def discover_codex_dirs() -> list[pathlib.Path]:
    env_val = os.environ.get("CODEX_HOME", "").strip()
    root = pathlib.Path(env_val).expanduser() if env_val else (pathlib.Path.home() / ".codex")
    if root.is_symlink():
        return []
    has_sessions = (root / "sessions").is_dir() and not (root / "sessions").is_symlink()
    has_archived = (root / "archived_sessions").is_dir() and not (root / "archived_sessions").is_symlink()
    return [root] if (has_sessions or has_archived) else []


def discover_gemini_dirs() -> list[pathlib.Path]:
    env_val = os.environ.get("GEMINI_CLI_HOME", "").strip()
    if env_val:
        base = pathlib.Path(env_val).expanduser()
        candidates = [base / "tmp", base / ".gemini" / "tmp"]
    else:
        candidates = [pathlib.Path.home() / ".gemini" / "tmp"]
    out: list[pathlib.Path] = []
    for p in _dedup_paths(candidates):
        if not p.is_symlink() and p.is_dir():
            out.append(p)
    return out


def discover_default_sources(agents: set[str] | None = None) -> list[SourceSpec]:
    allowed = set(TIER1_AGENTS) if agents is None else set(agents)
    sources: list[SourceSpec] = []
    if "antigravity" in allowed:
        for p in default_data_dirs():
            if not p.is_symlink() and (p / "brain").is_dir():
                sources.append(SourceSpec("antigravity", p, _infer_source_name(p)))
    if "claude" in allowed:
        for p in discover_claude_dirs():
            sources.append(SourceSpec("claude", p, "claude"))
    if "codex" in allowed:
        for p in discover_codex_dirs():
            sources.append(SourceSpec("codex", p, "codex"))
    if "gemini" in allowed:
        for p in discover_gemini_dirs():
            sources.append(SourceSpec("gemini", p, "gemini"))
    if not sources and "antigravity" in allowed:
        sources.append(SourceSpec("antigravity", DEFAULT_DATA_DIR, "cli"))
    return sources

_USER_REQUEST_RE = re.compile(r"<USER_REQUEST>\s*(.*?)\s*</USER_REQUEST>", re.DOTALL)
_PBTXT_TITLE_RE = re.compile(r'(?m)^\s*title\s*:\s*"((?:\\.|[^"\\])*)"')
_PBTXT_ESCAPE_RE = re.compile(r"\\([0-7]{1,3}|x[0-9a-fA-F]{2}|[abfnrtv\\\"'?])")
_UUID_RE = re.compile(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")

_EFFORT_SUFFIXES = ("-xhigh", "-high", "-medium", "-med", "-low", "-minimal", "-max", "-none")
_CARBON_STD_DATE = dt.date(2026, 12, 1)
_FLASH38_STD_DATE = dt.date(2027, 1, 1)


@dataclass(frozen=True)
class ModelRate:
    """USD per 1,000,000 tokens."""

    input: float
    cache_read: float
    cache_write: float
    output: float

    def calculate(
        self,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        cache_write_1h_tokens: int = 0,
    ) -> "CostBreakdown":
        per_m = 1_000_000.0
        in_usd = (input_tokens / per_m) * self.input
        cr_usd = (cache_read_tokens / per_m) * self.cache_read
        cw_1h = min(max(0, cache_write_1h_tokens), max(0, cache_write_tokens))
        cw_5m = max(0, cache_write_tokens - cw_1h)
        cw_usd = (cw_5m / per_m) * self.cache_write + (cw_1h / per_m) * (self.input * 2.0)
        out_usd = (output_tokens / per_m) * self.output
        saved_usd = (
            (cache_read_tokens / per_m) * (self.input - self.cache_read)
            if self.input > self.cache_read
            else 0.0
        )
        return CostBreakdown(
            input_usd=in_usd,
            cache_read_usd=cr_usd,
            cache_write_usd=cw_usd,
            output_usd=out_usd,
            cache_saved_usd=saved_usd,
            unpriced_calls=0,
        )


@dataclass
class CostBreakdown:
    input_usd: float = 0.0
    cache_read_usd: float = 0.0
    cache_write_usd: float = 0.0
    output_usd: float = 0.0
    cache_saved_usd: float = 0.0
    unpriced_calls: int = 0

    @property
    def total_usd(self) -> float:
        return self.input_usd + self.cache_read_usd + self.cache_write_usd + self.output_usd

    @property
    def non_cached_usd(self) -> float:
        return self.input_usd + self.cache_write_usd + self.output_usd

    @property
    def input_plus_write_usd(self) -> float:
        return self.input_usd + self.cache_write_usd

    def merge(self, other: "CostBreakdown") -> None:
        self.input_usd += other.input_usd
        self.cache_read_usd += other.cache_read_usd
        self.cache_write_usd += other.cache_write_usd
        self.output_usd += other.output_usd
        self.cache_saved_usd += other.cache_saved_usd
        self.unpriced_calls += other.unpriced_calls

    def is_fully_unpriced(self, calls: int) -> bool:
        return calls > 0 and self.unpriced_calls >= calls


_PROVIDER_PREFIXES = (
    "us.anthropic.",
    "eu.anthropic.",
    "global.anthropic.",
    "anthropic.",
)


def canonical_exact_key(raw: str) -> str:
    s = raw.strip().lower()
    if "/" in s:
        s = s.rsplit("/", 1)[1]
    for pfx in _PROVIDER_PREFIXES:
        if s.startswith(pfx):
            s = s[len(pfx) :]
            break
    return s.replace("@", "-").replace(".", "-").replace("_", "-")


def normalize_model_key(raw: str) -> str:
    s = canonical_exact_key(raw)
    for suffix in _EFFORT_SUFFIXES:
        if s.endswith(suffix) and len(s) > len(suffix):
            s = s[: -len(suffix)]
            break
    return s


def _matches_model_prefix(norm: str, prefix: str) -> bool:
    return norm == prefix or norm.startswith(prefix + "-")


_BUILTIN_SPECS: tuple[tuple[str, tuple[str, ...], ModelRate, int | None, ModelRate | None, dt.date | None, ModelRate | None, str], ...] = (
    ("gemini-4-neon", ("gemini-4-neon",), ModelRate(3.00, 0.15, 3.00, 15.00), None, None, None, None, "1M flat"),
    ("gemini-4-hydrogen", ("gemini-4-hydrogen",), ModelRate(1.50, 0.075, 1.50, 7.50), None, None, None, None, ""),
    ("gemini-4-carbon", ("gemini-4-carbon", "gemini-4-argon"), ModelRate(4.00, 0.20, 4.00, 20.00), None, None, _CARBON_STD_DATE, ModelRate(2.00, 0.10, 2.00, 10.00), "intro $2/$10 before 2026-12-01"),
    ("gemini-3.8-flash", ("gemini-3-8-flash",), ModelRate(1.50, 0.15, 1.50, 7.50), None, None, _FLASH38_STD_DATE, ModelRate(0.75, 0.075, 0.75, 3.75), "intro $0.75/$3.75 in 2026"),
    ("gemini-3.5-flash-lite", ("gemini-3-5-flash-lite", "gemini-2-5-flash-lite"), ModelRate(0.10, 0.025, 0.10, 0.40), None, None, None, None, ""),
    ("gemini-3.1-pro", ("gemini-3-1-pro", "gemini-3-0-pro", "gemini-3-pro"), ModelRate(2.00, 0.20, 2.00, 12.00), 200_000, ModelRate(4.00, 0.40, 4.00, 18.00), None, None, ">200K $4/$18"),
    ("gemini-3-flash", ("gemini-3-flash", "gemini-3-0-flash"), ModelRate(0.50, 0.05, 0.50, 3.00), None, None, None, None, ""),
    ("gemini-2.5-pro", ("gemini-2-5-pro",), ModelRate(1.25, 0.125, 1.25, 10.00), 200_000, ModelRate(2.50, 0.25, 2.50, 15.00), None, None, ">200K $2.5/$15"),
    ("gemini-2.5-flash", ("gemini-2-5-flash",), ModelRate(0.30, 0.03, 0.30, 2.50), None, None, None, None, ""),
    ("gemini-2.0-flash-lite", ("gemini-2-0-flash-lite",), ModelRate(0.075, 0.01875, 0.075, 0.30), None, None, None, None, ""),
    ("gemini-2.0-flash", ("gemini-2-0-flash",), ModelRate(0.10, 0.025, 0.10, 0.40), None, None, None, None, ""),
    ("claude-opus-5-5", ("claude-opus-5-5",), ModelRate(4.00, 0.20, 5.00, 20.00), None, None, None, None, "1M flat"),
    ("claude-sonnet-5-5", ("claude-sonnet-5-5",), ModelRate(2.00, 0.10, 2.50, 10.00), None, None, None, None, ""),
    ("claude-haiku-5-5", ("claude-haiku-5-5",), ModelRate(0.10, 0.01, 0.125, 0.50), 100_000, ModelRate(0.50, 0.05, 0.625, 2.50), None, None, ">100K $0.5/$2.5"),
    ("claude-fable-5-2", ("claude-fable-5-2", "claude-mythos-5-2"), ModelRate(8.00, 0.20, 10.00, 40.00), None, None, None, None, ""),
    ("claude-opus-5", ("claude-opus-5",), ModelRate(5.00, 0.50, 6.25, 25.00), None, None, None, None, ""),
    ("claude-sonnet-5", ("claude-sonnet-5",), ModelRate(2.00, 0.20, 2.50, 10.00), None, None, None, None, ""),
    ("claude-opus-4-6", ("claude-opus-4-6", "claude-opus-4-5"), ModelRate(5.00, 0.50, 6.25, 25.00), None, None, None, None, ""),
    ("claude-opus-4", ("claude-opus-4",), ModelRate(15.00, 1.50, 18.75, 75.00), None, None, None, None, ""),
    ("claude-sonnet-4", ("claude-sonnet-4", "claude-3-7-sonnet", "claude-3-5-sonnet"), ModelRate(3.00, 0.30, 3.75, 15.00), None, None, None, None, ""),
    ("claude-haiku-4-5", ("claude-haiku-4-5",), ModelRate(1.00, 0.10, 1.25, 5.00), None, None, None, None, ""),
    ("claude-3-5-haiku", ("claude-3-5-haiku",), ModelRate(0.80, 0.08, 1.00, 4.00), None, None, None, None, ""),
    # OpenAI / Codex models — ordered most-specific before shorter prefixes (e.g. before "gpt-5")
    ("gpt-5.4", ("gpt-5-4",), ModelRate(2.50, 0.25, 2.50, 15.00), None, None, None, None, ""),
    ("gpt-5.3-codex", ("gpt-5-3-codex",), ModelRate(1.75, 0.175, 1.75, 14.00), None, None, None, None, ""),
    ("gpt-5.2-codex", ("gpt-5-2-codex",), ModelRate(1.75, 0.175, 1.75, 14.00), None, None, None, None, ""),
    ("gpt-5.2", ("gpt-5-2",), ModelRate(1.75, 0.175, 1.75, 14.00), None, None, None, None, ""),
    ("gpt-5.1-codex-mini", ("gpt-5-1-codex-mini",), ModelRate(0.25, 0.025, 0.25, 2.00), None, None, None, None, ""),
    ("gpt-5.1-codex", ("gpt-5-1-codex",), ModelRate(1.25, 0.125, 1.25, 10.00), None, None, None, None, ""),
    ("gpt-5.1", ("gpt-5-1",), ModelRate(1.25, 0.125, 1.25, 10.00), None, None, None, None, ""),
    ("gpt-5-codex", ("gpt-5-codex",), ModelRate(1.25, 0.125, 1.25, 10.00), None, None, None, None, ""),
    ("gpt-5-mini", ("gpt-5-mini",), ModelRate(0.25, 0.025, 0.25, 2.00), None, None, None, None, ""),
    ("gpt-5-nano", ("gpt-5-nano",), ModelRate(0.05, 0.005, 0.05, 0.40), None, None, None, None, ""),
    ("gpt-5", ("gpt-5",), ModelRate(1.25, 0.125, 1.25, 10.00), None, None, None, None, ""),
    ("codex-mini-latest", ("codex-mini-latest", "codex-mini"), ModelRate(1.50, 0.375, 1.50, 6.00), None, None, None, None, ""),
    ("gpt-4.1-mini", ("gpt-4-1-mini",), ModelRate(0.40, 0.10, 0.40, 1.60), None, None, None, None, ""),
    ("gpt-4.1-nano", ("gpt-4-1-nano",), ModelRate(0.10, 0.025, 0.10, 0.40), None, None, None, None, ""),
    ("gpt-4.1", ("gpt-4-1",), ModelRate(2.00, 0.50, 2.00, 8.00), None, None, None, None, ""),
    ("gpt-4o-mini", ("gpt-4o-mini",), ModelRate(0.15, 0.075, 0.15, 0.60), None, None, None, None, ""),
    ("gpt-4o", ("gpt-4o",), ModelRate(2.50, 1.25, 2.50, 10.00), None, None, None, None, ""),
    ("o3-mini", ("o3-mini", "o4-mini"), ModelRate(1.10, 0.55, 1.10, 4.40), None, None, None, None, ""),
    ("o3", ("o3",), ModelRate(2.00, 0.50, 2.00, 8.00), None, None, None, None, ""),
)


def _builtin_detail(norm: str, ctx: int, date_obj: dt.date | None) -> tuple[ModelRate, str, str] | None:
    for display, prefixes, base, th, lr, intro_before, intro_rate, note in _BUILTIN_SPECS:
        if any(_matches_model_prefix(norm, p) for p in prefixes):
            if intro_before is not None and intro_rate is not None and date_obj is not None and date_obj < intro_before:
                return intro_rate, display, note
            if th is not None and lr is not None and ctx > th:
                return lr, display, note
            return base, display, note
    return None


def _parse_rate_dict(obj: dict[str, Any]) -> ModelRate | None:
    raw_in = obj.get("input")
    raw_out = obj.get("output")
    if isinstance(raw_in, bool) or isinstance(raw_out, bool):
        return None
    if not isinstance(raw_in, (int, float)) or not isinstance(raw_out, (int, float)):
        return None
    if raw_in < 0 or raw_out < 0:
        return None
    raw_cr = obj.get("cache_read", 0.0)
    raw_cw = obj.get("cache_write", raw_in)
    cr = float(raw_cr) if isinstance(raw_cr, (int, float)) and not isinstance(raw_cr, bool) and raw_cr >= 0 else 0.0
    cw = float(raw_cw) if isinstance(raw_cw, (int, float)) and not isinstance(raw_cw, bool) and raw_cw >= 0 else float(raw_in)
    return ModelRate(float(raw_in), cr, cw, float(raw_out))


class PricingCatalog:
    """Strictly read-only pricing catalog with optional local pricing.json overrides."""

    def __init__(self, override_path: pathlib.Path | None = DEFAULT_PRICING_PATH) -> None:
        self.override_path = override_path
        self._override_sig: tuple[int, int] | None = None
        # (raw_key, exact_key, prefix, has_effort, base, long_th, long_rate)
        self._custom_rules: list[tuple[str, str, str, bool, ModelRate, int | None, ModelRate | None]] = []
        self.refresh()

    def refresh(self) -> bool:
        path = self.override_path
        if path is None or path.is_symlink() or not path.is_file():
            changed = self._override_sig is not None or bool(self._custom_rules)
            self._override_sig = None
            self._custom_rules = []
            return changed
        try:
            st = path.stat()
            sig = (st.st_mtime_ns, st.st_size)
        except OSError:
            return False
        if sig == self._override_sig:
            return False
        self._override_sig = sig
        self._custom_rules = self._load_rules(path)
        return True

    @staticmethod
    def _load_rules(path: pathlib.Path) -> list[tuple[str, str, str, bool, ModelRate, int | None, ModelRate | None]]:
        try:
            with path.open("r", encoding="utf-8", errors="replace") as fh:
                payload = json.load(fh)
        except (OSError, json.JSONDecodeError):
            return []
        if not isinstance(payload, dict):
            return []
        models = payload.get("models") if isinstance(payload.get("models"), dict) else payload
        rules: list[tuple[str, str, str, bool, ModelRate, int | None, ModelRate | None]] = []
        for k, v in models.items():
            if not isinstance(v, dict):
                continue
            base = _parse_rate_dict(v)
            if base is None:
                continue
            raw_key = str(k).strip()
            exact_key = canonical_exact_key(raw_key)
            prefix = normalize_model_key(raw_key)
            if not prefix:
                continue
            has_effort = exact_key != prefix
            raw_th = v.get("long_context_threshold")
            th = int(raw_th) if isinstance(raw_th, int) and not isinstance(raw_th, bool) and raw_th > 0 else None
            lc = v.get("long_context")
            lr = _parse_rate_dict(lc) if isinstance(lc, dict) else None
            rules.append((raw_key, exact_key, prefix, has_effort, base, th, lr))
        return rules

    def resolve_detail(
        self, model: str, total_input_tokens: int, date_obj: dt.date | None
    ) -> tuple[ModelRate, str, str] | None:
        exact = canonical_exact_key(model)
        norm = normalize_model_key(model)
        if not norm or norm in ("unknown", "compaction") or model == "(上下文压缩)":
            return None
        for raw_key, exact_key, _, _, base, th, lr in self._custom_rules:
            if exact_key == exact:
                r = lr if (th is not None and lr is not None and total_input_tokens > th) else base
                return r, "custom(exact)", raw_key
        best: tuple[str, str, str, bool, ModelRate, int | None, ModelRate | None] | None = None
        for rule in self._custom_rules:
            if rule[3]:  # effort-specific custom rule does not match other effort levels
                continue
            if _matches_model_prefix(norm, rule[2]):
                if best is None or len(rule[2]) > len(best[2]):
                    best = rule
        if best is not None:
            raw_key, _, _, _, base, th, lr = best
            r = lr if (th is not None and lr is not None and total_input_tokens > th) else base
            return r, "custom(prefix)", raw_key
        bi = _builtin_detail(norm, total_input_tokens, date_obj)
        if bi is not None:
            return bi[0], "builtin", bi[1]
        return None

    def resolve_rate(self, model: str, total_input_tokens: int, date_obj: dt.date | None) -> ModelRate | None:
        det = self.resolve_detail(model, total_input_tokens, date_obj)
        return det[0] if det is not None else None

    def build_table(self, observed_models: Sequence[str], date_obj: dt.date) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        seen: set[str] = set()
        for m in observed_models:
            ex = canonical_exact_key(m)
            if not ex or ex in ("unknown", "compaction") or ex in seen:
                continue
            seen.add(ex)
            det = self.resolve_detail(m, 0, date_obj)
            if det is not None:
                r, src, rule = det
                rows.append({"model": m, "source": src, "rule": rule, "rate": r, "reference": False})
            else:
                rows.append({"model": m, "source": "unpriced", "rule": "-", "rate": None, "reference": False})
        for raw_key, exact_key, _, has_eff, base, _, _ in sorted(self._custom_rules, key=lambda x: x[0]):
            if exact_key not in seen:
                seen.add(exact_key)
                rows.append({
                    "model": raw_key,
                    "source": "custom(exact)" if has_eff else "custom(prefix)",
                    "rule": raw_key,
                    "rate": base,
                    "reference": False,
                })
        for display, _, _, _, _, _, _, _ in _BUILTIN_SPECS:
            ex = canonical_exact_key(display)
            if ex in seen:
                continue
            seen.add(ex)
            det = self.resolve_detail(display, 0, date_obj)
            r = det[0] if det else None
            src = det[1] if det else "builtin"
            rule = det[2] if det else display
            rows.append({"model": display, "source": src, "rule": rule, "rate": r, "reference": True})
        return rows

    def price_turn(
        self,
        model: str,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        date_obj: dt.date | None,
        cache_write_1h_tokens: int = 0,
    ) -> CostBreakdown:
        ctx = input_tokens + cache_read_tokens + cache_write_tokens
        rate = self.resolve_rate(model, ctx, date_obj)
        if rate is None:
            return CostBreakdown(unpriced_calls=1)
        return rate.calculate(
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            cache_write_1h_tokens=cache_write_1h_tokens,
        )

    def price_turn_with_ttl(
        self,
        model: str,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        cache_write_1h_tokens: int,
        output_tokens: int,
        date_obj: dt.date | None,
    ) -> CostBreakdown:
        return self.price_turn(
            model,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            date_obj,
            cache_write_1h_tokens=cache_write_1h_tokens,
        )


@dataclass
class TokenCounts:
    calls: int = 0
    input_tokens: int = 0  # Uncached new input tokens
    cache_read_tokens: int = 0  # Prompt cache hit input tokens
    cache_write_tokens: int = 0  # Prompt cache creation tokens (if reported)
    output_tokens: int = 0  # Output tokens (thinking + response + tool calls)
    peak_context: int = 0  # Max single-call total input tokens
    cost: CostBreakdown = field(default_factory=CostBreakdown)

    @property
    def total_input_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens

    @property
    def total_tokens(self) -> int:
        return self.total_input_tokens + self.output_tokens

    @property
    def non_cached_tokens(self) -> int:
        return self.input_tokens + self.cache_write_tokens + self.output_tokens

    @property
    def cache_hit_rate(self) -> float:
        denom = self.total_input_tokens
        if denom <= 0:
            return 0.0
        return (self.cache_read_tokens / denom) * 100.0

    @property
    def total_cost_usd(self) -> float:
        return self.cost.total_usd

    def add_call(
        self,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        cost: CostBreakdown | None = None,
    ) -> None:
        self.calls += 1
        self.input_tokens += input_tokens
        self.cache_read_tokens += cache_read_tokens
        self.cache_write_tokens += cache_write_tokens
        self.output_tokens += output_tokens
        if cost is not None:
            self.cost.merge(cost)
        ctx = input_tokens + cache_read_tokens + cache_write_tokens
        if ctx > self.peak_context:
            self.peak_context = ctx

    def merge(self, other: "TokenCounts") -> None:
        self.calls += other.calls
        self.input_tokens += other.input_tokens
        self.cache_read_tokens += other.cache_read_tokens
        self.cache_write_tokens += other.cache_write_tokens
        self.output_tokens += other.output_tokens
        self.cost.merge(other.cost)
        if other.peak_context > self.peak_context:
            self.peak_context = other.peak_context

    def to_dict(self) -> dict[str, Any]:
        return {
            "calls": self.calls,
            "input_tokens": self.input_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "total_input_tokens": self.total_input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "non_cached_tokens": self.non_cached_tokens,
            "cache_hit_rate_pct": round(self.cache_hit_rate, 2),
            "peak_context": self.peak_context,
            "cost_usd": round(self.cost.total_usd, 6),
            "cost_input_usd": round(self.cost.input_usd, 6),
            "cost_cache_read_usd": round(self.cost.cache_read_usd, 6),
            "cost_cache_write_usd": round(self.cost.cache_write_usd, 6),
            "cost_output_usd": round(self.cost.output_usd, 6),
            "cache_saved_usd": round(self.cost.cache_saved_usd, 6),
            "unpriced_calls": self.cost.unpriced_calls,
        }


@dataclass
class TurnRecord:
    conversation_id: str
    step_index: int
    timestamp: dt.datetime | None
    model: str
    step_type: str
    status: str
    input_tokens: int
    cache_read_tokens: int
    cache_write_tokens: int
    output_tokens: int
    tool_names: tuple[str, ...] = ()
    thinking_chars: int = 0
    reasoning_tokens: int = 0
    cache_write_1h_tokens: int = 0
    cost: CostBreakdown = field(default_factory=CostBreakdown)

    @property
    def total_input_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens

    @property
    def total_tokens(self) -> int:
        return self.total_input_tokens + self.output_tokens

    @property
    def cache_hit_rate(self) -> float:
        denom = self.total_input_tokens
        return (self.cache_read_tokens / denom) * 100.0 if denom > 0 else 0.0

    @property
    def has_thinking(self) -> bool:
        return self.thinking_chars > 0 or self.reasoning_tokens > 0


@dataclass
class SessionInfo:
    conversation_id: str
    title: str = ""
    workspace: str = ""
    transcript_path: str = ""
    source: str = "cli"
    is_archived: bool = False
    is_subagent: bool = False
    parent_id: str | None = None
    parent_title: str = ""
    turns: list[TurnRecord] = field(default_factory=list)
    counts: TokenCounts = field(default_factory=TokenCounts)
    models: set[str] = field(default_factory=set)
    first_time: dt.datetime | None = None
    last_time: dt.datetime | None = None

    @property
    def agent_family(self) -> str:
        return source_to_agent_family(self.source)

    @property
    def active_duration_secs(self) -> int:
        times = sorted(t.timestamp for t in self.turns if t.timestamp is not None)
        if not times:
            return 0
        if len(times) == 1:
            return 15
        secs = 0
        for a, b in zip(times, times[1:]):
            d = int((b - a).total_seconds())
            if d > 0:
                secs += min(d, 600)
        return max(secs, 15)

    @property
    def thinking_turns(self) -> int:
        return sum(1 for t in self.turns if t.has_thinking)

    def tool_counts(self) -> dict[str, int]:
        counts: dict[str, int] = defaultdict(int)
        for t in self.turns:
            for name in t.tool_names:
                counts[name] += 1
        return dict(counts)

    def add_turn(self, turn: TurnRecord) -> None:
        self.turns.append(turn)
        self.counts.add_call(
            turn.input_tokens,
            turn.cache_read_tokens,
            turn.cache_write_tokens,
            turn.output_tokens,
            turn.cost,
        )
        if turn.model:
            self.models.add(turn.model)
        if turn.timestamp is not None:
            if self.first_time is None or turn.timestamp < self.first_time:
                self.first_time = turn.timestamp
            if self.last_time is None or turn.timestamp > self.last_time:
                self.last_time = turn.timestamp


@dataclass
class ScanReport:
    data_dir: pathlib.Path
    sessions: list[SessionInfo]
    warnings: list[str]
    data_dirs: list[pathlib.Path] = field(default_factory=list)
    archived_sessions_count: int = 0
    skipped_empty_sessions: int = 0
    skipped_lines: int = 0
    scan_ms: float = 0.0
    files_cached: int = 0
    files_reparsed: int = 0


def iso_week_key(date_obj: dt.date) -> str:
    iso = date_obj.isocalendar()
    return f"{iso.year}-W{iso.week:02d}"


def iso_week_bounds(week_key: str) -> tuple[dt.date, dt.date] | None:
    m = re.match(r"^(\d{4})-W(\d{2})$", week_key)
    if not m:
        return None
    year, week = int(m.group(1)), int(m.group(2))
    try:
        mon = dt.date.fromisocalendar(year, week, 1)
        sun = dt.date.fromisocalendar(year, week, 7)
        return mon, sun
    except ValueError:
        return None


def format_week_label(week_key: str) -> str:
    bounds = iso_week_bounds(week_key)
    if not bounds:
        return week_key
    mon, sun = bounds
    return f"{week_key} ({mon.strftime('%m/%d')}-{sun.strftime('%m/%d')})"


def _configure_stdio() -> None:
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            try:
                reconfigure(encoding="utf-8", errors="replace")
            except Exception:
                pass


def _is_within_dir(path: pathlib.Path, root: pathlib.Path) -> bool:
    try:
        resolved_path = path.resolve(strict=True)
        resolved_root = root.resolve(strict=True)
        resolved_path.relative_to(resolved_root)
        return True
    except (OSError, ValueError):
        return False


def _safe_read_text(path: pathlib.Path, root: pathlib.Path) -> str | None:
    if path.is_symlink() or not path.is_file():
        return None
    if not _is_within_dir(path, root):
        return None
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


def _unescape_pbtxt_string(raw: str) -> str:
    """Decode protobuf text-format string escapes (including octal UTF-8 bytes)."""
    simple = {
        "a": b"\a",
        "b": b"\b",
        "f": b"\f",
        "n": b"\n",
        "r": b"\r",
        "t": b"\t",
        "v": b"\v",
        "\\": b"\\",
        '"': b'"',
        "'": b"'",
        "?": b"?",
    }
    out = bytearray()
    pos = 0
    for match in _PBTXT_ESCAPE_RE.finditer(raw):
        if match.start() > pos:
            out.extend(raw[pos : match.start()].encode("utf-8"))
        esc = match.group(1)
        if esc in simple:
            out.extend(simple[esc])
        elif esc.startswith("x"):
            out.append(int(esc[1:], 16))
        else:
            out.append(int(esc, 8) & 0xFF)
        pos = match.end()
    if pos < len(raw):
        out.extend(raw[pos:].encode("utf-8"))
    return out.decode("utf-8", errors="replace")


def _parse_annotation_title(path: pathlib.Path, root: pathlib.Path) -> str:
    text = _safe_read_text(path, root)
    if not text:
        return ""
    match = _PBTXT_TITLE_RE.search(text)
    if not match:
        return ""
    return _clean_single_line(_unescape_pbtxt_string(match.group(1)))


def _decode_workspace_uri(uri: str) -> str:
    if not uri:
        return ""
    if uri.lower().startswith("file://"):
        parsed = urllib.parse.urlparse(uri)
        path = urllib.parse.unquote(parsed.path or "")
        # On Windows, file:///C:/Users/... parses path as /C:/Users/...
        if re.match(r"^/[A-Za-z]:/", path):
            path = path[1:]
        return path
    return uri


def _clean_single_line(text: str, max_len: int = 120) -> str:
    cleaned = " ".join(text.strip().split())
    if len(cleaned) > max_len:
        return cleaned[: max_len - 1] + "…"
    return cleaned


def _extract_user_preview(content: str) -> str:
    if not content:
        return ""
    match = _USER_REQUEST_RE.search(content)
    snippet = match.group(1) if match else content
    return _clean_single_line(snippet, max_len=80)


def _extract_subagent_meta(content: str) -> tuple[str | None, str]:
    """Parse subagent launcher SYSTEM_MESSAGE for sender=<parent-uuid> and task preview."""
    if not content:
        return None, ""
    parent_id: str | None = None
    pos = content.find("sender=")
    if pos >= 0:
        rest = content[pos + len("sender=") :]
        tok = rest.split(None, 1)[0] if rest.split(None, 1) else ""
        if _UUID_RE.match(tok):
            parent_id = tok
    preview = ""
    cpos = content.find("content=")
    if cpos >= 0:
        body = content[cpos + len("content=") :]
        for raw_line in body.splitlines():
            trimmed = raw_line.strip().lstrip("#").strip()
            if not trimmed or trimmed.startswith("</SYSTEM_MESSAGE>"):
                continue
            cleaned_md = trimmed.replace("**", "").replace("`", "")
            preview = _clean_single_line(cleaned_md, max_len=72)
            if preview:
                break
    return parent_id, preview


def _parse_iso_timestamp(value: Any, tz: dt.tzinfo) -> dt.datetime | None:
    if not isinstance(value, str) or not value.strip():
        return None
    s = value.strip()
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    # Python <3.11 does not accept >6 fractional digits (nanoseconds).
    s = re.sub(r"(\.\d{6})\d+", r"\1", s)
    try:
        parsed = dt.datetime.fromisoformat(s)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(tz)


def _to_non_neg_int(value: Any) -> int:
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        return max(0, value)
    if isinstance(value, float):
        return max(0, int(value))
    if isinstance(value, str):
        try:
            return max(0, int(value.strip()))
        except ValueError:
            return 0
    return 0


def _load_metadata_cache(data_dir: pathlib.Path) -> dict[str, dict[str, Any]]:
    meta_path = data_dir / "cache" / "conversation_metadata.json"
    text = _safe_read_text(meta_path, data_dir)
    if not text:
        return {}
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return {}
    conversations = payload.get("conversations")
    if not isinstance(conversations, dict):
        return {}
    result: dict[str, dict[str, Any]] = {}
    for cid, entry in conversations.items():
        if not isinstance(entry, dict):
            continue
        is_internal = bool(entry.get("is_internal"))
        summary = entry.get("summary")
        if isinstance(summary, dict):
            title = _clean_single_line(str(summary.get("Title") or ""))
            preview = _clean_single_line(str(summary.get("Preview") or ""))
            ws_uris = summary.get("WorkspaceURIs")
            workspace = ""
            if isinstance(ws_uris, list) and ws_uris:
                workspace = _decode_workspace_uri(str(ws_uris[0]))
            result[str(cid)] = {
                "title": title,
                "preview": preview,
                "workspace": workspace,
                "is_internal": is_internal,
            }
        elif is_internal:
            result[str(cid)] = {
                "title": "",
                "preview": "",
                "workspace": "",
                "is_internal": True,
            }
    return result


def _has_token_metrics(record: dict[str, Any]) -> bool:
    token_keys = (
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_creation_tokens",
        "cache_creation_input_tokens",
    )
    return any(k in record and record[k] is not None for k in token_keys)


def _parse_transcript_file(
    transcript_path: pathlib.Path,
    cid: str,
) -> tuple[list[TurnRecord], str, str | None, str, int]:
    """Parse a single transcript JSONL file in UTC (tz conversion happens later)."""
    turns_by_step: dict[int, TurnRecord] = {}
    fallback_order = -1
    seen_user_input = False
    first_user_preview = ""
    subagent_parent_id: str | None = None
    subagent_preview = ""
    skipped_lines = 0

    with transcript_path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            stripped = line.strip()
            if not stripped:
                continue
            need_preview = (not seen_user_input) and ("USER_INPUT" in stripped)
            need_subagent_meta = (
                (not seen_user_input)
                and subagent_parent_id is None
                and (not subagent_preview)
                and ("SYSTEM_MESSAGE" in stripped)
            )
            if (
                stripped.startswith("{")
                and stripped.endswith("}")
                and not need_preview
                and not need_subagent_meta
                and "_tokens" not in stripped
            ):
                continue
            try:
                obj = json.loads(stripped)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue
            if not isinstance(obj, dict):
                continue

            step_type_peek = obj.get("type")
            if step_type_peek == "USER_INPUT":
                seen_user_input = True
                if not first_user_preview and isinstance(obj.get("content"), str):
                    first_user_preview = _extract_user_preview(obj["content"])
            elif (
                not seen_user_input
                and subagent_parent_id is None
                and not subagent_preview
                and step_type_peek == "SYSTEM_MESSAGE"
                and isinstance(obj.get("content"), str)
            ):
                subagent_parent_id, subagent_preview = _extract_subagent_meta(obj["content"])

            if not _has_token_metrics(obj):
                continue

            in_tok = _to_non_neg_int(obj.get("input_tokens"))
            cache_read = _to_non_neg_int(obj.get("cache_read_tokens"))
            cache_write = _to_non_neg_int(
                obj.get("cache_write_tokens")
                or obj.get("cache_creation_tokens")
                or obj.get("cache_creation_input_tokens")
            )
            out_tok = _to_non_neg_int(obj.get("output_tokens"))
            if (in_tok + cache_read + cache_write + out_tok) == 0:
                continue

            ts = _parse_iso_timestamp(obj.get("created_at"), dt.timezone.utc)
            step_type = str(obj.get("type") or "")
            raw_model = str(obj.get("model") or "").strip()
            if raw_model:
                model = raw_model
            elif step_type == "CHECKPOINT":
                model = "compaction"
            else:
                model = "unknown"

            raw_step = obj.get("step_index")
            if isinstance(raw_step, int) and not isinstance(raw_step, bool):
                step_key = raw_step
            else:
                step_key = fallback_order
                fallback_order -= 1

            raw_tools = obj.get("tool_calls")
            tool_names: list[str] = []
            if isinstance(raw_tools, list):
                for item in raw_tools:
                    if isinstance(item, dict):
                        tname = str(item.get("name") or "").strip()
                        if tname:
                            tool_names.append(tname)
            raw_thinking = obj.get("thinking")
            thinking_chars = len(raw_thinking) if isinstance(raw_thinking, str) else 0

            turns_by_step[step_key] = TurnRecord(
                conversation_id=cid,
                step_index=step_key,
                timestamp=ts,
                model=model,
                step_type=str(obj.get("type") or ""),
                status=str(obj.get("status") or ""),
                input_tokens=in_tok,
                cache_read_tokens=cache_read,
                cache_write_tokens=cache_write,
                output_tokens=out_tok,
                tool_names=tuple(tool_names),
                thinking_chars=thinking_chars,
            )

    ordered = sorted(
        turns_by_step.values(),
        key=lambda t: (
            t.step_index if t.step_index >= 0 else 10**9,
            t.timestamp or dt.datetime.min.replace(tzinfo=dt.timezone.utc),
        ),
    )
    if seen_user_input:
        subagent_parent_id = None
        subagent_preview = ""
    return ordered, first_user_preview, subagent_parent_id, subagent_preview, skipped_lines


def _extract_text_from_content(content: Any) -> str:
    """Extract plain text snippet from str or Anthropic/OpenAI/Gemini content block lists."""
    if isinstance(content, str):
        return content.strip()
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, str) and item.strip():
                parts.append(item.strip())
            elif isinstance(item, dict):
                btype = str(item.get("type") or "")
                if btype in ("tool_result", "tool_use", "thinking", "redacted_thinking", "image"):
                    continue
                txt = item.get("text")
                if isinstance(txt, str) and txt.strip():
                    parts.append(txt.strip())
        return " ".join(parts).strip()
    return ""


# ---------- Claude Code read-only parser ----------

_CLAUDE_SAFE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")


def _is_allowed_claude_session_stem(stem: str) -> bool:
    if not _CLAUDE_SAFE_ID_RE.match(stem):
        return False
    low = stem.lower()
    if (
        low == "history"
        or low == "sessions-index"
        or low.startswith("agent-")
        or low.startswith("settings")
        or low.startswith("auth")
        or low.startswith("credentials")
        or "credentials" in low
        or low.startswith("statsig")
        or low.startswith("debug")
    ):
        return False
    return True


def _is_allowed_claude_agent_id(agent_id: str) -> bool:
    return bool(_CLAUDE_SAFE_ID_RE.match(agent_id))


def _load_claude_sessions_index(
    index_path: pathlib.Path, root_dir: pathlib.Path
) -> dict[str, dict[str, str]]:
    """Read optional Claude Code projects/<proj>/sessions-index.json."""
    text = _safe_read_text(index_path, root_dir)
    if not text:
        return {}
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return {}
    entries: list[dict[str, Any]] = []
    if isinstance(payload, dict) and isinstance(payload.get("entries"), list):
        entries = [e for e in payload["entries"] if isinstance(e, dict)]
    elif isinstance(payload, list):
        entries = [e for e in payload if isinstance(e, dict)]
    elif isinstance(payload, dict):
        for k, v in payload.items():
            if isinstance(v, dict):
                item = dict(v)
                item.setdefault("sessionId", k)
                entries.append(item)
    out: dict[str, dict[str, str]] = {}
    for entry in entries:
        sid = str(entry.get("sessionId") or entry.get("id") or "").strip()
        if not sid:
            continue
        summary = _clean_single_line(str(entry.get("summary") or entry.get("customTitle") or ""))
        first_prompt = _clean_single_line(str(entry.get("firstPrompt") or ""), max_len=80)
        proj_path = str(entry.get("projectPath") or entry.get("cwd") or "").strip()
        out[sid] = {
            "title": summary or first_prompt,
            "workspace": proj_path,
        }
    return out


def _parse_claude_agent_meta(meta_path: pathlib.Path, root_dir: pathlib.Path) -> tuple[str, str]:
    """Read optional subagents/agent-<id>.meta.json -> (agent_type, description)."""
    text = _safe_read_text(meta_path, root_dir)
    if not text:
        return "", ""
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return "", ""
    if not isinstance(payload, dict):
        return "", ""
    agent_type = _clean_single_line(str(payload.get("agentType") or payload.get("subagentType") or ""), max_len=40)
    desc = _clean_single_line(str(payload.get("description") or payload.get("title") or ""), max_len=96)
    return agent_type, desc


def _parse_claude_jsonl(
    jsonl_path: pathlib.Path,
    *,
    fallback_cid: str,
    is_subagent: bool,
    parent_uuid_hint: str | None = None,
    agent_id_hint: str = "",
) -> dict[str, Any]:
    """Parse a single Claude Code JSONL file strictly read-only in UTC."""
    dedup_entries: dict[tuple[Any, ...], dict[str, Any]] = {}
    cwd = ""
    custom_title = ""
    summary_title = ""
    first_user_preview = ""
    session_id_in_file = ""
    agent_id_in_file = ""
    skipped_lines = 0

    with jsonl_path.open("r", encoding="utf-8", errors="replace") as fh:
        for line_idx, line in enumerate(fh):
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue
            if not isinstance(obj, dict):
                continue

            if not cwd and isinstance(obj.get("cwd"), str) and obj["cwd"].strip():
                cwd = obj["cwd"].strip()
            if not session_id_in_file and isinstance(obj.get("sessionId"), str) and obj["sessionId"].strip():
                session_id_in_file = obj["sessionId"].strip()
            if not agent_id_in_file and isinstance(obj.get("agentId"), str) and obj["agentId"].strip():
                agent_id_in_file = obj["agentId"].strip()

            rec_type = str(obj.get("type") or "")
            if rec_type == "custom-title":
                ct = str(obj.get("customTitle") or obj.get("title") or "").strip()
                if ct:
                    custom_title = _clean_single_line(ct)
                continue
            if rec_type == "summary":
                sm = str(obj.get("summary") or "").strip()
                if sm:
                    summary_title = _clean_single_line(sm)
                continue
            if rec_type == "user":
                if not first_user_preview and not obj.get("isMeta"):
                    msg = obj.get("message")
                    raw_c = msg.get("content") if isinstance(msg, dict) else obj.get("content")
                    txt = _extract_text_from_content(raw_c)
                    if txt:
                        first_user_preview = _clean_single_line(txt, max_len=80)
                continue
            if rec_type != "assistant":
                continue

            if obj.get("isApiErrorMessage") is True:
                continue
            msg = obj.get("message")
            if not isinstance(msg, dict):
                continue
            if msg.get("isApiErrorMessage") is True:
                continue
            raw_model = str(msg.get("model") or "").strip()
            if raw_model.lower() == "<synthetic>":
                continue

            msg_id = str(msg.get("id") or "").strip()
            usage = msg.get("usage")
            has_usage = isinstance(usage, dict)
            if not has_usage and not msg_id:
                continue

            if has_usage:
                in_tok = _to_non_neg_int(usage.get("input_tokens"))
                cache_read = _to_non_neg_int(usage.get("cache_read_input_tokens"))
                cache_write = _to_non_neg_int(usage.get("cache_creation_input_tokens"))
                out_tok = _to_non_neg_int(usage.get("output_tokens"))
                cc_obj = usage.get("cache_creation")
                cw_1h = (
                    min(_to_non_neg_int(cc_obj.get("ephemeral_1h_input_tokens")), cache_write)
                    if isinstance(cc_obj, dict)
                    else 0
                )
            else:
                in_tok = 0
                cache_read = 0
                cache_write = 0
                cw_1h = 0
                out_tok = 0

            ts = _parse_iso_timestamp(obj.get("timestamp"), dt.timezone.utc)
            stop_reason = str(msg.get("stop_reason") or "").strip()

            chunk_tools: list[tuple[str | None, str]] = []
            chunk_thinking = 0
            raw_content = msg.get("content")
            if isinstance(raw_content, list):
                for block in raw_content:
                    if not isinstance(block, dict):
                        continue
                    btype = str(block.get("type") or "")
                    if btype == "tool_use":
                        tname = str(block.get("name") or "").strip()
                        tid = str(block.get("id") or "").strip() or None
                        if tname:
                            chunk_tools.append((tid, tname))
                    elif btype == "thinking":
                        t_txt = block.get("thinking")
                        chunk_thinking += max(1, len(t_txt) if isinstance(t_txt, str) else 1)
                    elif btype == "redacted_thinking":
                        t_data = block.get("data")
                        chunk_thinking += max(1, len(t_data) if isinstance(t_data, str) else 1)

            req_id = str(obj.get("requestId") or "").strip()
            rec_uuid = str(obj.get("uuid") or "").strip()
            if msg_id and req_id:
                dkey: tuple[Any, ...] = ("mr", msg_id, req_id)
            elif msg_id:
                dkey = ("m", msg_id)
            elif rec_uuid:
                dkey = ("u", rec_uuid)
            else:
                dkey = ("l", line_idx)

            existing = dedup_entries.get(dkey)
            if existing is None:
                seen_tids: set[str] = set()
                seen_anon_tools: set[str] = set()
                tools_list: list[str] = []
                for tid, tname in chunk_tools:
                    if tid is not None:
                        if tid not in seen_tids:
                            seen_tids.add(tid)
                            tools_list.append(tname)
                    elif tname not in seen_anon_tools:
                        seen_anon_tools.add(tname)
                        tools_list.append(tname)
                dedup_entries[dkey] = {
                    "timestamp": ts,
                    "model": raw_model or "unknown",
                    "status": stop_reason or "DONE",
                    "input_tokens": in_tok,
                    "cache_read_tokens": cache_read,
                    "cache_write_tokens": cache_write,
                    "cache_write_1h_tokens": cw_1h,
                    "output_tokens": out_tok,
                    "tools": tools_list,
                    "seen_tids": seen_tids,
                    "seen_anon_tools": seen_anon_tools,
                    "thinking_chars": chunk_thinking,
                }
            else:
                if ts is not None:
                    existing["timestamp"] = ts
                if raw_model:
                    existing["model"] = raw_model
                if stop_reason:
                    existing["status"] = stop_reason
                if has_usage and (in_tok + cache_read + cache_write) > 0:
                    existing["input_tokens"] = in_tok
                    existing["cache_read_tokens"] = cache_read
                    existing["cache_write_tokens"] = cache_write
                    existing["cache_write_1h_tokens"] = min(
                        max(existing["cache_write_1h_tokens"], cw_1h),
                        cache_write,
                    )
                if has_usage and out_tok > existing["output_tokens"]:
                    existing["output_tokens"] = out_tok
                if chunk_thinking > existing["thinking_chars"]:
                    existing["thinking_chars"] = chunk_thinking
                seen_tids = existing["seen_tids"]
                seen_anon_tools = existing["seen_anon_tools"]
                tools_list = existing["tools"]
                for tid, tname in chunk_tools:
                    if tid is not None:
                        if tid not in seen_tids:
                            seen_tids.add(tid)
                            tools_list.append(tname)
                    elif tname not in seen_anon_tools:
                        seen_anon_tools.add(tname)
                        tools_list.append(tname)

    if is_subagent:
        parent_id = parent_uuid_hint or session_id_in_file or None
        aid = agent_id_in_file or agent_id_hint
        cid = f"{parent_id}__{aid}" if (parent_id and aid) else fallback_cid
    else:
        cid = session_id_in_file or fallback_cid
        parent_id = None

    turns: list[TurnRecord] = []
    step_idx = 0
    for entry in dedup_entries.values():
        total_tok = (
            entry["input_tokens"]
            + entry["cache_read_tokens"]
            + entry["cache_write_tokens"]
            + entry["output_tokens"]
        )
        if total_tok == 0:
            continue
        step_idx += 1
        turns.append(
            TurnRecord(
                conversation_id=cid,
                step_index=step_idx,
                timestamp=entry["timestamp"],
                model=entry["model"],
                step_type="ASSISTANT",
                status=entry["status"],
                input_tokens=entry["input_tokens"],
                cache_read_tokens=entry["cache_read_tokens"],
                cache_write_tokens=entry["cache_write_tokens"],
                output_tokens=entry["output_tokens"],
                tool_names=tuple(entry["tools"]),
                thinking_chars=entry["thinking_chars"],
                cache_write_1h_tokens=entry["cache_write_1h_tokens"],
            )
        )

    return {
        "cid": cid,
        "parent_id": parent_id,
        "is_subagent": is_subagent,
        "cwd": cwd,
        "title_hint": custom_title or summary_title or first_user_preview,
        "turns": turns,
        "skipped_lines": skipped_lines,
    }


# ---------- OpenAI Codex CLI read-only parser ----------

_CODEX_UUID_TAIL_RE = re.compile(
    r"([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$"
)
_CODEX_TOOL_ITEM_TYPES = frozenset(
    {"function_call", "custom_tool_call", "local_shell_call", "web_search_call", "mcp_tool_call"}
)


def _load_codex_session_index(root_dir: pathlib.Path) -> dict[str, str]:
    """Read optional <codex_home>/session_index.jsonl -> {session_id: title}."""
    idx_path = root_dir / "session_index.jsonl"
    text = _safe_read_text(idx_path, root_dir)
    if not text:
        return {}
    titles: dict[str, str] = {}
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        try:
            obj = json.loads(stripped)
        except json.JSONDecodeError:
            continue
        if not isinstance(obj, dict):
            continue
        sid = str(obj.get("id") or obj.get("session_id") or "").strip()
        title = _clean_single_line(
            str(obj.get("thread_name") or obj.get("title") or obj.get("name") or "")
        )
        if sid and title:
            titles[sid] = title
    return titles


def _codex_usage_tuple(usage: dict[str, Any]) -> tuple[int, int, int, int]:
    return (
        _to_non_neg_int(usage.get("input_tokens")),
        _to_non_neg_int(usage.get("cached_input_tokens")),
        _to_non_neg_int(usage.get("output_tokens")),
        _to_non_neg_int(usage.get("reasoning_output_tokens")),
    )


def _parse_codex_jsonl(rollout_path: pathlib.Path) -> dict[str, Any]:
    """Parse a single Codex CLI rollout-*.jsonl file strictly read-only in UTC."""
    stem = rollout_path.stem
    m_uuid = _CODEX_UUID_TAIL_RE.search(stem)
    session_id = m_uuid.group(1) if m_uuid else stem
    cwd = ""
    current_model = ""
    is_subagent = False
    parent_id: str | None = None
    subagent_label = ""
    first_user_preview = ""
    buffered_tools: list[str] = []
    buffered_thinking_chars = 0
    prev_total: tuple[int, int, int, int] | None = None
    turns: list[TurnRecord] = []
    skipped_lines = 0

    with rollout_path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue
            if not isinstance(obj, dict):
                continue

            rec_type = str(obj.get("type") or "")
            payload = obj.get("payload")
            if not isinstance(payload, dict):
                continue

            if rec_type == "session_meta":
                sid = str(payload.get("id") or "").strip()
                if sid:
                    session_id = sid
                if not cwd and isinstance(payload.get("cwd"), str) and payload["cwd"].strip():
                    cwd = payload["cwd"].strip()
                if not current_model and isinstance(payload.get("model"), str) and payload["model"].strip():
                    current_model = payload["model"].strip()
                src = payload.get("source")
                if isinstance(src, dict) and src.get("subagent") is not None:
                    is_subagent = True
                    sub = src.get("subagent")
                    if isinstance(sub, dict):
                        ts_obj = sub.get("thread_spawn")
                        if isinstance(ts_obj, dict):
                            pid = str(ts_obj.get("parent_thread_id") or "").strip()
                            if pid:
                                parent_id = pid
                            nick = str(ts_obj.get("agent_nickname") or "").strip()
                            role = str(ts_obj.get("agent_role") or "").strip()
                            parts = [p for p in (nick, role) if p]
                            if parts:
                                subagent_label = _clean_single_line(" · ".join(parts), max_len=72)
                    elif isinstance(sub, str) and sub.strip():
                        subagent_label = _clean_single_line(sub, max_len=72)
                continue

            if rec_type == "turn_context":
                mdl = str(payload.get("model") or "").strip()
                if mdl:
                    current_model = mdl
                if not cwd and isinstance(payload.get("cwd"), str) and payload["cwd"].strip():
                    cwd = payload["cwd"].strip()
                continue

            if rec_type == "response_item":
                ptype = str(payload.get("type") or "")
                if ptype in _CODEX_TOOL_ITEM_TYPES:
                    tname = str(payload.get("name") or ptype).strip()
                    if tname:
                        buffered_tools.append(tname)
                elif ptype == "reasoning":
                    r_len = 0
                    for field_key in ("summary", "content"):
                        val = payload.get(field_key)
                        if isinstance(val, str):
                            r_len += len(val)
                        elif isinstance(val, list):
                            for elem in val:
                                if isinstance(elem, dict):
                                    t_s = elem.get("text")
                                    if isinstance(t_s, str):
                                        r_len += len(t_s)
                    buffered_thinking_chars += max(1, r_len)
                elif ptype == "message" and payload.get("role") == "user" and not first_user_preview:
                    txt = _extract_text_from_content(payload.get("content"))
                    if txt:
                        first_user_preview = _clean_single_line(txt, max_len=80)
                continue

            if rec_type != "event_msg":
                continue

            ptype = str(payload.get("type") or "")
            if ptype == "user_message" and not first_user_preview:
                raw_msg = payload.get("message") or payload.get("text")
                txt = _extract_text_from_content(raw_msg)
                if txt:
                    first_user_preview = _clean_single_line(txt, max_len=80)
                continue
            if ptype == "agent_reasoning":
                r_txt = payload.get("text") or payload.get("reasoning") or ""
                buffered_thinking_chars += max(1, len(r_txt) if isinstance(r_txt, str) else 1)
                continue
            if ptype != "token_count":
                continue

            info = payload.get("info")
            if not isinstance(info, dict):
                continue

            total_u = info.get("total_token_usage")
            last_u = info.get("last_token_usage")
            delta: tuple[int, int, int, int] | None = None

            if isinstance(total_u, dict):
                curr_total = _codex_usage_tuple(total_u)
                if prev_total is not None and curr_total == prev_total:
                    continue
                if prev_total is None:
                    delta = curr_total
                elif curr_total[0] >= prev_total[0] and curr_total[2] >= prev_total[2]:
                    delta = (
                        curr_total[0] - prev_total[0],
                        max(0, curr_total[1] - prev_total[1]),
                        curr_total[2] - prev_total[2],
                        max(0, curr_total[3] - prev_total[3]),
                    )
                else:
                    delta = _codex_usage_tuple(last_u) if isinstance(last_u, dict) else curr_total
                prev_total = curr_total
            elif isinstance(last_u, dict):
                delta = _codex_usage_tuple(last_u)
            else:
                continue

            d_in, d_cached, d_out, d_reason = delta
            cache_read = min(d_cached, d_in)
            uncached_in = max(0, d_in - d_cached)
            output_tok = d_out
            if (uncached_in + cache_read + output_tok) == 0:
                continue

            ts = _parse_iso_timestamp(obj.get("timestamp"), dt.timezone.utc)
            turn_model = current_model or str(info.get("model") or "").strip() or "unknown"
            turns.append(
                TurnRecord(
                    conversation_id=session_id,
                    step_index=len(turns) + 1,
                    timestamp=ts,
                    model=turn_model,
                    step_type="TOKEN_COUNT",
                    status="DONE",
                    input_tokens=uncached_in,
                    cache_read_tokens=cache_read,
                    cache_write_tokens=0,
                    output_tokens=output_tok,
                    tool_names=tuple(buffered_tools),
                    thinking_chars=buffered_thinking_chars,
                    reasoning_tokens=d_reason,
                )
            )
            buffered_tools = []
            buffered_thinking_chars = 0

    if turns and (buffered_tools or buffered_thinking_chars > 0):
        last_t = turns[-1]
        turns[-1] = TurnRecord(
            conversation_id=last_t.conversation_id,
            step_index=last_t.step_index,
            timestamp=last_t.timestamp,
            model=last_t.model,
            step_type=last_t.step_type,
            status=last_t.status,
            input_tokens=last_t.input_tokens,
            cache_read_tokens=last_t.cache_read_tokens,
            cache_write_tokens=last_t.cache_write_tokens,
            output_tokens=last_t.output_tokens,
            tool_names=last_t.tool_names + tuple(buffered_tools),
            thinking_chars=last_t.thinking_chars + buffered_thinking_chars,
            reasoning_tokens=last_t.reasoning_tokens,
            cache_write_1h_tokens=last_t.cache_write_1h_tokens,
        )

    # Ensure conversation_id on every turn reflects final session_id
    if turns and any(t.conversation_id != session_id for t in turns):
        turns = [
            TurnRecord(
                conversation_id=session_id,
                step_index=t.step_index,
                timestamp=t.timestamp,
                model=t.model,
                step_type=t.step_type,
                status=t.status,
                input_tokens=t.input_tokens,
                cache_read_tokens=t.cache_read_tokens,
                cache_write_tokens=t.cache_write_tokens,
                output_tokens=t.output_tokens,
                tool_names=t.tool_names,
                thinking_chars=t.thinking_chars,
                reasoning_tokens=t.reasoning_tokens,
                cache_write_1h_tokens=t.cache_write_1h_tokens,
            )
            for t in turns
        ]

    return {
        "cid": session_id,
        "parent_id": parent_id if (is_subagent and parent_id != session_id) else None,
        "is_subagent": is_subagent,
        "cwd": cwd,
        "title_hint": first_user_preview or subagent_label,
        "turns": turns,
        "skipped_lines": skipped_lines,
    }


# ---------- Gemini CLI read-only parser ----------


def _load_gemini_projects_map(home_root: pathlib.Path) -> dict[str, str]:
    """Read optional <gemini_home>/projects.json -> {slug_or_hash: workspace_path}."""
    proj_path = home_root / "projects.json"
    text = _safe_read_text(proj_path, home_root)
    if not text:
        return {}
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(payload, dict):
        return {}
    mapping_obj = payload.get("projects") if isinstance(payload.get("projects"), dict) else payload
    result: dict[str, str] = {}
    for k, v in mapping_obj.items():
        if not isinstance(k, str) or not isinstance(v, str):
            continue
        ks, vs = k.strip(), v.strip()
        if not ks or not vs:
            continue
        k_is_path = ("/" in ks) or ("\\" in ks) or (":" in ks)
        v_is_path = ("/" in vs) or ("\\" in vs) or (":" in vs)
        if k_is_path and not v_is_path:
            result[vs] = ks
            result[hashlib.sha256(ks.encode("utf-8")).hexdigest()] = ks
        elif v_is_path and not k_is_path:
            result[ks] = vs
            result[hashlib.sha256(vs.encode("utf-8")).hexdigest()] = vs
        else:
            result[vs] = ks
            result[ks] = vs
    return result


def _replay_gemini_jsonl(session_path: pathlib.Path) -> tuple[str, str, list[dict[str, Any]], int]:
    """Replay Gemini CLI append-only session-*.jsonl including $set and $rewindTo mutations."""
    session_id = session_path.stem
    summary = ""
    messages: list[dict[str, Any]] = []
    skipped_lines = 0

    with session_path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue
            if not isinstance(obj, dict):
                continue

            if "$rewindTo" in obj:
                target = obj["$rewindTo"]
                if isinstance(target, dict):
                    target = target.get("messageId") or target.get("id")
                if isinstance(target, str) and target.strip():
                    tid = target.strip()
                    for idx, m in enumerate(messages):
                        if str(m.get("id") or "") == tid:
                            messages = messages[:idx]
                            break
                elif isinstance(target, int) and not isinstance(target, bool) and target >= 0:
                    messages = messages[:target]
                continue

            if "$set" in obj and isinstance(obj["$set"], dict):
                patch = obj["$set"]
                target_id = str(obj.get("id") or obj.get("messageId") or "").strip()
                if target_id:
                    for m in messages:
                        if str(m.get("id") or "") == target_id:
                            m.update(patch)
                            break
                else:
                    if isinstance(patch.get("summary"), str):
                        summary = _clean_single_line(patch["summary"])
                    if isinstance(patch.get("sessionId"), str) and patch["sessionId"].strip():
                        session_id = patch["sessionId"].strip()
                    if isinstance(patch.get("messages"), list):
                        messages = [dict(m) for m in patch["messages"] if isinstance(m, dict)]
                    elif isinstance(patch.get("id"), str) and patch["id"].strip():
                        pid = patch["id"].strip()
                        for m in messages:
                            if str(m.get("id") or "") == pid:
                                m.update(patch)
                                break
                continue

            rec_type = str(obj.get("type") or "")
            if rec_type == "session_metadata" or (
                "sessionId" in obj and not rec_type and "tokens" not in obj
            ):
                if isinstance(obj.get("sessionId"), str) and obj["sessionId"].strip():
                    session_id = obj["sessionId"].strip()
                if isinstance(obj.get("summary"), str) and obj["summary"].strip():
                    summary = _clean_single_line(obj["summary"])
                if isinstance(obj.get("messages"), list):
                    messages = [dict(m) for m in obj["messages"] if isinstance(m, dict)]
                continue

            if rec_type == "message_update":
                target_id = str(obj.get("id") or obj.get("messageId") or "").strip()
                patch = (
                    obj.get("patch")
                    if isinstance(obj.get("patch"), dict)
                    else {k: v for k, v in obj.items() if k not in ("type", "messageId")}
                )
                if target_id:
                    for m in messages:
                        if str(m.get("id") or "") == target_id:
                            m.update(patch)
                            break
                continue

            if rec_type or "id" in obj or "tokens" in obj:
                mid = str(obj.get("id") or "").strip()
                if mid:
                    replaced = False
                    for idx, existing in enumerate(messages):
                        if str(existing.get("id") or "") == mid:
                            messages[idx] = dict(obj)
                            replaced = True
                            break
                    if not replaced:
                        messages.append(dict(obj))
                else:
                    messages.append(dict(obj))

    return session_id, summary, messages, skipped_lines


def _parse_gemini_session_file(session_path: pathlib.Path) -> dict[str, Any]:
    """Parse a Gemini CLI session-*.json or session-*.jsonl file strictly read-only in UTC."""
    skipped_lines = 0
    if session_path.suffix.lower() == ".jsonl":
        session_id, summary, messages, skipped_lines = _replay_gemini_jsonl(session_path)
    else:
        try:
            with session_path.open("r", encoding="utf-8", errors="replace") as fh:
                payload = json.load(fh)
        except json.JSONDecodeError:
            return {
                "cid": session_path.stem,
                "title_hint": "",
                "turns": [],
                "skipped_lines": 1,
            }
        if not isinstance(payload, dict):
            return {
                "cid": session_path.stem,
                "title_hint": "",
                "turns": [],
                "skipped_lines": 0,
            }
        session_id = str(payload.get("sessionId") or session_path.stem).strip() or session_path.stem
        summary = _clean_single_line(str(payload.get("summary") or ""))
        raw_msgs = payload.get("messages")
        messages = [m for m in raw_msgs if isinstance(m, dict)] if isinstance(raw_msgs, list) else []

    first_user_preview = ""
    dedup_gemini: dict[Any, dict[str, Any]] = {}
    for idx, msg in enumerate(messages):
        mtype = str(msg.get("type") or "")
        if mtype == "user" and not first_user_preview:
            txt = _extract_text_from_content(msg.get("content"))
            if txt:
                first_user_preview = _clean_single_line(txt, max_len=80)
            continue
        if mtype != "gemini":
            continue
        tokens = msg.get("tokens")
        if not isinstance(tokens, dict):
            continue
        mid = str(msg.get("id") or "").strip()
        key: Any = ("id", mid) if mid else ("idx", idx)
        dedup_gemini[key] = msg

    turns: list[TurnRecord] = []
    step_idx = 0
    for msg in dedup_gemini.values():
        tokens = msg["tokens"]
        raw_in = _to_non_neg_int(tokens.get("input"))
        raw_cached = _to_non_neg_int(tokens.get("cached"))
        raw_tool = _to_non_neg_int(tokens.get("tool"))
        raw_out = _to_non_neg_int(tokens.get("output"))
        raw_thoughts = _to_non_neg_int(tokens.get("thoughts"))

        cache_read = min(raw_cached, raw_in)
        uncached_in = max(0, raw_in - raw_cached) + raw_tool
        output_tok = raw_out + raw_thoughts  # Gemini CLI output excludes thoughts!
        if (uncached_in + cache_read + output_tok) == 0:
            continue

        tool_names: list[str] = []
        raw_tc = msg.get("toolCalls")
        if isinstance(raw_tc, list):
            for tc in raw_tc:
                if isinstance(tc, dict):
                    tname = str(tc.get("name") or "").strip()
                    if tname:
                        tool_names.append(tname)

        thinking_chars = 0
        raw_thoughts_list = msg.get("thoughts")
        if isinstance(raw_thoughts_list, list):
            for th in raw_thoughts_list:
                if isinstance(th, dict):
                    subj = str(th.get("subject") or "")
                    desc = str(th.get("description") or "")
                    thinking_chars += max(1, len(subj) + len(desc))
                elif isinstance(th, str):
                    thinking_chars += max(1, len(th))

        step_idx += 1
        turns.append(
            TurnRecord(
                conversation_id=session_id,
                step_index=step_idx,
                timestamp=_parse_iso_timestamp(msg.get("timestamp"), dt.timezone.utc),
                model=str(msg.get("model") or "").strip() or "unknown",
                step_type="GEMINI",
                status="DONE",
                input_tokens=uncached_in,
                cache_read_tokens=cache_read,
                cache_write_tokens=0,
                output_tokens=output_tok,
                tool_names=tuple(tool_names),
                thinking_chars=thinking_chars,
                reasoning_tokens=raw_thoughts,
            )
        )

    return {
        "cid": session_id,
        "title_hint": summary or first_user_preview,
        "turns": turns,
        "skipped_lines": skipped_lines,
    }


def _read_companion_json(path: pathlib.Path | None) -> dict[str, Any] | None:
    if path is None or path.is_symlink() or not path.is_file():
        return None
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            payload = json.load(fh)
    except (OSError, json.JSONDecodeError):
        return None
    return payload if isinstance(payload, dict) else None


def load_statusline_state(
    path: pathlib.Path | None = DEFAULT_STATUSLINE_PATH,
    now_utc: dt.datetime | None = None,
) -> dict[str, Any] | None:
    raw = _read_companion_json(path)
    if not raw:
        return None
    now = now_utc or dt.datetime.now(dt.timezone.utc)
    captured = _parse_iso_timestamp(raw.get("captured_at"), dt.timezone.utc) or now
    payload = raw.get("payload") if isinstance(raw.get("payload"), dict) else raw
    quota_raw = payload.get("quota")
    buckets: list[dict[str, Any]] = []
    if isinstance(quota_raw, dict):
        items = (
            [
                (
                    f"{p.get('provider', 'quota')}-{p.get('window', '')}".rstrip("-"),
                    p,
                )
                for p in quota_raw["pools"]
                if isinstance(p, dict)
            ]
            if isinstance(quota_raw.get("pools"), list)
            else [(k, v) for k, v in quota_raw.items() if isinstance(v, dict)]
        )
        for bid, bobj in items:
            rf = bobj.get("remaining_fraction")
            ra = bobj.get("remaining_amount")
            disabled = bool(bobj.get("disabled"))
            if rf is None and ra is None and not disabled:
                continue
            rt = _parse_iso_timestamp(bobj.get("reset_time"), dt.timezone.utc)
            rs = bobj.get("reset_in_seconds")
            live_rem: int | None = None
            if rt is not None:
                live_rem = max(0, int((rt - now).total_seconds()))
            elif isinstance(rs, (int, float)) and not isinstance(rs, bool):
                age = max(0, int((now - captured).total_seconds()))
                live_rem = max(0, int(rs) - age)
            rem_pct = round(max(0.0, min(1.0, float(rf))) * 100.0, 2) if isinstance(rf, (int, float)) and not isinstance(rf, bool) else None
            buckets.append({
                "id": bid,
                "remaining_pct": rem_pct,
                "remaining_amount": ra if isinstance(ra, (int, float)) and not isinstance(ra, bool) else None,
                "reset_time": rt.isoformat() if rt else None,
                "live_remaining_secs": live_rem,
                "disabled": disabled,
            })
    buckets.sort(key=lambda b: (b["disabled"], b["remaining_pct"] if b["remaining_pct"] is not None else 999.0, b["id"]))
    age_secs = max(0, int((now - captured).total_seconds()))
    provenance = "local_estimate" if not buckets else ("official" if age_secs <= 600 else "official_cached")
    model_obj = payload.get("model")
    model_name = (
        str(model_obj.get("display_name") or model_obj.get("id") or "")
        if isinstance(model_obj, dict)
        else (str(model_obj) if isinstance(model_obj, str) else "")
    )
    return {
        "captured_at": captured.isoformat(),
        "age_secs": age_secs,
        "provenance": provenance,
        "plan_tier": str(payload.get("plan_tier") or ""),
        "model": model_name,
        "agent_state": str(payload.get("agent_state") or ""),
        "buckets": buckets,
    }


def load_budget_config(path: pathlib.Path | None = DEFAULT_BUDGET_PATH) -> dict[str, float]:
    cfg = {
        "daily_usd": 0.0,
        "weekly_usd": 0.0,
        "warn_pct": 70.0,
        "critical_pct": 90.0,
        "quota_warn_rem_pct": 20.0,
        "quota_crit_rem_pct": 10.0,
    }
    raw = _read_companion_json(path)
    if not raw:
        return cfg
    for k in list(cfg.keys()):
        v = raw.get(k)
        if isinstance(v, (int, float)) and not isinstance(v, bool) and v >= 0:
            cfg[k] = float(v)
    return cfg


def _fmt_countdown(secs: int) -> str:
    s = max(0, int(secs))
    if s >= 86400:
        return f"{s // 86400}d{(s % 86400) // 3600}h"
    if s >= 3600:
        return f"{s // 3600}h{(s % 3600) // 60:02d}m"
    if s >= 60:
        return f"{s // 60}m"
    return f"{s}s"


def _fmt_duration(secs: int) -> str:
    s = max(0, int(secs))
    if s < 60:
        return f"{s}s"
    if s < 3600:
        return f"{(s + 30) // 60}m"
    return f"{s // 3600}h{(s % 3600) // 60:02d}m"


class IncrementalScanner:
    """In-memory mtime/size cached read-only scanner for low-CPU live polling.

    Never writes to disk; caches parsed immutable per-file results in RAM only.
    Supports multi-agent read-only scanning (Antigravity CLI/App/IDE, Claude Code,
    OpenAI Codex CLI, Gemini CLI) and optional read-only backfill from `archive.json`.
    """

    def __init__(
        self,
        data_dir: pathlib.Path = DEFAULT_DATA_DIR,
        pricing_path: pathlib.Path | None = DEFAULT_PRICING_PATH,
        extra_dirs: Sequence[pathlib.Path] | None = None,
        archive_path: pathlib.Path | None = None,
        sources: Sequence[SourceSpec] | None = None,
        agent_filter: set[str] | None = None,
    ) -> None:
        self.agent_filter = set(agent_filter) if agent_filter is not None else None
        if sources is not None:
            specs = [
                SourceSpec(s.kind, s.root.expanduser(), s.source_name)
                for s in sources
            ]
            self.sources = specs
            self.data_dirs = [s.root for s in specs]
            self.data_dir = specs[0].root if specs else data_dir.expanduser()
        else:
            self.data_dir = data_dir.expanduser()
            dirs: list[pathlib.Path] = [self.data_dir]
            if extra_dirs:
                for d in extra_dirs:
                    ep = d.expanduser()
                    if ep not in dirs:
                        dirs.append(ep)
            self.data_dirs = dirs
            self.sources = [
                SourceSpec("antigravity", d, _infer_source_name(d)) for d in dirs
            ]
        self.pricing = PricingCatalog(pricing_path)
        self.archive_path = archive_path
        self._transcript_cache: dict[str, tuple[int, int, list[TurnRecord], str, str | None, str, int]] = {}
        self._agent_file_cache: dict[str, tuple[int, int, dict[str, Any]]] = {}
        self._ann_cache: dict[str, tuple[int, int, str]] = {}
        self._meta_sigs: dict[str, tuple[int, int]] = {}
        self._meta_caches: dict[str, dict[str, dict[str, Any]]] = {}
        self._aux_json_cache: dict[str, tuple[int, int, Any]] = {}

    def _get_metadata_cache(self, root_dir: pathlib.Path) -> dict[str, dict[str, Any]]:
        rk = str(root_dir)
        meta_path = root_dir / "cache" / "conversation_metadata.json"
        if meta_path.is_symlink() or not meta_path.is_file():
            self._meta_sigs.pop(rk, None)
            self._meta_caches[rk] = {}
            return self._meta_caches[rk]
        try:
            st = meta_path.stat()
            sig = (st.st_mtime_ns, st.st_size)
        except OSError:
            return self._meta_caches.get(rk, {})
        if sig != self._meta_sigs.get(rk):
            self._meta_caches[rk] = _load_metadata_cache(root_dir)
            self._meta_sigs[rk] = sig
        return self._meta_caches[rk]

    def _get_annotation_title(self, root_dir: pathlib.Path, cid: str) -> str:
        ann_path = root_dir / "annotations" / f"{cid}.pbtxt"
        key = str(ann_path)
        if ann_path.is_symlink() or not ann_path.is_file():
            self._ann_cache.pop(key, None)
            return ""
        try:
            st = ann_path.stat()
            mtime_ns, size = st.st_mtime_ns, st.st_size
        except OSError:
            return ""
        cached = self._ann_cache.get(key)
        if cached is not None and cached[0] == mtime_ns and cached[1] == size:
            return cached[2]
        title = _parse_annotation_title(ann_path, root_dir)
        self._ann_cache[key] = (mtime_ns, size, title)
        return title

    def _get_cached_aux(
        self, path: pathlib.Path, loader: Any, default_val: Any
    ) -> Any:
        key = str(path)
        if path.is_symlink() or not path.is_file():
            self._aux_json_cache.pop(key, None)
            return default_val
        try:
            st = path.stat()
            sig = (st.st_mtime_ns, st.st_size)
        except OSError:
            return default_val
        cached = self._aux_json_cache.get(key)
        if cached is not None and cached[0] == sig[0] and cached[1] == sig[1]:
            return cached[2]
        val = loader()
        self._aux_json_cache[key] = (sig[0], sig[1], val)
        return val

    def _filter_and_price_turns(
        self,
        raw_turns: Sequence[TurnRecord],
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
    ) -> list[TurnRecord]:
        real_totals: dict[str, int] = defaultdict(int)
        for t in raw_turns:
            if t.step_type != "CHECKPOINT" and t.model not in ("compaction", "unknown"):
                real_totals[t.model] += t.total_tokens
        fallback_model = max(real_totals.items(), key=lambda kv: kv[1])[0] if real_totals else ""
        last_real_model = ""

        filtered_turns: list[TurnRecord] = []
        for turn in raw_turns:
            is_cp = turn.step_type == "CHECKPOINT" or turn.model == "compaction"
            if not is_cp and turn.model not in ("compaction", "unknown"):
                last_real_model = turn.model
                eff_model = turn.model
            elif turn.model not in ("compaction", "unknown"):
                eff_model = turn.model
            else:
                eff_model = last_real_model or fallback_model or turn.model

            local_ts = turn.timestamp.astimezone(tz) if turn.timestamp is not None else None
            if since is not None and (local_ts is None or local_ts.date() < since):
                continue
            if until is not None and (local_ts is None or local_ts.date() > until):
                continue
            if model_needle and model_needle not in turn.model.lower():
                continue
            turn_cost = self.pricing.price_turn(
                eff_model,
                turn.input_tokens,
                turn.cache_read_tokens,
                turn.cache_write_tokens,
                turn.output_tokens,
                local_ts.date() if local_ts is not None else None,
                cache_write_1h_tokens=turn.cache_write_1h_tokens,
            )
            filtered_turns.append(
                TurnRecord(
                    conversation_id=turn.conversation_id,
                    step_index=turn.step_index,
                    timestamp=local_ts,
                    model=turn.model,
                    step_type=turn.step_type,
                    status=turn.status,
                    input_tokens=turn.input_tokens,
                    cache_read_tokens=turn.cache_read_tokens,
                    cache_write_tokens=turn.cache_write_tokens,
                    output_tokens=turn.output_tokens,
                    tool_names=turn.tool_names,
                    thinking_chars=turn.thinking_chars,
                    reasoning_tokens=turn.reasoning_tokens,
                    cache_write_1h_tokens=turn.cache_write_1h_tokens,
                    cost=turn_cost,
                )
            )
        return filtered_turns

    def _load_archived_backfill(
        self,
        live_keys: set[tuple[str, str]],
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
    ) -> list[SessionInfo]:
        raw = _read_companion_json(self.archive_path)
        if not raw or not isinstance(raw.get("sessions"), dict):
            return []
        out: list[SessionInfo] = []
        for raw_key, s_obj in raw["sessions"].items():
            if not isinstance(s_obj, dict):
                continue
            raw_src = str(s_obj.get("source") or "cli")
            src = _normalize_source_name(raw_src)
            fam = source_to_agent_family(src)
            explicit_cid = str(s_obj.get("conversation_id") or "").strip()
            if explicit_cid:
                cid = explicit_cid
            elif ":" in raw_key:
                pfx, rest = raw_key.split(":", 1)
                cid = rest.strip() if (pfx.strip().lower() in _AGENT_ALIASES and rest.strip()) else raw_key
            else:
                cid = raw_key
            if self.agent_filter is not None and fam not in self.agent_filter:
                continue
            if (fam, cid) in live_keys:
                continue
            if session_needle and not cid.lower().startswith(session_needle):
                continue
            raw_turns = s_obj.get("turns")
            if not isinstance(raw_turns, list) or not raw_turns:
                continue
            parsed_turns: list[TurnRecord] = []
            for t_obj in raw_turns:
                if not isinstance(t_obj, dict):
                    continue
                ts = _parse_iso_timestamp(t_obj.get("ts"), dt.timezone.utc)
                is_cp = bool(t_obj.get("cp"))
                model = str(t_obj.get("m") or ("compaction" if is_cp else "unknown"))
                tools_raw = t_obj.get("tools")
                tools = tuple(str(x) for x in tools_raw if isinstance(x, str)) if isinstance(tools_raw, list) else ()
                cw = _to_non_neg_int(t_obj.get("cw"))
                cw1h = min(_to_non_neg_int(t_obj.get("cw1h")), cw)
                parsed_turns.append(
                    TurnRecord(
                        conversation_id=cid,
                        step_index=int(t_obj.get("step", -1)),
                        timestamp=ts,
                        model=model,
                        step_type="CHECKPOINT" if is_cp else "PLANNER_RESPONSE",
                        status="DONE",
                        input_tokens=_to_non_neg_int(t_obj.get("in")),
                        cache_read_tokens=_to_non_neg_int(t_obj.get("cr")),
                        cache_write_tokens=cw,
                        output_tokens=_to_non_neg_int(t_obj.get("out")),
                        tool_names=tools,
                        thinking_chars=_to_non_neg_int(t_obj.get("th")),
                        reasoning_tokens=_to_non_neg_int(t_obj.get("rt")),
                        cache_write_1h_tokens=cw1h,
                    )
                )
            filtered_turns = self._filter_and_price_turns(
                parsed_turns, tz, since, until, model_needle
            )
            if not filtered_turns:
                continue
            sess = SessionInfo(
                conversation_id=cid,
                title=str(s_obj.get("title") or "(archived)"),
                workspace=str(s_obj.get("workspace") or "(unknown)"),
                source=src,
                is_archived=True,
                is_subagent=bool(s_obj.get("is_subagent")),
                parent_id=str(s_obj["parent_id"]) if s_obj.get("parent_id") else None,
                parent_title=str(s_obj.get("parent_title") or ""),
            )
            for t in filtered_turns:
                sess.add_turn(t)
            out.append(sess)
        return out

    def _scan_antigravity_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        seen_agy_keys: set[str],
        warnings: list[str],
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if root_dir.is_symlink() or not root_dir.exists() or not root_dir.is_dir():
            return False, 0, 0, 0, 0
        brain_dir = root_dir / "brain"
        if brain_dir.is_symlink() or not brain_dir.exists() or not brain_dir.is_dir():
            return False, 0, 0, 0, 0

        source_name = spec.source_name
        meta_cache = self._get_metadata_cache(root_dir)
        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        try:
            conv_dirs = sorted(brain_dir.iterdir(), key=lambda p: p.name)
        except OSError as exc:
            warnings.append(f"Failed to list {brain_dir}: {exc}")
            return True, 0, 0, 0, 0

        for conv_dir in conv_dirs:
            if conv_dir.is_symlink() or not conv_dir.is_dir():
                continue
            cid = conv_dir.name
            if session_needle and not cid.lower().startswith(session_needle):
                continue

            logs_dir = conv_dir / ".system_generated" / "logs"
            transcript_path = logs_dir / "transcript.jsonl"
            if not transcript_path.is_file() or transcript_path.is_symlink():
                fallback = logs_dir / "transcript_full.jsonl"
                if fallback.is_file() and not fallback.is_symlink():
                    transcript_path = fallback
                else:
                    skipped_empty += 1
                    continue

            if not _is_within_dir(transcript_path, root_dir):
                continue

            cache_key = str(transcript_path)
            seen_agy_keys.add(cache_key)
            try:
                st = transcript_path.stat()
                mtime_ns, size = st.st_mtime_ns, st.st_size
            except OSError as exc:
                warnings.append(f"Could not stat {transcript_path}: {exc}")
                continue

            cached = self._transcript_cache.get(cache_key)
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                raw_turns, first_user_preview, subagent_parent_id, subagent_preview, file_skipped = (
                    cached[2],
                    cached[3],
                    cached[4],
                    cached[5],
                    cached[6],
                )
                files_cached += 1
            else:
                try:
                    raw_turns, first_user_preview, subagent_parent_id, subagent_preview, file_skipped = (
                        _parse_transcript_file(transcript_path, cid)
                    )
                except OSError as exc:
                    warnings.append(f"Could not read {transcript_path}: {exc}")
                    continue
                self._transcript_cache[cache_key] = (
                    mtime_ns,
                    size,
                    raw_turns,
                    first_user_preview,
                    subagent_parent_id,
                    subagent_preview,
                    file_skipped,
                )
                files_reparsed += 1

            skipped_lines += file_skipped
            filtered_turns = self._filter_and_price_turns(raw_turns, tz, since, until, model_needle)
            if not filtered_turns:
                skipped_empty += 1
                continue

            ann_title = self._get_annotation_title(root_dir, cid)
            meta = meta_cache.get(cid)
            if meta is not None:
                is_subagent = bool(meta.get("is_internal"))
            else:
                is_subagent = subagent_parent_id is not None
            parent_id = subagent_parent_id if (is_subagent and subagent_parent_id != cid) else None
            meta_dict = meta or {}
            title = (
                ann_title
                or str(meta_dict.get("title") or "")
                or str(meta_dict.get("preview") or "")
                or first_user_preview
                or (subagent_preview if is_subagent else "")
                or ("(subagent)" if is_subagent else "(untitled)")
            )
            workspace = str(meta_dict.get("workspace") or "") or "(unknown)"

            session = SessionInfo(
                conversation_id=cid,
                title=title,
                workspace=workspace,
                transcript_path=cache_key,
                source=source_name,
                is_subagent=is_subagent,
                parent_id=parent_id,
            )
            for t in filtered_turns:
                session.add_turn(t)

            skey = ("antigravity", cid)
            prev = sessions_by_key.get(skey)
            if prev is None or len(session.turns) > len(prev.turns):
                sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_claude_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        seen_agent_keys: set[str],
        claude_parent_meta: dict[str, tuple[str, str]],
        warnings: list[str],
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if root_dir.is_symlink() or not root_dir.is_dir():
            return False, 0, 0, 0, 0
        projects_dir = root_dir / "projects"
        if projects_dir.is_symlink() or not projects_dir.is_dir():
            return False, 0, 0, 0, 0

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        try:
            proj_dirs = sorted(projects_dir.iterdir(), key=lambda p: p.name)
        except OSError as exc:
            warnings.append(f"Failed to list {projects_dir}: {exc}")
            return True, 0, 0, 0, 0

        for proj_dir in proj_dirs:
            if proj_dir.is_symlink() or not proj_dir.is_dir():
                continue
            if not _is_within_dir(proj_dir, root_dir):
                continue

            idx_path = proj_dir / "sessions-index.json"
            sess_index: dict[str, dict[str, str]] = self._get_cached_aux(
                idx_path,
                lambda p=idx_path, r=root_dir: _load_claude_sessions_index(p, r),
                {},
            )
            for sid, info in sess_index.items():
                if info.get("title") or info.get("workspace"):
                    claude_parent_meta.setdefault(sid, (info.get("title", ""), info.get("workspace", "")))

            # Collect main session JSONL files and subagent JSONL files strictly under projects/<proj>/
            file_jobs: list[tuple[pathlib.Path, bool, str | None, str]] = []
            try:
                children = sorted(proj_dir.iterdir(), key=lambda p: p.name)
            except OSError as exc:
                warnings.append(f"Failed to list {proj_dir}: {exc}")
                continue

            for child in children:
                if child.is_symlink():
                    continue
                if child.is_file() and child.suffix.lower() == ".jsonl":
                    if _is_allowed_claude_session_stem(child.stem):
                        file_jobs.append((child, False, None, ""))
                elif child.is_dir() and _is_allowed_claude_session_stem(child.name):
                    sub_dir = child / "subagents"
                    if sub_dir.is_symlink() or not sub_dir.is_dir():
                        continue
                    try:
                        sub_files = sorted(sub_dir.iterdir(), key=lambda p: p.name)
                    except OSError:
                        continue
                    for sf in sub_files:
                        if (
                            not sf.is_symlink()
                            and sf.is_file()
                            and sf.name.startswith("agent-")
                            and sf.suffix.lower() == ".jsonl"
                        ):
                            aid_hint = sf.stem[len("agent-") :]
                            if _is_allowed_claude_agent_id(aid_hint):
                                file_jobs.append((sf, True, child.name, aid_hint))

            for fpath, is_sub, parent_hint, aid_hint in file_jobs:
                if not _is_within_dir(fpath, root_dir):
                    continue
                cache_key = f"claude:{fpath}"
                seen_agent_keys.add(cache_key)
                try:
                    st = fpath.stat()
                    mtime_ns, size = st.st_mtime_ns, st.st_size
                except OSError as exc:
                    warnings.append(f"Could not stat {fpath}: {exc}")
                    continue

                cached = self._agent_file_cache.get(cache_key)
                if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                    parsed = cached[2]
                    files_cached += 1
                else:
                    try:
                        parsed = _parse_claude_jsonl(
                            fpath,
                            fallback_cid=fpath.stem,
                            is_subagent=is_sub,
                            parent_uuid_hint=parent_hint,
                            agent_id_hint=aid_hint,
                        )
                    except OSError as exc:
                        warnings.append(f"Could not read {fpath}: {exc}")
                        continue
                    self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                    files_reparsed += 1

                skipped_lines += int(parsed.get("skipped_lines", 0))
                cid = str(parsed["cid"])
                if not is_sub and (parsed.get("title_hint") or parsed.get("cwd")):
                    prev_pm = claude_parent_meta.get(cid, ("", ""))
                    claude_parent_meta[cid] = (
                        prev_pm[0] or str(parsed.get("title_hint") or ""),
                        prev_pm[1] or str(parsed.get("cwd") or ""),
                    )

                if session_needle and not cid.lower().startswith(session_needle):
                    continue

                raw_turns: list[TurnRecord] = parsed["turns"]
                filtered_turns = self._filter_and_price_turns(raw_turns, tz, since, until, model_needle)
                if not filtered_turns:
                    skipped_empty += 1
                    continue

                parent_id: str | None = parsed.get("parent_id")
                idx_info = sess_index.get(parent_id or cid, {}) if is_sub else sess_index.get(cid, {})
                title = ""
                if is_sub:
                    meta_path = fpath.with_name(f"{fpath.stem}.meta.json")
                    agent_type, desc = self._get_cached_aux(
                        meta_path,
                        lambda p=meta_path, r=root_dir: _parse_claude_agent_meta(p, r),
                        ("", ""),
                    )
                    if agent_type and desc:
                        title = _clean_single_line(f"[{agent_type}] {desc}")
                    elif desc:
                        title = desc
                    elif parsed.get("title_hint"):
                        title = str(parsed["title_hint"])
                    elif agent_type:
                        title = f"{agent_type} subagent"
                    else:
                        title = "(subagent)"
                else:
                    title = (
                        str(parsed.get("title_hint") or "")
                        or str(idx_info.get("title") or "")
                        or "(untitled)"
                    )

                workspace = (
                    str(parsed.get("cwd") or "")
                    or str(idx_info.get("workspace") or "")
                    or "(unknown)"
                )

                session = SessionInfo(
                    conversation_id=cid,
                    title=title,
                    workspace=workspace,
                    transcript_path=str(fpath),
                    source="claude",
                    is_subagent=is_sub,
                    parent_id=parent_id,
                )
                for t in filtered_turns:
                    session.add_turn(t)

                skey = ("claude", cid)
                prev = sessions_by_key.get(skey)
                if prev is None or len(session.turns) > len(prev.turns):
                    sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_codex_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        seen_agent_keys: set[str],
        codex_parent_meta: dict[str, tuple[str, str]],
        warnings: list[str],
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if root_dir.is_symlink() or not root_dir.is_dir():
            return False, 0, 0, 0, 0
        sessions_dir = root_dir / "sessions"
        archived_dir = root_dir / "archived_sessions"
        has_sessions = sessions_dir.is_dir() and not sessions_dir.is_symlink()
        has_archived = archived_dir.is_dir() and not archived_dir.is_symlink()
        if not has_sessions and not has_archived:
            return False, 0, 0, 0, 0

        idx_path = root_dir / "session_index.jsonl"
        index_titles: dict[str, str] = self._get_cached_aux(
            idx_path,
            lambda r=root_dir: _load_codex_session_index(r),
            {},
        )
        for sid, t_val in index_titles.items():
            codex_parent_meta.setdefault(sid, (t_val, ""))

        rollout_files: list[pathlib.Path] = []
        if has_sessions:
            try:
                year_dirs = sorted(sessions_dir.iterdir(), key=lambda p: p.name)
            except OSError as exc:
                warnings.append(f"Failed to list {sessions_dir}: {exc}")
                year_dirs = []
            for y_dir in year_dirs:
                if y_dir.is_symlink() or not y_dir.is_dir():
                    continue
                try:
                    month_dirs = sorted(y_dir.iterdir(), key=lambda p: p.name)
                except OSError:
                    continue
                for m_dir in month_dirs:
                    if m_dir.is_symlink() or not m_dir.is_dir():
                        continue
                    try:
                        day_dirs = sorted(m_dir.iterdir(), key=lambda p: p.name)
                    except OSError:
                        continue
                    for d_dir in day_dirs:
                        if d_dir.is_symlink() or not d_dir.is_dir():
                            continue
                        try:
                            files = sorted(d_dir.iterdir(), key=lambda p: p.name)
                        except OSError:
                            continue
                        for f in files:
                            if (
                                not f.is_symlink()
                                and f.is_file()
                                and f.name.startswith("rollout-")
                                and f.suffix.lower() == ".jsonl"
                            ):
                                rollout_files.append(f)

        if has_archived:
            try:
                a_files = sorted(archived_dir.iterdir(), key=lambda p: p.name)
            except OSError as exc:
                warnings.append(f"Failed to list {archived_dir}: {exc}")
                a_files = []
            for f in a_files:
                if (
                    not f.is_symlink()
                    and f.is_file()
                    and f.name.startswith("rollout-")
                    and f.suffix.lower() == ".jsonl"
                ):
                    rollout_files.append(f)

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        for fpath in rollout_files:
            if not _is_within_dir(fpath, root_dir):
                continue
            cache_key = f"codex:{fpath}"
            seen_agent_keys.add(cache_key)
            try:
                st = fpath.stat()
                mtime_ns, size = st.st_mtime_ns, st.st_size
            except OSError as exc:
                warnings.append(f"Could not stat {fpath}: {exc}")
                continue

            cached = self._agent_file_cache.get(cache_key)
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                parsed = cached[2]
                files_cached += 1
            else:
                try:
                    parsed = _parse_codex_jsonl(fpath)
                except OSError as exc:
                    warnings.append(f"Could not read {fpath}: {exc}")
                    continue
                self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                files_reparsed += 1

            skipped_lines += int(parsed.get("skipped_lines", 0))
            cid = str(parsed["cid"])
            is_sub = bool(parsed.get("is_subagent"))
            idx_title = index_titles.get(cid, "")
            title = (
                idx_title
                or str(parsed.get("title_hint") or "")
                or ("(subagent)" if is_sub else "(untitled)")
            )
            workspace = str(parsed.get("cwd") or "") or "(unknown)"
            if not is_sub and (title not in ("(untitled)", "") or workspace != "(unknown)"):
                codex_parent_meta[cid] = (title, workspace)

            if session_needle and not cid.lower().startswith(session_needle):
                continue

            filtered_turns = self._filter_and_price_turns(
                parsed["turns"], tz, since, until, model_needle
            )
            if not filtered_turns:
                skipped_empty += 1
                continue

            session = SessionInfo(
                conversation_id=cid,
                title=title,
                workspace=workspace,
                transcript_path=str(fpath),
                source="codex",
                is_subagent=is_sub,
                parent_id=parsed.get("parent_id"),
            )
            for t in filtered_turns:
                session.add_turn(t)

            skey = ("codex", cid)
            prev = sessions_by_key.get(skey)
            if prev is None or len(session.turns) > len(prev.turns):
                sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_gemini_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        seen_agent_keys: set[str],
        warnings: list[str],
    ) -> tuple[bool, int, int, int, int]:
        root = spec.root
        if root.is_symlink() or not root.is_dir():
            return False, 0, 0, 0, 0
        if (root / "tmp").is_dir() and not (root / "tmp").is_symlink():
            tmp_dir = root / "tmp"
            home_root = root
        else:
            tmp_dir = root
            home_root = root.parent

        proj_json_path = home_root / "projects.json"
        projects_map: dict[str, str] = self._get_cached_aux(
            proj_json_path,
            lambda hr=home_root: _load_gemini_projects_map(hr),
            {},
        )

        try:
            proj_dirs = sorted(tmp_dir.iterdir(), key=lambda p: p.name)
        except OSError as exc:
            warnings.append(f"Failed to list {tmp_dir}: {exc}")
            return True, 0, 0, 0, 0

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        for proj_dir in proj_dirs:
            if proj_dir.is_symlink() or not proj_dir.is_dir():
                continue
            if not _is_within_dir(proj_dir, tmp_dir):
                continue
            chats_dir = proj_dir / "chats"
            if chats_dir.is_symlink() or not chats_dir.is_dir():
                continue

            pr_path = proj_dir / ".project_root"
            proj_root_txt: str = self._get_cached_aux(
                pr_path,
                lambda p=pr_path, r=tmp_dir: (_safe_read_text(p, r) or "").strip(),
                "",
            )
            workspace = proj_root_txt or projects_map.get(proj_dir.name, "") or "(unknown)"

            try:
                chat_files = sorted(chats_dir.iterdir(), key=lambda p: p.name)
            except OSError as exc:
                warnings.append(f"Failed to list {chats_dir}: {exc}")
                continue

            for fpath in chat_files:
                if (
                    fpath.is_symlink()
                    or not fpath.is_file()
                    or not fpath.name.startswith("session-")
                    or fpath.suffix.lower() not in (".json", ".jsonl")
                ):
                    continue
                if not _is_within_dir(fpath, tmp_dir):
                    continue

                cache_key = f"gemini:{fpath}"
                seen_agent_keys.add(cache_key)
                try:
                    st = fpath.stat()
                    mtime_ns, size = st.st_mtime_ns, st.st_size
                except OSError as exc:
                    warnings.append(f"Could not stat {fpath}: {exc}")
                    continue

                cached = self._agent_file_cache.get(cache_key)
                if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                    parsed = cached[2]
                    files_cached += 1
                else:
                    try:
                        parsed = _parse_gemini_session_file(fpath)
                    except OSError as exc:
                        warnings.append(f"Could not read {fpath}: {exc}")
                        continue
                    self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                    files_reparsed += 1

                skipped_lines += int(parsed.get("skipped_lines", 0))
                cid = str(parsed["cid"])
                if session_needle and not cid.lower().startswith(session_needle):
                    continue

                filtered_turns = self._filter_and_price_turns(
                    parsed["turns"], tz, since, until, model_needle
                )
                if not filtered_turns:
                    skipped_empty += 1
                    continue

                session = SessionInfo(
                    conversation_id=cid,
                    title=str(parsed.get("title_hint") or "") or "(untitled)",
                    workspace=workspace,
                    transcript_path=str(fpath),
                    source="gemini",
                    is_subagent=False,
                    parent_id=None,
                )
                for t in filtered_turns:
                    session.add_turn(t)

                skey = ("gemini", cid)
                prev = sessions_by_key.get(skey)
                if prev is None or len(session.turns) > len(prev.turns):
                    sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def scan(
        self,
        tz: dt.tzinfo,
        since: dt.date | None = None,
        until: dt.date | None = None,
        model_filter: str | None = None,
        session_filter: str | None = None,
        workspace_filter: str | None = None,
    ) -> ScanReport:
        t0 = time.perf_counter()
        self.pricing.refresh()
        warnings: list[str] = []
        sessions_by_key: dict[tuple[str, str], SessionInfo] = {}
        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0
        found_any_source = False

        model_needle = model_filter.lower().strip() if model_filter else None
        session_needle = session_filter.lower().strip() if session_filter else None
        workspace_needle = workspace_filter.lower().strip() if workspace_filter else None
        seen_agy_keys: set[str] = set()
        seen_agent_keys: set[str] = set()
        claude_parent_meta: dict[str, tuple[str, str]] = {}
        codex_parent_meta: dict[str, tuple[str, str]] = {}

        for spec in self.sources:
            if self.agent_filter is not None and spec.kind not in self.agent_filter:
                continue
            if spec.kind == "antigravity":
                found, se, sl, fc, fr = self._scan_antigravity_source(
                    spec, tz, since, until, model_needle, session_needle,
                    sessions_by_key, seen_agy_keys, warnings,
                )
            elif spec.kind == "claude":
                found, se, sl, fc, fr = self._scan_claude_source(
                    spec, tz, since, until, model_needle, session_needle,
                    sessions_by_key, seen_agent_keys, claude_parent_meta, warnings,
                )
            elif spec.kind == "codex":
                found, se, sl, fc, fr = self._scan_codex_source(
                    spec, tz, since, until, model_needle, session_needle,
                    sessions_by_key, seen_agent_keys, codex_parent_meta, warnings,
                )
            elif spec.kind == "gemini":
                found, se, sl, fc, fr = self._scan_gemini_source(
                    spec, tz, since, until, model_needle, session_needle,
                    sessions_by_key, seen_agent_keys, warnings,
                )
            else:
                continue
            found_any_source = found_any_source or found
            skipped_empty += se
            skipped_lines += sl
            files_cached += fc
            files_reparsed += fr

        sessions = list(sessions_by_key.values())

        # Link subagent sessions to their parent within the same agent family.
        parent_lookup: dict[tuple[str, str], tuple[str, str]] = {
            (s.agent_family, s.conversation_id): (s.title, s.workspace) for s in sessions
        }
        agy_roots = [s.root for s in self.sources if s.kind == "antigravity"]
        for s in sessions:
            if not s.is_subagent or not s.parent_id:
                continue
            fam = s.agent_family
            pid = s.parent_id
            resolved = parent_lookup.get((fam, pid))
            if resolved is None:
                if fam == "antigravity":
                    for root_dir in agy_roots:
                        p_ann = self._get_annotation_title(root_dir, pid)
                        p_meta = self._meta_caches.get(str(root_dir), {}).get(pid, {})
                        ptitle = p_ann or str(p_meta.get("title") or "") or str(p_meta.get("preview") or "")
                        pws = str(p_meta.get("workspace") or "")
                        if ptitle or pws:
                            resolved = (ptitle, pws)
                            break
                elif fam == "claude":
                    resolved = claude_parent_meta.get(pid)
                elif fam == "codex":
                    resolved = codex_parent_meta.get(pid)
            if resolved is not None:
                ptitle, pws = resolved
                if ptitle:
                    s.parent_title = ptitle
                if (not s.workspace or s.workspace == "(unknown)") and pws and pws != "(unknown)":
                    s.workspace = pws
                if s.title in ("(subagent)", "(untitled)") and ptitle:
                    s.title = f"subtask · {ptitle}"

        archived_backfill = self._load_archived_backfill(
            set(sessions_by_key.keys()),
            tz,
            since,
            until,
            model_needle,
            session_needle,
        )
        sessions.extend(archived_backfill)

        if workspace_needle:
            sessions = [s for s in sessions if workspace_needle in s.workspace.lower()]

        if not found_any_source and not archived_backfill:
            warnings.append(f"Data directory not found: {self.data_dir}")

        if not session_needle:
            for stale_key in list(self._transcript_cache.keys()):
                if stale_key not in seen_agy_keys:
                    self._transcript_cache.pop(stale_key, None)
            for stale_key in list(self._agent_file_cache.keys()):
                if stale_key not in seen_agent_keys:
                    self._agent_file_cache.pop(stale_key, None)

        if skipped_lines > 0:
            warnings.append(
                f"Skipped {skipped_lines} malformed/incomplete JSONL line(s) "
                "(normal if an agent session is actively writing)."
            )

        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        return ScanReport(
            data_dir=self.data_dir,
            data_dirs=list(self.data_dirs),
            sessions=sessions,
            warnings=warnings,
            archived_sessions_count=len(archived_backfill),
            skipped_empty_sessions=skipped_empty,
            skipped_lines=skipped_lines,
            scan_ms=elapsed_ms,
            files_cached=files_cached,
            files_reparsed=files_reparsed,
        )


def scan_usage(
    data_dir: pathlib.Path,
    tz: dt.tzinfo,
    since: dt.date | None = None,
    until: dt.date | None = None,
    model_filter: str | None = None,
    session_filter: str | None = None,
    workspace_filter: str | None = None,
    pricing_path: pathlib.Path | None = DEFAULT_PRICING_PATH,
    extra_dirs: Sequence[pathlib.Path] | None = None,
    archive_path: pathlib.Path | None = None,
    sources: Sequence[SourceSpec] | None = None,
    agent_filter: set[str] | None = None,
) -> ScanReport:
    return IncrementalScanner(
        data_dir,
        pricing_path=pricing_path,
        extra_dirs=extra_dirs,
        archive_path=archive_path,
        sources=sources,
        agent_filter=agent_filter,
    ).scan(
        tz=tz,
        since=since,
        until=until,
        model_filter=model_filter,
        session_filter=session_filter,
        workspace_filter=workspace_filter,
    )


# ---------- Display & formatting helpers ----------


def _display_width(text: str) -> int:
    width = 0
    for ch in text:
        if unicodedata.combining(ch):
            continue
        eaw = unicodedata.east_asian_width(ch)
        width += 2 if eaw in ("W", "F") else 1
    return width


def _truncate_display(text: str, max_width: int) -> str:
    if max_width <= 0:
        return ""
    if _display_width(text) <= max_width:
        return text
    if max_width == 1:
        return "…"
    out: list[str] = []
    cur = 0
    limit = max_width - 1
    for ch in text:
        cw = 0 if unicodedata.combining(ch) else (2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1)
        if cur + cw > limit:
            break
        out.append(ch)
        cur += cw
    return "".join(out) + "…"


def _pad_display(text: str, width: int, align: str = "left") -> str:
    text = _truncate_display(text, width)
    pad = max(0, width - _display_width(text))
    if align == "right":
        return (" " * pad) + text
    return text + (" " * pad)


def _fmt_num(value: int, human: bool = False) -> str:
    if not human:
        return f"{value:,}"
    abs_v = abs(value)
    if abs_v >= 1_000_000_000:
        return f"{value / 1_000_000_000:.2f}B"
    if abs_v >= 1_000_000:
        return f"{value / 1_000_000:.2f}M"
    if abs_v >= 10_000:
        return f"{value / 1_000:.1f}K"
    return f"{value:,}"


def _fmt_pct(value: float) -> str:
    return f"{value:5.1f}%"


def _fmt_usd(value: float, human: bool = False) -> str:
    if value <= 0.0:
        return "$0.00"
    if human:
        if value >= 10_000.0:
            return f"${value / 1_000.0:.1f}K"
        if value >= 1_000.0:
            return f"${value / 1_000.0:.2f}K"
        if value >= 0.01:
            return f"${value:.2f}"
        if value >= 0.001:
            return f"${value:.3f}"
        return "<$0.001"
    if value >= 1_000.0:
        return f"${value:,.2f}"
    if value >= 0.01:
        return f"${value:.2f}"
    if value >= 0.0001:
        return f"${value:.4f}"
    return "<$0.0001"


def _fmt_cost(cost: CostBreakdown, calls: int, human: bool = False) -> str:
    if calls == 0:
        return "$0.00"
    if cost.is_fully_unpriced(calls):
        return "-"
    s = _fmt_usd(cost.total_usd, human)
    if cost.unpriced_calls > 0:
        s += "*"
    return s


def _fmt_time(ts: dt.datetime | None) -> str:
    if ts is None:
        return "-"
    return ts.strftime("%Y-%m-%d %H:%M")


def _fmt_time_sec(ts: dt.datetime | None) -> str:
    if ts is None:
        return "-"
    return ts.strftime("%Y-%m-%d %H:%M:%S")


def _render_table(
    headers: Sequence[str],
    rows: Sequence[Sequence[str]],
    alignments: Sequence[str],
    max_widths: Sequence[int | None] | None = None,
) -> str:
    ncols = len(headers)
    widths = [_display_width(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], _display_width(cell))
    if max_widths:
        for i, cap in enumerate(max_widths):
            if cap is not None and widths[i] > cap:
                widths[i] = max(cap, _display_width(headers[i]))

    def fmt_row(cols: Sequence[str]) -> str:
        return "  ".join(
            _pad_display(cols[i], widths[i], alignments[i]) for i in range(ncols)
        ).rstrip()

    lines = [
        fmt_row(headers),
        "  ".join("-" * widths[i] for i in range(ncols)),
    ]
    for row in rows:
        lines.append(fmt_row(row))
    return "\n".join(lines)


def _use_color(no_color_flag: bool) -> bool:
    if no_color_flag or os.environ.get("NO_COLOR"):
        return False
    return sys.stdout.isatty()


def _style(text: str, code: str, enabled: bool) -> str:
    if not enabled:
        return text
    return f"\033[{code}m{text}\033[0m"


# ---------- Aggregation ----------


def aggregate_sessions(sessions: Sequence[SessionInfo]) -> dict[str, Any]:
    total = TokenCounts()
    by_model: dict[str, TokenCounts] = defaultdict(TokenCounts)
    by_day: dict[str, TokenCounts] = defaultdict(TokenCounts)
    by_week: dict[str, TokenCounts] = defaultdict(TokenCounts)
    week_days: dict[str, set[str]] = defaultdict(set)
    by_month: dict[str, TokenCounts] = defaultdict(TokenCounts)
    by_hour: dict[int, TokenCounts] = {h: TokenCounts() for h in range(24)}
    by_workspace: dict[str, TokenCounts] = defaultdict(TokenCounts)
    workspace_sessions: dict[str, int] = defaultdict(int)
    by_tool: dict[str, int] = defaultdict(int)
    by_source: dict[str, TokenCounts] = defaultdict(TokenCounts)
    source_sessions: dict[str, int] = defaultdict(int)
    by_agent: dict[str, TokenCounts] = defaultdict(TokenCounts)
    agent_sessions: dict[str, int] = defaultdict(int)
    total_thinking_turns = 0
    total_active_secs = 0

    for s in sessions:
        total.merge(s.counts)
        total_active_secs += s.active_duration_secs
        by_workspace[s.workspace].merge(s.counts)
        workspace_sessions[s.workspace] += 1
        by_source[s.source].merge(s.counts)
        source_sessions[s.source] += 1
        fam = s.agent_family
        by_agent[fam].merge(s.counts)
        agent_sessions[fam] += 1
        for t in s.turns:
            if t.has_thinking:
                total_thinking_turns += 1
            for tool_name in t.tool_names:
                by_tool[tool_name] += 1
            by_model[t.model].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens, t.cost
            )
            if t.timestamp is not None:
                day_key = t.timestamp.strftime("%Y-%m-%d")
                week_key = iso_week_key(t.timestamp.date())
                month_key = t.timestamp.strftime("%Y-%m")
                by_hour[t.timestamp.hour].add_call(
                    t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens, t.cost
                )
                week_days[week_key].add(day_key)
            else:
                day_key = "unknown"
                week_key = "unknown"
                month_key = "unknown"
            by_day[day_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens, t.cost
            )
            by_week[week_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens, t.cost
            )
            by_month[month_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens, t.cost
            )

    return {
        "total": total,
        "by_model": by_model,
        "by_day": by_day,
        "by_week": by_week,
        "week_active_days": {k: len(v) for k, v in week_days.items()},
        "by_month": by_month,
        "by_hour": by_hour,
        "by_workspace": by_workspace,
        "workspace_sessions": workspace_sessions,
        "by_tool": dict(by_tool),
        "by_source": by_source,
        "source_sessions": source_sessions,
        "by_agent": by_agent,
        "agent_sessions": agent_sessions,
        "total_thinking_turns": total_thinking_turns,
        "total_active_secs": total_active_secs,
    }


_aggregate = aggregate_sessions


def _sort_sessions(sessions: Sequence[SessionInfo], sort_by: str) -> list[SessionInfo]:
    min_dt = dt.datetime.min.replace(tzinfo=dt.timezone.utc)
    if sort_by == "cost":
        return sorted(sessions, key=lambda s: (s.counts.total_cost_usd, s.counts.total_tokens), reverse=True)
    if sort_by == "total":
        return sorted(sessions, key=lambda s: s.counts.total_tokens, reverse=True)
    if sort_by == "non-cached":
        return sorted(sessions, key=lambda s: s.counts.non_cached_tokens, reverse=True)
    if sort_by == "hit":
        return sorted(
            sessions,
            key=lambda s: (s.counts.calls <= 1, s.counts.cache_hit_rate, -s.counts.total_tokens),
        )
    if sort_by == "output":
        return sorted(sessions, key=lambda s: s.counts.output_tokens, reverse=True)
    if sort_by == "input":
        return sorted(sessions, key=lambda s: s.counts.input_tokens, reverse=True)
    if sort_by == "calls":
        return sorted(sessions, key=lambda s: s.counts.calls, reverse=True)
    if sort_by == "peak":
        return sorted(sessions, key=lambda s: s.counts.peak_context, reverse=True)
    return sorted(sessions, key=lambda s: s.last_time or min_dt, reverse=True)


def _primary_model(s: SessionInfo) -> str:
    by_m: dict[str, tuple[bool, int]] = {}
    for t in s.turns:
        is_cp = t.step_type == "CHECKPOINT"
        prev_cp, prev_tok = by_m.get(t.model, (is_cp, 0))
        by_m[t.model] = (prev_cp and is_cp, prev_tok + t.total_tokens)
    if not by_m:
        return "-"
    return max(by_m.items(), key=lambda kv: (not kv[1][0], kv[1][1]))[0]


def _format_counts_row(label: str, c: TokenCounts, human: bool, include_cache_write: bool) -> list[str]:
    row = [
        label,
        str(c.calls),
        _fmt_num(c.input_tokens, human),
        _fmt_num(c.cache_read_tokens, human),
    ]
    if include_cache_write:
        row.append(_fmt_num(c.cache_write_tokens, human))
    row.extend(
        [
            _fmt_num(c.output_tokens, human),
            _fmt_num(c.total_tokens, human),
            _fmt_cost(c.cost, c.calls, human),
            _fmt_pct(c.cache_hit_rate),
            _fmt_num(c.peak_context, human),
        ]
    )
    return row


def _counts_headers(label_header: str, include_cache_write: bool) -> tuple[list[str], list[str]]:
    headers = [label_header, "Calls", "Input(new)", "CacheRead"]
    aligns = ["left", "right", "right", "right"]
    if include_cache_write:
        headers.append("CacheWrite")
        aligns.append("right")
    headers.extend(["Output", "Total", "Cost($)", "Hit%", "PeakCtx"])
    aligns.extend(["right", "right", "right", "right", "right"])
    return headers, aligns


# ---------- Privacy / Redaction ----------


def _is_unknown_workspace(ws: str) -> bool:
    t = ws.strip()
    return not t or t in ("(unknown)", "(未知工作区)")


def _alpha_label(idx: int) -> str:
    chars: list[str] = []
    n = idx
    while True:
        chars.append(chr(ord("A") + (n % 26)))
        if n < 26:
            break
        n = n // 26 - 1
    return "".join(reversed(chars))


_WARN_PATH_TAIL_RE = re.compile(
    r"((?:<redacted>|~/\.(?:claude|config/claude|codex|gemini/(?:antigravity-cli|antigravity-ide|antigravity|tmp))))[/\\][^\s:]+"
)


def _redact_data_dir(path: pathlib.Path) -> str:
    norm = str(path).replace("\\", "/").rstrip("/")
    lower = norm.lower()
    for suffix in ("antigravity-cli", "antigravity", "antigravity-ide", "tmp"):
        if lower == f".gemini/{suffix}" or lower.endswith(f"/.gemini/{suffix}"):
            return f"~/.gemini/{suffix}"
    for suffix in (".config/claude", ".claude", ".codex"):
        if lower == suffix or lower.endswith(f"/{suffix}"):
            return f"~/{suffix}"
    return "<redacted>"


def _redact_pricing_path(path: pathlib.Path | None) -> str:
    name = path.name if path is not None and path.name else "pricing.json"
    return f"./{name}"


def _redact_warning(
    msg: str,
    data_dir: pathlib.Path,
    data_dirs: Sequence[pathlib.Path] | None = None,
) -> str:
    out = msg
    all_dirs: list[pathlib.Path] = [data_dir]
    if data_dirs:
        for d in data_dirs:
            if d not in all_dirs:
                all_dirs.append(d)
    for d in sorted(all_dirs, key=lambda p: len(str(p)), reverse=True):
        redacted_dir = _redact_data_dir(d)
        raw_s = str(d)
        out = out.replace(raw_s, redacted_dir).replace(raw_s.replace("\\", "/"), redacted_dir)
    home = pathlib.Path.home()
    for sub, label in (
        (home / ".gemini" / "antigravity-cli", "~/.gemini/antigravity-cli"),
        (home / ".gemini" / "antigravity-ide", "~/.gemini/antigravity-ide"),
        (home / ".gemini" / "antigravity", "~/.gemini/antigravity"),
        (home / ".gemini" / "tmp", "~/.gemini/tmp"),
        (home / ".config" / "claude", "~/.config/claude"),
        (home / ".claude", "~/.claude"),
        (home / ".codex", "~/.codex"),
    ):
        raw_s = str(sub)
        out = out.replace(raw_s, label).replace(raw_s.replace("\\", "/"), label)
    home_s = str(home)
    if home_s:
        out = out.replace(home_s, "~").replace(home_s.replace("\\", "/"), "~")
    out = _WARN_PATH_TAIL_RE.sub(r"\1/<redacted-path>", out)
    return out


class Redactor:
    """Deterministic privacy mapper for session IDs, titles, workspaces, and local paths."""

    def __init__(self, sessions: Sequence[SessionInfo], agg: dict[str, Any]) -> None:
        self._session_ids: dict[str, str] = {}
        self._session_titles: dict[str, str] = {}
        self._parent_ids: dict[str, str] = {}
        self._parent_titles: dict[str, str] = {}
        self._workspaces: dict[str, str] = {}

        # Rank workspaces deterministically by total token volume descending.
        ws_items = [
            (ws, c.total_tokens)
            for ws, c in agg.get("by_workspace", {}).items()
            if not _is_unknown_workspace(ws)
        ]
        ws_items.sort(key=lambda kv: (-kv[1], kv[0]))
        for idx, (ws, _) in enumerate(ws_items):
            self._workspaces[ws] = f"Project {_alpha_label(idx)}"
        for ws in sorted({s.workspace for s in sessions if not _is_unknown_workspace(s.workspace)}):
            if ws not in self._workspaces:
                self._workspaces[ws] = f"Project {_alpha_label(len(self._workspaces))}"

        # Rank sessions deterministically by last_time descending (matches default display order).
        min_dt = dt.datetime.min.replace(tzinfo=dt.timezone.utc)
        ordered = sorted(
            sessions,
            key=lambda s: (-(s.last_time.timestamp() if s.last_time else min_dt.timestamp()), s.conversation_id),
        )
        present = {s.conversation_id for s in ordered}
        root_nums: dict[str, int] = {}
        root_idx = 0
        for s in ordered:
            attached_child = bool(s.is_subagent and s.parent_id and s.parent_id in present)
            if attached_child:
                continue
            root_idx += 1
            root_nums[s.conversation_id] = root_idx
            sid = f"S-{root_idx:03d}"
            title = f"Subagent #{root_idx}" if s.is_subagent else f"Session #{root_idx}"
            self._session_ids[s.conversation_id] = sid
            self._session_titles[s.conversation_id] = title
            self._parent_ids[s.conversation_id] = sid
            self._parent_titles[s.conversation_id] = title

        sub_counts: dict[str, int] = defaultdict(int)
        orphan_parent_idx = 0
        for s in ordered:
            if s.is_subagent and s.parent_id:
                pid = s.parent_id
                if pid in root_nums:
                    p_num = root_nums[pid]
                    sub_counts[pid] += 1
                    sub_n = sub_counts[pid]
                    self._session_ids[s.conversation_id] = f"S-{p_num:03d}.{sub_n}"
                    self._session_titles[s.conversation_id] = f"Subagent #{p_num}.{sub_n}"
                elif pid not in self._parent_ids:
                    orphan_parent_idx += 1
                    self._parent_ids[pid] = f"P-{orphan_parent_idx:03d}"
                    self._parent_titles[pid] = f"Parent #{orphan_parent_idx}"

    def session_id(self, cid: str) -> str:
        return self._session_ids.get(cid, "S-???")

    def parent_id(self, pid: str | None) -> str | None:
        if pid is None:
            return None
        return self._parent_ids.get(pid, self._session_ids.get(pid, "P-???"))

    def session_title(self, s: SessionInfo) -> str:
        return self._session_titles.get(
            s.conversation_id,
            "Subagent" if s.is_subagent else "Session",
        )

    def parent_title(self, pid: str | None, raw_title: str) -> str:
        if pid is not None:
            return self._parent_titles.get(pid, self._session_titles.get(pid, "Parent"))
        return "Parent" if raw_title else ""

    def workspace(self, ws: str) -> str:
        if _is_unknown_workspace(ws):
            return "(unknown)"
        return self._workspaces.get(ws, "Project ?")


# ---------- Output formatters ----------


def _group_sessions_for_display(
    sessions: Sequence[SessionInfo],
    *,
    group_subagents: bool,
) -> list[tuple[SessionInfo, TokenCounts, dt.datetime | None, set[str], str, int]]:
    """Build display rows: (session, agg_counts, last_time, models, primary_model, subagent_count)."""
    if not group_subagents:
        rows = []
        for s in sessions:
            rows.append((s, s.counts, s.last_time, set(s.models), _primary_model(s), 0))
        return rows

    present_keys = {(s.agent_family, s.conversation_id) for s in sessions}
    children_by_parent: dict[tuple[str, str], list[SessionInfo]] = defaultdict(list)
    roots: list[SessionInfo] = []
    for s in sessions:
        if s.is_subagent and s.parent_id and (s.agent_family, s.parent_id) in present_keys:
            children_by_parent[(s.agent_family, s.parent_id)].append(s)
        else:
            roots.append(s)

    rows = []
    for s in roots:
        counts = TokenCounts()
        counts.merge(s.counts)
        last_time = s.last_time
        models = set(s.models)
        kids = children_by_parent.get((s.agent_family, s.conversation_id), [])
        for k in kids:
            counts.merge(k.counts)
            models.update(k.models)
            if k.last_time is not None and (last_time is None or k.last_time > last_time):
                last_time = k.last_time
        # Compute primary model across root + grouped children
        by_m: dict[str, tuple[bool, int]] = {}
        for src in (s, *kids):
            for t in src.turns:
                is_cp = t.step_type == "CHECKPOINT"
                prev_cp, prev_tok = by_m.get(t.model, (is_cp, 0))
                by_m[t.model] = (prev_cp and is_cp, prev_tok + t.total_tokens)
        primary = max(by_m.items(), key=lambda kv: (not kv[1][0], kv[1][1]))[0] if by_m else "-"
        rows.append((s, counts, last_time, models, primary, len(kids)))
    return rows


def render_text(
    report: ScanReport,
    agg: dict[str, Any],
    *,
    view: str,
    sort_by: str,
    limit: int,
    human: bool,
    tz_label: str,
    since: dt.date | None,
    until: dt.date | None,
    model_filter: str | None,
    session_filter: str | None,
    color: bool,
    show_subagents: bool = False,
    redact: bool = False,
    statusline: dict[str, Any] | None = None,
    agent_filter: set[str] | None = None,
) -> str:
    total: TokenCounts = agg["total"]
    include_cw = total.cache_write_tokens > 0
    redactor = Redactor(report.sessions, agg) if redact else None
    out: list[str] = []

    hdr_tag = "Read-Only, Redacted" if redact else "Read-Only"
    header = _style(f"Gauge Quota & Token Usage [{hdr_tag}]", "1;36", color)
    out.append(header)
    dirs_list = report.data_dirs or [report.data_dir]
    data_dir_disp = ", ".join(_redact_data_dir(d) if redact else str(d) for d in dirs_list)
    out.append(f"Data Dir : {data_dir_disp}")

    filters: list[str] = [f"TZ={tz_label}"]
    if since or until:
        filters.append(f"Date={since or '*'}..{until or '*'}")
    if agent_filter:
        filters.append(f"Agents={','.join(sorted(agent_filter))}")
    if model_filter:
        filters.append(f"Model~={model_filter}")
    if session_filter:
        filters.append("Session=<redacted>" if redact else f"Session={session_filter}*")
    if report.archived_sessions_count > 0:
        filters.append(f"Archived={report.archived_sessions_count}")
    out.append(f"Scope    : {', '.join(filters)}")
    out.append("")

    if not report.sessions:
        out.append("No matching token usage records found.")
        return "\n".join(out)

    # Overview block
    out.append(_style("Overview", "1;33", color))
    sub_cnt = sum(1 for s in report.sessions if s.is_subagent)
    root_cnt = len(report.sessions) - sub_cnt
    sess_str = (
        f"{root_cnt} (+{sub_cnt} subagents)"
        if sub_cnt > 0
        else str(len(report.sessions))
    )
    overview_lines = [
        f"  Sessions         : {sess_str} (LLM calls: {total.calls:,})",
        f"  Input (uncached) : {_fmt_num(total.input_tokens, human)}",
        f"  Cache Read       : {_fmt_num(total.cache_read_tokens, human)} (Hit Rate: {total.cache_hit_rate:.1f}%)",
    ]
    if include_cw:
        overview_lines.append(f"  Cache Write      : {_fmt_num(total.cache_write_tokens, human)}")
    overview_lines.extend(
        [
            f"  Total Input      : {_fmt_num(total.total_input_tokens, human)} (Input + Cache)",
            f"  Output           : {_fmt_num(total.output_tokens, human)}",
            f"  Total Tokens     : {_fmt_num(total.total_tokens, human)} (Non-cached: {_fmt_num(total.non_cached_tokens, human)})",
            f"  Peak Context     : {_fmt_num(total.peak_context, human)}",
            (
                f"  Estimated Cost   : {_fmt_cost(total.cost, total.calls, human)}"
                f" (Input {_fmt_usd(total.cost.input_plus_write_usd, human)}"
                f" / Cache {_fmt_usd(total.cost.cache_read_usd, human)}"
                f" / Output {_fmt_usd(total.cost.output_usd, human)};"
                f" Cache Saved: {_fmt_usd(total.cost.cache_saved_usd, human)})"
            ),
        ]
    )
    if agg.get("total_active_secs", 0) > 0:
        think_turns = agg.get("total_thinking_turns", 0)
        tool_total = sum(agg.get("by_tool", {}).values())
        overview_lines.append(
            f"  Activity         : Active ~{_fmt_duration(agg['total_active_secs'])}"
            f" | Thinking Turns: {think_turns}/{total.calls} | Tool Calls: {tool_total:,}"
        )
    if statusline and statusline.get("buckets"):
        prov = statusline.get("provenance", "official")
        b_strs = []
        for b in statusline["buckets"][:3]:
            if b.get("disabled"):
                continue
            rp = b.get("remaining_pct")
            if rp is not None:
                rem = f" ({_fmt_countdown(b['live_remaining_secs'])})" if b.get("live_remaining_secs") is not None else ""
                b_strs.append(f"{b['id']}={rp:.0f}%{rem}")
        if b_strs:
            tier = f" [{statusline['plan_tier']}]" if statusline.get("plan_tier") else ""
            overview_lines.append(f"  Official Quota   : {', '.join(b_strs)} ({prov}{tier})")
    out.extend(overview_lines)

    # Single-session detailed turn-by-turn view
    if session_filter and len(report.sessions) == 1:
        s = report.sessions[0]
        compactions = sum(1 for t in s.turns if t.step_type == "CHECKPOINT")
        miss_cnt = 0
        miss_tok = 0
        for idx, t in enumerate(s.turns):
            if idx > 0 and t.step_type != "CHECKPOINT" and t.total_input_tokens >= 30_000 and t.cache_hit_rate < 50.0:
                miss_cnt += 1
                miss_tok += t.input_tokens
        out.append("")
        sid_disp = redactor.session_id(s.conversation_id) if redactor else s.conversation_id
        title_disp = redactor.session_title(s) if redactor else s.title
        ws_disp = redactor.workspace(s.workspace) if redactor else s.workspace
        out.append(_style(f"Session Detail: {sid_disp}", "1;33", color))
        out.append(f"  Title     : {title_disp}")
        out.append(f"  Source    : {source_display_label(s.source)}")
        if s.is_subagent and s.parent_id:
            if redactor:
                pid_disp = redactor.parent_id(s.parent_id) or "P-???"
                pt_disp = redactor.parent_title(s.parent_id, s.parent_title)
                p_lbl = f"{pid_disp} ({pt_disp})" if pt_disp else pid_disp
            else:
                p_lbl = f"{s.parent_id[:8]} ({s.parent_title})" if s.parent_title else s.parent_id[:8]
            out.append(f"  Parent    : {p_lbl}")
        out.append(f"  Workspace : {ws_disp}")
        out.append(f"  Range     : {_fmt_time_sec(s.first_time)} -> {_fmt_time_sec(s.last_time)}")
        out.append(
            f"  Est. Cost : {_fmt_cost(s.counts.cost, s.counts.calls, human)}"
            f" (Cache Saved: {_fmt_usd(s.counts.cost.cache_saved_usd, human)})"
        )
        if compactions or miss_cnt:
            parts = []
            if compactions:
                parts.append(f"Compactions={compactions}")
            if miss_cnt:
                parts.append(f"CacheMisses={miss_cnt} ({_fmt_num(miss_tok, human)} uncached)")
            out.append(f"  Events    : {', '.join(parts)}")
        out.append("")
        t_headers = ["Step", "Time", "Gap", "Model", "Input(new)", "CacheRead"]
        t_aligns = ["right", "left", "right", "left", "right", "right"]
        if include_cw:
            t_headers.append("CacheWrite")
            t_aligns.append("right")
        t_headers.extend(["Output", "CtxSize", "Cost($)", "Hit%"])
        t_aligns.extend(["right", "right", "right", "right"])
        t_rows = []
        prev_ts: dt.datetime | None = None
        for t in s.turns:
            if prev_ts is not None and t.timestamp is not None:
                sec = max(0, int((t.timestamp - prev_ts).total_seconds()))
                if sec < 60:
                    gap_str = f"{sec}s"
                elif sec < 3600:
                    gap_str = f"{sec // 60}m{sec % 60:02d}s"
                else:
                    gap_str = f"{sec // 3600}h{(sec % 3600) // 60:02d}m"
            else:
                gap_str = "-"
            if t.timestamp is not None:
                prev_ts = t.timestamp
            r = [
                str(t.step_index) if t.step_index >= 0 else "-",
                _fmt_time_sec(t.timestamp),
                gap_str,
                t.model,
                _fmt_num(t.input_tokens, human),
                _fmt_num(t.cache_read_tokens, human),
            ]
            if include_cw:
                r.append(_fmt_num(t.cache_write_tokens, human))
            r.extend(
                [
                    _fmt_num(t.output_tokens, human),
                    _fmt_num(t.total_input_tokens, human),
                    _fmt_cost(t.cost, 1, human),
                    _fmt_pct(t.cache_hit_rate),
                ]
            )
            t_rows.append(r)
        out.append(_render_table(t_headers, t_rows, t_aligns))
        return "\n".join(out)

    show_model = view in ("summary", "all", "model")
    show_daily = view in ("summary", "all", "daily")
    show_weekly = view in ("all", "weekly")
    show_monthly = view in ("all", "monthly")
    show_hourly = view in ("all", "hourly")
    show_workspace = view in ("all", "workspace")
    show_sessions = view in ("summary", "all", "session")
    show_tools = view in ("all", "tools")
    show_sources = view in ("all", "sources")

    if show_sources and agg.get("by_source"):
        out.append("")
        out.append(_style("By Source", "1;33", color))
        src_headers = ["Source", "Agent", "Sessions", "Calls", "Input(new)", "CacheRead"]
        src_aligns = ["left", "left", "right", "right", "right", "right"]
        if include_cw:
            src_headers.append("CacheWrite")
            src_aligns.append("right")
        src_headers.extend(["Output", "Total", "Cost($)", "Hit%", "PeakCtx"])
        src_aligns.extend(["right", "right", "right", "right", "right"])
        src_items = sorted(
            agg["by_source"].items(),
            key=lambda kv: (-kv[1].total_tokens, kv[0]),
        )
        src_rows = []
        for sk, c in src_items:
            row = [
                sk,
                agent_display_label(source_to_agent_family(sk)),
                str(agg["source_sessions"][sk]),
                str(c.calls),
                _fmt_num(c.input_tokens, human),
                _fmt_num(c.cache_read_tokens, human),
            ]
            if include_cw:
                row.append(_fmt_num(c.cache_write_tokens, human))
            row.extend([
                _fmt_num(c.output_tokens, human),
                _fmt_num(c.total_tokens, human),
                _fmt_cost(c.cost, c.calls, human),
                _fmt_pct(c.cache_hit_rate),
                _fmt_num(c.peak_context, human),
            ])
            src_rows.append(row)
        out.append(_render_table(src_headers, src_rows, src_aligns))

    if show_tools and agg.get("by_tool"):
        out.append("")
        out.append(_style("By Tool Calls", "1;33", color))
        t_items = sorted(agg["by_tool"].items(), key=lambda kv: (-kv[1], kv[0]))
        total_tc = max(1, sum(v for _, v in t_items))
        t_rows = [[name, str(cnt), f"{(cnt / total_tc) * 100.0:5.1f}%"] for name, cnt in t_items[:25]]
        out.append(_render_table(["Tool", "Calls", "Share%"], t_rows, ["left", "right", "right"]))

    if show_model:
        out.append("")
        out.append(_style("By Model", "1;33", color))
        headers, aligns = _counts_headers("Model", include_cw)
        primary_items = sorted(
            ((k, v) for k, v in agg["by_model"].items() if k != "compaction"),
            key=lambda kv: kv[1].total_tokens,
            reverse=True,
        )
        compaction_items = [
            (k, v) for k, v in agg["by_model"].items() if k == "compaction"
        ]
        rows = [_format_counts_row(k, v, human, include_cw) for k, v in primary_items]
        if compaction_items:
            rows.extend(
                _format_counts_row(f"{k} (secondary)", v, human, include_cw)
                for k, v in compaction_items
            )
        out.append(_render_table(headers, rows, aligns))

    if show_monthly:
        out.append("")
        out.append(_style("By Month", "1;33", color))
        headers, aligns = _counts_headers("Month", include_cw)
        items = sorted(agg["by_month"].items(), key=lambda kv: kv[0], reverse=True)
        rows = [_format_counts_row(k, v, human, include_cw) for k, v in items]
        out.append(_render_table(headers, rows, aligns))

    if show_weekly:
        out.append("")
        out.append(_style("By Week (ISO Mon-Sun)", "1;33", color))
        headers, aligns = _counts_headers("Week", include_cw)
        items = sorted(agg["by_week"].items(), key=lambda kv: kv[0], reverse=True)
        rows = [_format_counts_row(format_week_label(k), v, human, include_cw) for k, v in items]
        out.append(_render_table(headers, rows, aligns))

    if show_daily:
        out.append("")
        out.append(_style("By Date", "1;33", color))
        headers, aligns = _counts_headers("Date", include_cw)
        items = sorted(agg["by_day"].items(), key=lambda kv: kv[0], reverse=True)
        if view == "summary" and limit > 0 and len(items) > limit:
            items = items[:limit]
        rows = [_format_counts_row(k, v, human, include_cw) for k, v in items]
        out.append(_render_table(headers, rows, aligns))

    if show_hourly:
        out.append("")
        out.append(_style("By Hour of Day", "1;33", color))
        headers, aligns = _counts_headers("Hour", include_cw)
        items = [(f"{h:02d}:00", c) for h, c in sorted(agg["by_hour"].items()) if c.calls > 0]
        rows = [_format_counts_row(k, v, human, include_cw) for k, v in items]
        out.append(_render_table(headers, rows, aligns))

    if show_workspace:
        out.append("")
        out.append(_style("By Workspace", "1;33", color))
        headers = ["Workspace", "Sessions", "Calls", "Input(new)", "CacheRead", "Output", "Total", "Cost($)", "Hit%"]
        aligns = ["left", "right", "right", "right", "right", "right", "right", "right", "right"]
        items = sorted(
            agg["by_workspace"].items(),
            key=lambda kv: kv[1].total_tokens,
            reverse=True,
        )
        rows = [
            [
                redactor.workspace(ws) if redactor else ws,
                str(agg["workspace_sessions"][ws]),
                str(c.calls),
                _fmt_num(c.input_tokens, human),
                _fmt_num(c.cache_read_tokens, human),
                _fmt_num(c.output_tokens, human),
                _fmt_num(c.total_tokens, human),
                _fmt_cost(c.cost, c.calls, human),
                _fmt_pct(c.cache_hit_rate),
            ]
            for ws, c in items
        ]
        out.append(_render_table(headers, rows, aligns, max_widths=[44, None, None, None, None, None, None, None, None]))

    if show_sessions:
        should_group = (not show_subagents) and (not model_filter) and (not session_filter)
        grouped = _group_sessions_for_display(report.sessions, group_subagents=should_group)
        min_dt = dt.datetime.min.replace(tzinfo=dt.timezone.utc)
        if sort_by == "cost":
            grouped.sort(key=lambda r: (r[1].total_cost_usd, r[1].total_tokens), reverse=True)
        elif sort_by == "total":
            grouped.sort(key=lambda r: r[1].total_tokens, reverse=True)
        elif sort_by == "non-cached":
            grouped.sort(key=lambda r: r[1].non_cached_tokens, reverse=True)
        elif sort_by == "hit":
            grouped.sort(key=lambda r: (r[1].calls <= 1, r[1].cache_hit_rate, -r[1].total_tokens))
        elif sort_by == "output":
            grouped.sort(key=lambda r: r[1].output_tokens, reverse=True)
        elif sort_by == "input":
            grouped.sort(key=lambda r: r[1].input_tokens, reverse=True)
        elif sort_by == "calls":
            grouped.sort(key=lambda r: r[1].calls, reverse=True)
        elif sort_by == "peak":
            grouped.sort(key=lambda r: r[1].peak_context, reverse=True)
        else:
            grouped.sort(key=lambda r: r[2] or min_dt, reverse=True)

        total_cnt = len(grouped)
        shown = grouped if (view == "session" or limit <= 0) else grouped[:limit]
        out.append("")
        title_suffix = f" (showing {len(shown)}/{total_cnt}, sort={sort_by})" if len(shown) < total_cnt else f" ({total_cnt}, sort={sort_by})"
        out.append(_style(f"Sessions{title_suffix}", "1;33", color))
        has_non_agy = any(s.agent_family != "antigravity" for s in report.sessions)
        if has_non_agy:
            s_headers = ["ID", "Src", "Last Active", "Calls", "Model", "Hit%", "Input(new)", "CacheRead", "Output", "Total", "Cost($)", "PeakCtx", "Title"]
            s_aligns = ["left", "left", "left", "right", "left", "right", "right", "right", "right", "right", "right", "right", "left"]
            s_max_widths: list[int | None] = [8, 6, 16, None, None, None, None, None, None, None, None, None, 42]
        else:
            s_headers = ["ID", "Last Active", "Calls", "Model", "Hit%", "Input(new)", "CacheRead", "Output", "Total", "Cost($)", "PeakCtx", "Title"]
            s_aligns = ["left", "left", "right", "left", "right", "right", "right", "right", "right", "right", "right", "left"]
            s_max_widths = [8, 16, None, None, None, None, None, None, None, None, None, 42]
        s_rows = []
        for s, counts, last_time, models, primary, sub_count in shown:
            real_models = [m for m in sorted(models) if m != "compaction"]
            models_str = primary if len(real_models) <= 1 else f"{primary}+{len(real_models)-1}"
            raw_or_redacted_title = redactor.session_title(s) if redactor else s.title
            if sub_count > 0:
                suffix = f" (+{sub_count} sub)"
                title_cell = _truncate_display(raw_or_redacted_title, max(8, 42 - _display_width(suffix))) + suffix
            elif s.is_subagent:
                title_cell = f"↳ {raw_or_redacted_title}"
            else:
                title_cell = raw_or_redacted_title
            row_cells = [redactor.session_id(s.conversation_id) if redactor else s.conversation_id[:8]]
            if has_non_agy:
                row_cells.append(source_badge(s.source))
            row_cells.extend(
                [
                    _fmt_time(last_time),
                    str(counts.calls),
                    models_str,
                    _fmt_pct(counts.cache_hit_rate),
                    _fmt_num(counts.input_tokens, human),
                    _fmt_num(counts.cache_read_tokens, human),
                    _fmt_num(counts.output_tokens, human),
                    _fmt_num(counts.total_tokens, human),
                    _fmt_cost(counts.cost, counts.calls, human),
                    _fmt_num(counts.peak_context, human),
                    title_cell,
                ]
            )
            s_rows.append(row_cells)
        out.append(
            _render_table(
                s_headers,
                s_rows,
                s_aligns,
                max_widths=s_max_widths,
            )
        )

    return "\n".join(out)


def render_compact(
    report: ScanReport,
    agg: dict[str, Any],
    *,
    tz: dt.tzinfo,
    statusline: dict[str, Any] | None = None,
) -> str:
    today_key = dt.datetime.now(tz).strftime("%Y-%m-%d")
    tc: TokenCounts = agg["by_day"].get(today_key, TokenCounts())
    parts = [f"AGY today={_fmt_num(tc.total_tokens, True)} ({_fmt_cost(tc.cost, tc.calls, True)})"]
    if statusline and statusline.get("buckets"):
        b0 = next((b for b in statusline["buckets"] if not b.get("disabled") and b.get("remaining_pct") is not None), None)
        if b0 is not None:
            parts.append(f"quota={b0['id']}:{b0['remaining_pct']:.0f}%")
    return " · ".join(parts)


def evaluate_budget_exit_code(
    agg: dict[str, Any],
    *,
    tz: dt.tzinfo,
    daily_usd: float = 0.0,
    weekly_usd: float = 0.0,
    warn_pct: float = 70.0,
    crit_pct: float = 90.0,
) -> tuple[int, list[str]]:
    code = 0
    notes: list[str] = []
    now_date = dt.datetime.now(tz).date()
    today_key = now_date.strftime("%Y-%m-%d")
    today_cost = agg["by_day"].get(today_key, TokenCounts()).total_cost_usd
    if daily_usd > 0:
        pct = (today_cost / daily_usd) * 100.0
        if pct >= crit_pct:
            code = max(code, 20)
            notes.append(f"CRITICAL: daily ${today_cost:.2f} / ${daily_usd:.2f} ({pct:.0f}%)")
        elif pct >= warn_pct:
            code = max(code, 10)
            notes.append(f"WARN: daily ${today_cost:.2f} / ${daily_usd:.2f} ({pct:.0f}%)")
    if weekly_usd > 0:
        iso = now_date.isocalendar()
        week_key = f"{iso.year:04d}-W{iso.week:02d}"
        week_cost = agg["by_week"].get(week_key, TokenCounts()).total_cost_usd
        pct = (week_cost / weekly_usd) * 100.0
        if pct >= crit_pct:
            code = max(code, 20)
            notes.append(f"CRITICAL: weekly ${week_cost:.2f} / ${weekly_usd:.2f} ({pct:.0f}%)")
        elif pct >= warn_pct:
            code = max(code, 10)
            notes.append(f"WARN: weekly ${week_cost:.2f} / ${weekly_usd:.2f} ({pct:.0f}%)")
    return code, notes


def render_json(
    report: ScanReport,
    agg: dict[str, Any],
    *,
    sort_by: str,
    tz_label: str,
    include_turns: bool,
    redact: bool = False,
    statusline: dict[str, Any] | None = None,
) -> str:
    redactor = Redactor(report.sessions, agg) if redact else None
    ordered = _sort_sessions(report.sessions, sort_by)
    sessions_json = []
    for s in ordered:
        if redactor:
            cid_val = redactor.session_id(s.conversation_id)
            title_val = redactor.session_title(s)
            ws_val = redactor.workspace(s.workspace)
            pid_val = redactor.parent_id(s.parent_id)
            ptitle_val = redactor.parent_title(s.parent_id, s.parent_title) or None
        else:
            cid_val = s.conversation_id
            title_val = s.title
            ws_val = s.workspace
            pid_val = s.parent_id
            ptitle_val = s.parent_title or None
        entry: dict[str, Any] = {
            "conversation_id": cid_val,
            "title": title_val,
            "workspace": ws_val,
            "source": s.source,
            "agent": s.agent_family,
            "is_archived": s.is_archived,
            "is_subagent": s.is_subagent,
            "parent_id": pid_val,
            "parent_title": ptitle_val,
            "active_duration_secs": s.active_duration_secs,
            "thinking_turns": s.thinking_turns,
            "tool_counts": s.tool_counts(),
            "models": sorted(s.models),
            "first_time": s.first_time.isoformat() if s.first_time else None,
            "last_time": s.last_time.isoformat() if s.last_time else None,
            **s.counts.to_dict(),
        }
        if include_turns:
            entry["turns"] = [
                {
                    "step_index": t.step_index,
                    "timestamp": t.timestamp.isoformat() if t.timestamp else None,
                    "model": t.model,
                    "type": t.step_type,
                    "status": t.status,
                    "input_tokens": t.input_tokens,
                    "cache_read_tokens": t.cache_read_tokens,
                    "cache_write_tokens": t.cache_write_tokens,
                    "cache_write_1h_tokens": t.cache_write_1h_tokens,
                    "total_input_tokens": t.total_input_tokens,
                    "output_tokens": t.output_tokens,
                    "reasoning_tokens": t.reasoning_tokens,
                    "total_tokens": t.total_tokens,
                    "cache_hit_rate_pct": round(t.cache_hit_rate, 2),
                    "cost_usd": round(t.cost.total_usd, 6),
                }
                for t in s.turns
            ]
        sessions_json.append(entry)

    by_ws_json: dict[str, Any] = {}
    for k, v in sorted(agg["by_workspace"].items()):
        ws_key = redactor.workspace(k) if redactor else k
        by_ws_json[ws_key] = {"sessions": agg["workspace_sessions"][k], **v.to_dict()}

    payload: dict[str, Any] = {
        "data_dir": _redact_data_dir(report.data_dir) if redact else str(report.data_dir),
        "data_dirs": [_redact_data_dir(d) if redact else str(d) for d in (report.data_dirs or [report.data_dir])],
        "timezone": tz_label,
        "redacted": redact,
        "session_count": len(report.sessions),
        "archived_sessions_count": report.archived_sessions_count,
        "overview": agg["total"].to_dict(),
        "activity": {
            "active_duration_secs": agg.get("total_active_secs", 0),
            "thinking_turns": agg.get("total_thinking_turns", 0),
        },
        "official_quota": statusline,
        "by_tool": dict(sorted(agg.get("by_tool", {}).items(), key=lambda kv: (-kv[1], kv[0]))),
        "by_source": {
            k: {"sessions": agg["source_sessions"][k], **v.to_dict()}
            for k, v in sorted(agg.get("by_source", {}).items())
        },
        "by_agent": {
            k: {"sessions": agg["agent_sessions"][k], **v.to_dict()}
            for k, v in sorted(agg.get("by_agent", {}).items())
        },
        "by_model": {k: v.to_dict() for k, v in sorted(agg["by_model"].items())},
        "by_date": {k: v.to_dict() for k, v in sorted(agg["by_day"].items())},
        "by_week": {
            k: {
                "label": format_week_label(k),
                "active_days": agg["week_active_days"].get(k, 0),
                **v.to_dict(),
            }
            for k, v in sorted(agg["by_week"].items())
        },
        "by_month": {k: v.to_dict() for k, v in sorted(agg["by_month"].items())},
        "by_hour": {f"{h:02d}": v.to_dict() for h, v in sorted(agg["by_hour"].items()) if v.calls > 0},
        "by_workspace": by_ws_json,
        "sessions": sessions_json,
        "warnings": [
            _redact_warning(w, report.data_dir, report.data_dirs) for w in report.warnings
        ] if redact else report.warnings,
    }
    return json.dumps(payload, ensure_ascii=False, indent=2)


def render_csv(
    report: ScanReport,
    agg: dict[str, Any],
    *,
    view: str,
    sort_by: str,
    redact: bool = False,
) -> str:
    redactor = Redactor(report.sessions, agg) if redact else None
    buf = io.StringIO()
    writer = csv.writer(buf, lineterminator="\n")

    if view == "model":
        writer.writerow(["model", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "cost_usd", "cache_saved_usd"])
        for k, c in sorted(agg["by_model"].items(), key=lambda kv: kv[1].total_tokens, reverse=True):
            writer.writerow([k, c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context, round(c.cost.total_usd, 6), round(c.cost.cache_saved_usd, 6)])
    elif view in ("daily", "weekly", "monthly", "hourly"):
        if view == "daily":
            source, key_col = agg["by_day"], "date"
            items = sorted(source.items())
        elif view == "weekly":
            source, key_col = agg["by_week"], "week"
            items = [(format_week_label(k), c) for k, c in sorted(source.items())]
        elif view == "monthly":
            source, key_col = agg["by_month"], "month"
            items = sorted(source.items())
        else:
            source, key_col = agg["by_hour"], "hour"
            items = [(f"{h:02d}:00", c) for h, c in sorted(source.items()) if c.calls > 0]
        writer.writerow([key_col, "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "cost_usd", "cache_saved_usd"])
        for k, c in items:
            writer.writerow([k, c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context, round(c.cost.total_usd, 6), round(c.cost.cache_saved_usd, 6)])
    elif view == "sources":
        writer.writerow(["source", "agent", "sessions", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "cost_usd", "cache_saved_usd"])
        for k, c in sorted(agg.get("by_source", {}).items(), key=lambda kv: (-kv[1].total_tokens, kv[0])):
            writer.writerow([k, source_to_agent_family(k), agg["source_sessions"][k], c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context, round(c.cost.total_usd, 6), round(c.cost.cache_saved_usd, 6)])
    elif view == "workspace":
        writer.writerow(["workspace", "sessions", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "cost_usd", "cache_saved_usd"])
        for k, c in sorted(agg["by_workspace"].items(), key=lambda kv: kv[1].total_tokens, reverse=True):
            ws_cell = redactor.workspace(k) if redactor else k
            writer.writerow([ws_cell, agg["workspace_sessions"][k], c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context, round(c.cost.total_usd, 6), round(c.cost.cache_saved_usd, 6)])
    else:
        writer.writerow(["conversation_id", "first_time", "last_time", "models", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "cost_usd", "cache_saved_usd", "workspace", "title"])
        for s in _sort_sessions(report.sessions, sort_by):
            c = s.counts
            writer.writerow([
                redactor.session_id(s.conversation_id) if redactor else s.conversation_id,
                s.first_time.isoformat() if s.first_time else "",
                s.last_time.isoformat() if s.last_time else "",
                ";".join(sorted(s.models)),
                c.calls,
                c.input_tokens,
                c.cache_read_tokens,
                c.cache_write_tokens,
                c.total_input_tokens,
                c.output_tokens,
                c.total_tokens,
                c.non_cached_tokens,
                round(c.cache_hit_rate, 2),
                c.peak_context,
                round(c.cost.total_usd, 6),
                round(c.cost.cache_saved_usd, 6),
                redactor.workspace(s.workspace) if redactor else s.workspace,
                redactor.session_title(s) if redactor else s.title,
            ])
    return buf.getvalue().rstrip("\n")


# ---------- CLI ----------


def _parse_date_arg(value: str) -> dt.date:
    try:
        return dt.date.fromisoformat(value.strip())
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"Invalid date '{value}', expected YYYY-MM-DD.") from exc


def _fmt_rate_cell(v: float) -> str:
    s = f"{round(v, 4):.4f}".rstrip("0").rstrip(".")
    if "." in s:
        dec = len(s.split(".", 1)[1])
        return f"${v:.2f}" if dec < 2 else f"${s}"
    return f"${v:.2f}"


def render_pricing_table(
    pricing: PricingCatalog,
    observed_models: Sequence[str],
    date_obj: dt.date,
    color: bool,
    redact: bool = False,
) -> str:
    rows_data = pricing.build_table(observed_models, date_obj)
    out = [_style("API Pricing Table (USD / 1M tokens)", "1;33", color)]
    if pricing.override_path is not None:
        exists = pricing.override_path.is_file() and not pricing.override_path.is_symlink()
        path_disp = _redact_pricing_path(pricing.override_path) if redact else str(pricing.override_path)
        out.append(f"Override : {path_disp} ({'loaded' if exists else 'not created yet'})")
    out.append("")
    headers = ["Model / Rule", "Source", "Matched", "Input", "CacheRead", "CacheWrite", "Output"]
    aligns = ["left", "left", "left", "right", "right", "right", "right"]
    t_rows = []
    for item in rows_data:
        r: ModelRate | None = item["rate"]
        if r is not None:
            t_rows.append([
                item["model"],
                item["source"],
                item["rule"],
                _fmt_rate_cell(r.input),
                _fmt_rate_cell(r.cache_read),
                _fmt_rate_cell(r.cache_write),
                _fmt_rate_cell(r.output),
            ])
        else:
            t_rows.append([item["model"], item["source"], item["rule"], "-", "-", "-", "-"])
    out.append(_render_table(headers, t_rows, aligns))
    return "\n".join(out)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gauge_stats.py",
        description="Gauge — strictly read-only multi-agent token usage analyzer (Antigravity, Claude Code, Codex CLI, Gemini CLI).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Examples:\n"
            "  python gauge_stats.py                        # Auto-discover all installed Tier-1 agents\n"
            "  python gauge_stats.py --today --human        # Only today's usage in K/M format\n"
            "  python gauge_stats.py --agy-only             # Antigravity (CLI/App/IDE) sources only\n"
            "  python gauge_stats.py --agents claude,codex  # Specific agent families only\n"
            "  python gauge_stats.py --view sources         # Breakdown by agent source\n"
            "  python gauge_stats.py --format json          # Export structured JSON to stdout\n"
        ),
    )
    parser.add_argument(
        "--data-dir",
        type=pathlib.Path,
        default=None,
        help="Single Antigravity data directory (overrides auto-discovery)",
    )
    parser.add_argument(
        "--single-source",
        action="store_true",
        help="Scan only ~/.gemini/antigravity-cli instead of all installed agents",
    )
    parser.add_argument(
        "--agy-only",
        action="store_true",
        help="Scan only Antigravity sources (CLI/App/IDE), skipping Claude Code, Codex CLI, and Gemini CLI",
    )
    parser.add_argument(
        "--agent",
        "--agents",
        dest="agents",
        action="append",
        default=None,
        help="Comma-separated agent filter: agy/antigravity, claude, codex, gemini (default: all installed Tier-1 agents)",
    )
    parser.add_argument(
        "--view",
        choices=("summary", "all", "daily", "weekly", "monthly", "hourly", "model", "session", "workspace", "tools", "sources", "pricing"),
        default="summary",
        help="Report sections to display (default: summary)",
    )
    parser.add_argument(
        "--format",
        choices=("table", "json", "csv"),
        default="table",
        help="Output format (default: table)",
    )
    parser.add_argument(
        "--compact",
        action="store_true",
        help="Print a single-line prompt-ready summary (today + official quota)",
    )
    parser.add_argument(
        "--no-archive",
        action="store_true",
        help="Do not read local archive.json backfill",
    )
    parser.add_argument(
        "--statusline-state",
        type=pathlib.Path,
        default=DEFAULT_STATUSLINE_PATH,
        help="Optional read-only statusline_state.json written by agy-statusline",
    )
    parser.add_argument(
        "--budget-daily",
        type=float,
        default=None,
        help="Daily USD budget threshold for --check-budget (overrides budget.json)",
    )
    parser.add_argument(
        "--budget-weekly",
        type=float,
        default=None,
        help="Weekly USD budget threshold for --check-budget (overrides budget.json)",
    )
    parser.add_argument(
        "--warn-pct",
        type=float,
        default=None,
        help="Warning threshold percentage of budget (default: 70)",
    )
    parser.add_argument(
        "--crit-pct",
        type=float,
        default=None,
        help="Critical threshold percentage of budget (default: 90)",
    )
    parser.add_argument(
        "--check-budget",
        action="store_true",
        help="Exit with code 10 (warn) or 20 (critical) when budget thresholds are exceeded",
    )
    parser.add_argument(
        "--today",
        action="store_true",
        help="Filter to today only",
    )
    parser.add_argument(
        "--days",
        type=int,
        default=None,
        help="Filter to the last N days (inclusive of today; e.g. --days 7)",
    )
    parser.add_argument(
        "--since",
        type=_parse_date_arg,
        default=None,
        help="Start date inclusive (YYYY-MM-DD)",
    )
    parser.add_argument(
        "--until",
        type=_parse_date_arg,
        default=None,
        help="End date inclusive (YYYY-MM-DD)",
    )
    parser.add_argument(
        "--utc",
        action="store_true",
        help="Group and display timestamps in UTC instead of local timezone",
    )
    parser.add_argument(
        "--model",
        type=str,
        default=None,
        help="Case-insensitive substring filter on model name (e.g. gemini-4-neon)",
    )
    parser.add_argument(
        "--session",
        type=str,
        default=None,
        help="Filter by conversation ID prefix (shows turn-by-turn detail if 1 match)",
    )
    parser.add_argument(
        "--workspace",
        type=str,
        default=None,
        help="Case-insensitive substring filter on workspace path (e.g. agent-home)",
    )
    parser.add_argument(
        "--pricing",
        type=pathlib.Path,
        default=DEFAULT_PRICING_PATH,
        help=f"Optional local pricing.json override (read-only; default: {DEFAULT_PRICING_PATH})",
    )
    parser.add_argument(
        "--sort",
        choices=("time", "cost", "total", "non-cached", "hit", "output", "input", "calls", "peak"),
        default="time",
        help="Session sort order (default: time)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=15,
        help="Max rows for summary session/daily lists; 0 for unlimited (default: 15)",
    )
    parser.add_argument(
        "--human",
        action="store_true",
        help="Format large token numbers with K/M/B suffixes",
    )
    parser.add_argument(
        "--show-subagents",
        action="store_true",
        help="List internal subagent sessions individually instead of grouping under their parent session",
    )
    parser.add_argument(
        "--redact",
        "--privacy",
        dest="redact",
        action="store_true",
        help="Redact session titles, workspace names, session IDs, and local paths (keeps models, tokens, and costs)",
    )
    parser.add_argument(
        "--no-color",
        action="store_true",
        help="Disable ANSI colors in table output",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    _configure_stdio()
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.days is not None and args.days <= 0:
        parser.error("--days must be a positive integer.")

    if args.utc:
        tz: dt.tzinfo = dt.timezone.utc
        tz_label = "UTC"
    else:
        now_local = dt.datetime.now().astimezone()
        tz = now_local.tzinfo or dt.timezone.utc
        tz_label = now_local.tzname() or str(tz)

    today = dt.datetime.now(tz).date()
    since = args.since
    until = args.until
    if args.today:
        since = today
        until = today
    elif args.days is not None:
        since = today - dt.timedelta(days=args.days - 1)
        if until is None:
            until = today

    if since and until and since > until:
        parser.error(f"--since ({since}) cannot be after --until ({until}).")

    pricing_path = args.pricing.expanduser() if args.pricing else None
    try:
        agent_filter = parse_agent_filter(args.agents)
    except ValueError as exc:
        parser.error(str(exc))
    if args.agy_only:
        agent_filter = {"antigravity"}

    sources: list[SourceSpec] | None = None
    extra_dirs: list[pathlib.Path] = []
    if args.data_dir is not None:
        primary_dir = args.data_dir.expanduser()
        sources = [SourceSpec("antigravity", primary_dir, _infer_source_name(primary_dir))]
    elif args.single_source:
        primary_dir = DEFAULT_DATA_DIR
        sources = [SourceSpec("antigravity", primary_dir, "cli")]
    else:
        sources = discover_default_sources(agent_filter)
        primary_dir = sources[0].root if sources else DEFAULT_DATA_DIR

    archive_path = None if args.no_archive else DEFAULT_ARCHIVE_PATH
    report = scan_usage(
        data_dir=primary_dir,
        tz=tz,
        since=since,
        until=until,
        model_filter=args.model,
        session_filter=args.session,
        workspace_filter=args.workspace,
        pricing_path=pricing_path,
        extra_dirs=extra_dirs,
        archive_path=archive_path,
        sources=sources,
        agent_filter=agent_filter,
    )
    agg = _aggregate(report.sessions)
    sl_path = args.statusline_state.expanduser() if args.statusline_state else None
    statusline = load_statusline_state(sl_path)

    if args.view == "pricing" and args.format == "table" and not args.compact:
        observed = [
            k
            for k, _ in sorted(
                ((k, v) for k, v in agg["by_model"].items() if k not in ("compaction", "unknown")),
                key=lambda kv: kv[1].total_tokens,
                reverse=True,
            )
        ]
        print(
            render_pricing_table(
                PricingCatalog(pricing_path),
                observed,
                today,
                _use_color(args.no_color),
                redact=args.redact,
            )
        )
        return 0

    if args.compact:
        print(render_compact(report, agg, tz=tz, statusline=statusline))
    elif args.format == "json":
        include_turns = bool(args.session)
        print(
            render_json(
                report,
                agg,
                sort_by=args.sort,
                tz_label=tz_label,
                include_turns=include_turns,
                redact=args.redact,
                statusline=statusline,
            )
        )
    elif args.format == "csv":
        print(render_csv(report, agg, view=args.view, sort_by=args.sort, redact=args.redact))
    else:
        print(
            render_text(
                report,
                agg,
                view=args.view,
                sort_by=args.sort,
                limit=args.limit,
                human=args.human,
                tz_label=tz_label,
                since=since,
                until=until,
                model_filter=args.model,
                session_filter=args.session,
                color=_use_color(args.no_color),
                show_subagents=args.show_subagents,
                redact=args.redact,
                statusline=statusline,
                agent_filter=agent_filter,
            )
        )

    for warn in report.warnings:
        w_out = _redact_warning(warn, report.data_dir, report.data_dirs) if args.redact else warn
        print(f"Note: {w_out}", file=sys.stderr)

    if args.check_budget or args.budget_daily is not None or args.budget_weekly is not None:
        bcfg = load_budget_config(DEFAULT_BUDGET_PATH)
        daily_lim = args.budget_daily if args.budget_daily is not None else bcfg["daily_usd"]
        weekly_lim = args.budget_weekly if args.budget_weekly is not None else bcfg["weekly_usd"]
        warn_p = args.warn_pct if args.warn_pct is not None else bcfg["warn_pct"]
        crit_p = args.crit_pct if args.crit_pct is not None else bcfg["critical_pct"]
        exit_code, notes = evaluate_budget_exit_code(
            agg,
            tz=tz,
            daily_usd=daily_lim,
            weekly_usd=weekly_lim,
            warn_pct=warn_p,
            crit_pct=crit_p,
        )
        for n in notes:
            print(f"Budget: {n}", file=sys.stderr)
        if args.check_budget:
            return exit_code

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
