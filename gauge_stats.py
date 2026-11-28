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

import datetime as dt
import io
import json
import os
import pathlib
import re
import stat
import sys
import time
from collections import defaultdict


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


class SourceSpec:
    __slots__ = ("kind", "root", "source_name")

    def __init__(self, kind: str, root: pathlib.Path, source_name: str) -> None:
        object.__setattr__(self, "kind", kind)
        object.__setattr__(self, "root", root)
        object.__setattr__(self, "source_name", source_name)

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError(f"SourceSpec is immutable (cannot set {name!r})")

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, SourceSpec):
            return NotImplemented
        return (self.kind, self.root, self.source_name) == (other.kind, other.root, other.source_name)

    def __hash__(self) -> int:
        return hash((self.kind, self.root, self.source_name))

    def __repr__(self) -> str:
        return f"SourceSpec(kind={self.kind!r}, root={self.root!r}, source_name={self.source_name!r})"


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


_SOURCE_BADGES: dict[str, str] = {
    "cli": "AGY",
    "app": "App",
    "ide": "IDE",
    "custom": "Custom",
    "claude": "Claude",
    "codex": "Codex",
    "gemini": "Gemini",
}

_SOURCE_DISPLAY_LABELS: dict[str, str] = {
    "cli": "Antigravity CLI",
    "app": "Antigravity App",
    "ide": "Antigravity IDE",
    "custom": "Antigravity Custom",
    "claude": "Claude Code",
    "codex": "Codex CLI",
    "gemini": "Gemini CLI",
}

_AGENT_DISPLAY_LABELS: dict[str, str] = {
    "antigravity": "Antigravity",
    "claude": "Claude Code",
    "codex": "Codex CLI",
    "gemini": "Gemini CLI",
}


def source_badge(source: str) -> str:
    low = source.strip().lower()
    return _SOURCE_BADGES.get(low, source or "AGY")


def source_display_label(source: str) -> str:
    low = source.strip().lower()
    return _SOURCE_DISPLAY_LABELS.get(low, source or "Antigravity")


def agent_display_label(family: str) -> str:
    return _AGENT_DISPLAY_LABELS.get(family, family)


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
_ISO_WEEK_RE = re.compile(r"^(\d{4})-W(\d{2})$")
_WIN_DRIVE_URI_RE = re.compile(r"^/[A-Za-z]:/")
_FRAC_SEC_RE = re.compile(r"(\.\d{6})\d+")
_JSON_WS_RE = re.compile(r"[ \t\n\r]*")
_AGY_FAST_RE = re.compile(
    r'^\{"step_index":(\d+),"source":"[^"\\]*","type":"([^"\\]+)","status":"([^"\\]+)","created_at":"([^"\\]+)"'
    r'(?:,"model":"([^"\\]*)")?'
    r',"input_tokens":(\d+),"cache_read_tokens":(\d+)'
    r'(?:,"cache_write_tokens":(\d+))?'
    r',"output_tokens":(\d+)'
)
_PY311_PLUS = sys.version_info >= (3, 11)
_UTC = dt.timezone.utc
_FROMISOFORMAT = dt.datetime.fromisoformat
_JSON_DECODE = json.JSONDecoder().decode

_EFFORT_SUFFIXES = ("-xhigh", "-high", "-medium", "-med", "-low", "-minimal", "-max", "-none")
_CARBON_STD_DATE = dt.date(2026, 12, 1)
_FLASH38_STD_DATE = dt.date(2027, 1, 1)


class ModelRate:
    """USD per 1,000,000 tokens."""

    __slots__ = ("input", "cache_read", "cache_write", "output")

    def __init__(self, input: float, cache_read: float, cache_write: float, output: float) -> None:
        object.__setattr__(self, "input", input)
        object.__setattr__(self, "cache_read", cache_read)
        object.__setattr__(self, "cache_write", cache_write)
        object.__setattr__(self, "output", output)

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError(f"ModelRate is immutable (cannot set {name!r})")

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, ModelRate):
            return NotImplemented
        return (self.input, self.cache_read, self.cache_write, self.output) == (
            other.input, other.cache_read, other.cache_write, other.output
        )

    def __hash__(self) -> int:
        return hash((self.input, self.cache_read, self.cache_write, self.output))

    def __repr__(self) -> str:
        return (
            f"ModelRate(input={self.input!r}, cache_read={self.cache_read!r}, "
            f"cache_write={self.cache_write!r}, output={self.output!r})"
        )

    def calculate(
        self,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        cache_write_1h_tokens: int = 0,
    ) -> "CostBreakdown":
        inv_m = 1e-6
        in_usd = input_tokens * inv_m * self.input
        cr_usd = cache_read_tokens * inv_m * self.cache_read
        if cache_write_tokens > 0:
            if cache_write_1h_tokens > 0:
                cw_1h = cache_write_1h_tokens if cache_write_1h_tokens < cache_write_tokens else cache_write_tokens
                cw_5m = cache_write_tokens - cw_1h
                cw_usd = cw_5m * inv_m * self.cache_write + cw_1h * inv_m * (self.input * 2.0)
            else:
                cw_usd = cache_write_tokens * inv_m * self.cache_write
        else:
            cw_usd = 0.0
        out_usd = output_tokens * inv_m * self.output
        saved_usd = (
            cache_read_tokens * inv_m * (self.input - self.cache_read)
            if cache_read_tokens > 0 and self.input > self.cache_read
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


class CostBreakdown:
    __slots__ = (
        "input_usd",
        "cache_read_usd",
        "cache_write_usd",
        "output_usd",
        "cache_saved_usd",
        "unpriced_calls",
    )

    def __init__(
        self,
        input_usd: float = 0.0,
        cache_read_usd: float = 0.0,
        cache_write_usd: float = 0.0,
        output_usd: float = 0.0,
        cache_saved_usd: float = 0.0,
        unpriced_calls: int = 0,
    ) -> None:
        self.input_usd = input_usd
        self.cache_read_usd = cache_read_usd
        self.cache_write_usd = cache_write_usd
        self.output_usd = output_usd
        self.cache_saved_usd = cache_saved_usd
        self.unpriced_calls = unpriced_calls

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, CostBreakdown):
            return NotImplemented
        return (
            self.input_usd,
            self.cache_read_usd,
            self.cache_write_usd,
            self.output_usd,
            self.cache_saved_usd,
            self.unpriced_calls,
        ) == (
            other.input_usd,
            other.cache_read_usd,
            other.cache_write_usd,
            other.output_usd,
            other.cache_saved_usd,
            other.unpriced_calls,
        )

    def __repr__(self) -> str:
        return (
            f"CostBreakdown(input_usd={self.input_usd!r}, cache_read_usd={self.cache_read_usd!r}, "
            f"cache_write_usd={self.cache_write_usd!r}, output_usd={self.output_usd!r}, "
            f"cache_saved_usd={self.cache_saved_usd!r}, unpriced_calls={self.unpriced_calls!r})"
        )

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


_ZERO_COST = CostBreakdown()


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


def _strip_effort_suffix(exact: str) -> str:
    for suffix in _EFFORT_SUFFIXES:
        if exact.endswith(suffix) and len(exact) > len(suffix):
            return exact[: -len(suffix)]
    return exact


def normalize_model_key(raw: str) -> str:
    return _strip_effort_suffix(canonical_exact_key(raw))


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
        # Memoized matched rule spec per raw model string:
        # (source, rule_name, base, long_th, long_rate, intro_before, intro_rate) | None
        self._rule_cache: dict[
            str,
            tuple[str, str, ModelRate, int | None, ModelRate | None, dt.date | None, ModelRate | None] | None,
        ] = {}
        self.refresh()

    def refresh(self) -> bool:
        path = self.override_path
        if path is None or path.is_symlink() or not path.is_file():
            changed = self._override_sig is not None or bool(self._custom_rules)
            self._override_sig = None
            self._custom_rules = []
            if changed:
                self._rule_cache.clear()
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
        self._rule_cache.clear()
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
            prefix = _strip_effort_suffix(exact_key)
            if not prefix:
                continue
            has_effort = exact_key != prefix
            raw_th = v.get("long_context_threshold")
            th = int(raw_th) if isinstance(raw_th, int) and not isinstance(raw_th, bool) and raw_th > 0 else None
            lc = v.get("long_context")
            lr = _parse_rate_dict(lc) if isinstance(lc, dict) else None
            rules.append((raw_key, exact_key, prefix, has_effort, base, th, lr))
        return rules

    def _lookup_rule_spec(
        self, model: str
    ) -> tuple[str, str, ModelRate, int | None, ModelRate | None, dt.date | None, ModelRate | None] | None:
        if model in self._rule_cache:
            return self._rule_cache[model]
        exact = canonical_exact_key(model)
        norm = _strip_effort_suffix(exact)
        spec: tuple[str, str, ModelRate, int | None, ModelRate | None, dt.date | None, ModelRate | None] | None = None
        if norm and norm not in ("unknown", "compaction") and model != "(上下文压缩)":
            for raw_key, exact_key, _, _, base, th, lr in self._custom_rules:
                if exact_key == exact:
                    spec = ("custom(exact)", raw_key, base, th, lr, None, None)
                    break
            if spec is None and self._custom_rules:
                best: tuple[str, str, str, bool, ModelRate, int | None, ModelRate | None] | None = None
                for rule in self._custom_rules:
                    if rule[3]:  # effort-specific custom rule does not match other effort levels
                        continue
                    if _matches_model_prefix(norm, rule[2]):
                        if best is None or len(rule[2]) > len(best[2]):
                            best = rule
                if best is not None:
                    raw_key, _, _, _, base, th, lr = best
                    spec = ("custom(prefix)", raw_key, base, th, lr, None, None)
            if spec is None:
                for display, prefixes, base, th, lr, intro_before, intro_rate, _ in _BUILTIN_SPECS:
                    if any(_matches_model_prefix(norm, p) for p in prefixes):
                        spec = ("builtin", display, base, th, lr, intro_before, intro_rate)
                        break
        self._rule_cache[model] = spec
        return spec

    def resolve_detail(
        self, model: str, total_input_tokens: int, date_obj: dt.date | None
    ) -> tuple[ModelRate, str, str] | None:
        spec = self._rule_cache.get(model)
        if spec is None and model not in self._rule_cache:
            spec = self._lookup_rule_spec(model)
        if spec is None:
            return None
        src, rule_name, base, th, lr, intro_before, intro_rate = spec
        if intro_before is not None and intro_rate is not None and date_obj is not None and date_obj < intro_before:
            return intro_rate, src, rule_name
        if th is not None and lr is not None and total_input_tokens > th:
            return lr, src, rule_name
        return base, src, rule_name

    def resolve_rate(self, model: str, total_input_tokens: int, date_obj: dt.date | None) -> ModelRate | None:
        spec = self._rule_cache.get(model)
        if spec is None and model not in self._rule_cache:
            spec = self._lookup_rule_spec(model)
        if spec is None:
            return None
        _, _, base, th, lr, intro_before, intro_rate = spec
        if intro_before is not None and intro_rate is not None and date_obj is not None and date_obj < intro_before:
            return intro_rate
        if th is not None and lr is not None and total_input_tokens > th:
            return lr
        return base

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


class TokenCounts:
    __slots__ = (
        "calls",
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "peak_context",
        "cost",
    )

    def __init__(
        self,
        calls: int = 0,
        input_tokens: int = 0,
        cache_read_tokens: int = 0,
        cache_write_tokens: int = 0,
        output_tokens: int = 0,
        peak_context: int = 0,
        cost: CostBreakdown | None = None,
    ) -> None:
        self.calls = calls
        self.input_tokens = input_tokens
        self.cache_read_tokens = cache_read_tokens
        self.cache_write_tokens = cache_write_tokens
        self.output_tokens = output_tokens
        self.peak_context = peak_context
        self.cost = CostBreakdown() if cost is None else cost

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, TokenCounts):
            return NotImplemented
        return (
            self.calls,
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.output_tokens,
            self.peak_context,
            self.cost,
        ) == (
            other.calls,
            other.input_tokens,
            other.cache_read_tokens,
            other.cache_write_tokens,
            other.output_tokens,
            other.peak_context,
            other.cost,
        )

    def __repr__(self) -> str:
        return (
            f"TokenCounts(calls={self.calls!r}, input_tokens={self.input_tokens!r}, "
            f"cache_read_tokens={self.cache_read_tokens!r}, cache_write_tokens={self.cache_write_tokens!r}, "
            f"output_tokens={self.output_tokens!r}, peak_context={self.peak_context!r}, cost={self.cost!r})"
        )

    @property
    def total_input_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens

    @property
    def total_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens

    @property
    def non_cached_tokens(self) -> int:
        return self.input_tokens + self.cache_write_tokens + self.output_tokens

    @property
    def cache_hit_rate(self) -> float:
        denom = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
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
            sc = self.cost
            sc.input_usd += cost.input_usd
            sc.cache_read_usd += cost.cache_read_usd
            sc.cache_write_usd += cost.cache_write_usd
            sc.output_usd += cost.output_usd
            sc.cache_saved_usd += cost.cache_saved_usd
            sc.unpriced_calls += cost.unpriced_calls
        ctx = input_tokens + cache_read_tokens + cache_write_tokens
        if ctx > self.peak_context:
            self.peak_context = ctx

    def add_call_inline(
        self,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        ctx: int,
        in_usd: float,
        cr_usd: float,
        cw_usd: float,
        out_usd: float,
        saved_usd: float,
        unpriced: int,
    ) -> None:
        self.calls += 1
        self.input_tokens += input_tokens
        self.cache_read_tokens += cache_read_tokens
        self.cache_write_tokens += cache_write_tokens
        self.output_tokens += output_tokens
        sc = self.cost
        sc.input_usd += in_usd
        sc.cache_read_usd += cr_usd
        sc.cache_write_usd += cw_usd
        sc.output_usd += out_usd
        sc.cache_saved_usd += saved_usd
        sc.unpriced_calls += unpriced
        if ctx > self.peak_context:
            self.peak_context = ctx

    def merge(self, other: "TokenCounts") -> None:
        self.calls += other.calls
        self.input_tokens += other.input_tokens
        self.cache_read_tokens += other.cache_read_tokens
        self.cache_write_tokens += other.cache_write_tokens
        self.output_tokens += other.output_tokens
        sc = self.cost
        oc = other.cost
        sc.input_usd += oc.input_usd
        sc.cache_read_usd += oc.cache_read_usd
        sc.cache_write_usd += oc.cache_write_usd
        sc.output_usd += oc.output_usd
        sc.cache_saved_usd += oc.cache_saved_usd
        sc.unpriced_calls += oc.unpriced_calls
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


class TurnRecord:
    __slots__ = (
        "conversation_id",
        "step_index",
        "timestamp",
        "model",
        "step_type",
        "status",
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "tool_names",
        "thinking_chars",
        "reasoning_tokens",
        "cache_write_1h_tokens",
        "cost",
    )

    def __init__(
        self,
        conversation_id: str,
        step_index: int,
        timestamp: dt.datetime | None,
        model: str,
        step_type: str,
        status: str,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
        tool_names: tuple[str, ...] = (),
        thinking_chars: int = 0,
        reasoning_tokens: int = 0,
        cache_write_1h_tokens: int = 0,
        cost: CostBreakdown | None = None,
    ) -> None:
        self.conversation_id = conversation_id
        self.step_index = step_index
        self.timestamp = timestamp
        self.model = model
        self.step_type = step_type
        self.status = status
        self.input_tokens = input_tokens
        self.cache_read_tokens = cache_read_tokens
        self.cache_write_tokens = cache_write_tokens
        self.output_tokens = output_tokens
        self.tool_names = tool_names
        self.thinking_chars = thinking_chars
        self.reasoning_tokens = reasoning_tokens
        self.cache_write_1h_tokens = cache_write_1h_tokens
        self.cost = CostBreakdown() if cost is None else cost

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, TurnRecord):
            return NotImplemented
        return (
            self.conversation_id,
            self.step_index,
            self.timestamp,
            self.model,
            self.step_type,
            self.status,
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.output_tokens,
            self.tool_names,
            self.thinking_chars,
            self.reasoning_tokens,
            self.cache_write_1h_tokens,
            self.cost,
        ) == (
            other.conversation_id,
            other.step_index,
            other.timestamp,
            other.model,
            other.step_type,
            other.status,
            other.input_tokens,
            other.cache_read_tokens,
            other.cache_write_tokens,
            other.output_tokens,
            other.tool_names,
            other.thinking_chars,
            other.reasoning_tokens,
            other.cache_write_1h_tokens,
            other.cost,
        )

    def __repr__(self) -> str:
        return (
            f"TurnRecord(conversation_id={self.conversation_id!r}, step_index={self.step_index!r}, "
            f"model={self.model!r}, input_tokens={self.input_tokens!r}, output_tokens={self.output_tokens!r})"
        )

    @property
    def total_input_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens

    @property
    def total_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens

    @property
    def cache_hit_rate(self) -> float:
        denom = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
        return (self.cache_read_tokens / denom) * 100.0 if denom > 0 else 0.0

    @property
    def has_thinking(self) -> bool:
        return self.thinking_chars > 0 or self.reasoning_tokens > 0


class _SessionDigest:
    """Compact pre-aggregated per-session summary used when retain_turns=False."""

    __slots__ = (
        "by_model",
        "by_day",
        "by_hour",
        "day_to_wm",
        "tool_counts",
        "model_tokens",
        "active_duration_secs",
        "thinking_turns",
    )

    def __init__(
        self,
        by_model: dict[str, TokenCounts],
        by_day: dict[str, TokenCounts],
        by_hour: dict[int, TokenCounts],
        day_to_wm: dict[str, tuple[str, str]],
        tool_counts: dict[str, int],
        model_tokens: dict[str, tuple[bool, int]],
        active_duration_secs: int,
        thinking_turns: int,
    ) -> None:
        self.by_model = by_model
        self.by_day = by_day
        self.by_hour = by_hour
        self.day_to_wm = day_to_wm
        self.tool_counts = tool_counts
        self.model_tokens = model_tokens
        self.active_duration_secs = active_duration_secs
        self.thinking_turns = thinking_turns


def _active_duration_from_times(times: list[dt.datetime]) -> int:
    if not times:
        return 0
    if len(times) == 1:
        return 15
    times.sort()
    secs = 0
    prev = times[0]
    for cur in times[1:]:
        d = int((cur - prev).total_seconds())
        if d > 0:
            secs += d if d < 600 else 600
        prev = cur
    return secs if secs > 15 else 15


class SessionInfo:
    __slots__ = (
        "conversation_id",
        "title",
        "workspace",
        "transcript_path",
        "source",
        "is_archived",
        "is_subagent",
        "parent_id",
        "parent_title",
        "turns",
        "counts",
        "models",
        "first_time",
        "last_time",
        "_digest",
    )

    def __init__(
        self,
        conversation_id: str,
        title: str = "",
        workspace: str = "",
        transcript_path: str = "",
        source: str = "cli",
        is_archived: bool = False,
        is_subagent: bool = False,
        parent_id: str | None = None,
        parent_title: str = "",
        turns: list[TurnRecord] | None = None,
        counts: TokenCounts | None = None,
        models: set[str] | None = None,
        first_time: dt.datetime | None = None,
        last_time: dt.datetime | None = None,
        _digest: _SessionDigest | None = None,
    ) -> None:
        self.conversation_id = conversation_id
        self.title = title
        self.workspace = workspace
        self.transcript_path = transcript_path
        self.source = source
        self.is_archived = is_archived
        self.is_subagent = is_subagent
        self.parent_id = parent_id
        self.parent_title = parent_title
        self.turns = [] if turns is None else turns
        self.counts = TokenCounts() if counts is None else counts
        self.models = set() if models is None else models
        self.first_time = first_time
        self.last_time = last_time
        self._digest = _digest

    def __repr__(self) -> str:
        return (
            f"SessionInfo(conversation_id={self.conversation_id!r}, source={self.source!r}, "
            f"calls={self.counts.calls!r}, title={self.title!r})"
        )

    @property
    def agent_family(self) -> str:
        return source_to_agent_family(self.source)

    @property
    def active_duration_secs(self) -> int:
        if self._digest is not None and not self.turns:
            return self._digest.active_duration_secs
        times = [t.timestamp for t in self.turns if t.timestamp is not None]
        return _active_duration_from_times(times)

    @property
    def thinking_turns(self) -> int:
        if self._digest is not None and not self.turns:
            return self._digest.thinking_turns
        return sum(1 for t in self.turns if t.has_thinking)

    def tool_counts(self) -> dict[str, int]:
        if self._digest is not None and not self.turns:
            return dict(self._digest.tool_counts)
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


class ScanReport:
    __slots__ = (
        "data_dir",
        "sessions",
        "warnings",
        "data_dirs",
        "archived_sessions_count",
        "skipped_empty_sessions",
        "skipped_lines",
        "scan_ms",
        "files_cached",
        "files_reparsed",
    )

    def __init__(
        self,
        data_dir: pathlib.Path,
        sessions: list[SessionInfo],
        warnings: list[str],
        data_dirs: list[pathlib.Path] | None = None,
        archived_sessions_count: int = 0,
        skipped_empty_sessions: int = 0,
        skipped_lines: int = 0,
        scan_ms: float = 0.0,
        files_cached: int = 0,
        files_reparsed: int = 0,
    ) -> None:
        self.data_dir = data_dir
        self.sessions = sessions
        self.warnings = warnings
        self.data_dirs = [] if data_dirs is None else data_dirs
        self.archived_sessions_count = archived_sessions_count
        self.skipped_empty_sessions = skipped_empty_sessions
        self.skipped_lines = skipped_lines
        self.scan_ms = scan_ms
        self.files_cached = files_cached
        self.files_reparsed = files_reparsed

    def __repr__(self) -> str:
        return (
            f"ScanReport(data_dir={self.data_dir!r}, sessions={len(self.sessions)}, "
            f"archived_sessions_count={self.archived_sessions_count!r}, scan_ms={self.scan_ms:.1f})"
        )


def iso_week_key(date_obj: dt.date) -> str:
    iso = date_obj.isocalendar()
    return f"{iso.year}-W{iso.week:02d}"


def iso_week_bounds(week_key: str) -> tuple[dt.date, dt.date] | None:
    m = _ISO_WEEK_RE.match(week_key)
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


def _is_within_dir(
    path: pathlib.Path,
    root: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> bool:
    try:
        resolved_path = path.resolve(strict=True)
        res_root = resolved_root if resolved_root is not None else root.resolve(strict=True)
        resolved_path.relative_to(res_root)
        return True
    except (OSError, ValueError):
        return False


def _lstat_regular_file(path: pathlib.Path) -> os.stat_result | None:
    """Single-syscall check that path is a regular non-symlink file, returning lstat result."""
    try:
        st = path.lstat()
    except OSError:
        return None
    return st if stat.S_ISREG(st.st_mode) else None


def _lstat_regular_file_str(path_str: str) -> os.stat_result | None:
    """Zero-Path-alloc single-syscall check for regular non-symlink files."""
    try:
        st = os.lstat(path_str)
    except OSError:
        return None
    return st if stat.S_ISREG(st.st_mode) else None


def _lstat_dir(path: pathlib.Path) -> os.stat_result | None:
    """Single-syscall check that path is a non-symlink directory, returning lstat result."""
    try:
        st = path.lstat()
    except OSError:
        return None
    return st if stat.S_ISDIR(st.st_mode) else None


def _safe_read_text(
    path: pathlib.Path,
    root: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
    skip_lstat: bool = False,
) -> str | None:
    if not skip_lstat and _lstat_regular_file(path) is None:
        return None
    if not _is_within_dir(path, root, resolved_root=resolved_root):
        return None
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


_PBTXT_SIMPLE_ESCAPES: dict[str, bytes] = {
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


def _unescape_pbtxt_string(raw: str) -> str:
    """Decode protobuf text-format string escapes (including octal UTF-8 bytes)."""
    if "\\" not in raw:
        return raw
    simple = _PBTXT_SIMPLE_ESCAPES
    out = bytearray()
    pos = 0
    for match in _PBTXT_ESCAPE_RE.finditer(raw):
        if match.start() > pos:
            out.extend(raw[pos : match.start()].encode("utf-8"))
        esc = match.group(1)
        b_val = simple.get(esc)
        if b_val is not None:
            out.extend(b_val)
        elif esc.startswith("x"):
            out.append(int(esc[1:], 16))
        else:
            out.append(int(esc, 8) & 0xFF)
        pos = match.end()
    if pos < len(raw):
        out.extend(raw[pos:].encode("utf-8"))
    return out.decode("utf-8", errors="replace")


def _parse_annotation_title(
    path: pathlib.Path,
    root: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
    skip_lstat: bool = False,
) -> str:
    text = _safe_read_text(path, root, resolved_root=resolved_root, skip_lstat=skip_lstat)
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
        import urllib.parse

        parsed = urllib.parse.urlparse(uri)
        path = urllib.parse.unquote(parsed.path or "")
        # On Windows, file:///C:/Users/... parses path as /C:/Users/...
        if _WIN_DRIVE_URI_RE.match(path):
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


def _extract_swarm_role(header_line: str) -> str:
    """Extract '@role' from 'You have just started. Your assignment (... for @role):'."""
    marker = " for @"
    pos = header_line.find(marker)
    if pos < 0:
        return ""
    rest = header_line[pos + len(" for ") :]
    end = 0
    while end < len(rest) and rest[end] not in (")", ":", ",", " ", "\t", "\r", "\n"):
        end += 1
    role = rest[:end]
    return role if len(role) > 1 else ""


def _extract_subagent_meta(content: str) -> tuple[str | None, str, bool, bool]:
    """Parse subagent launcher SYSTEM_MESSAGE for (parent_id, preview, is_swarm, is_swarm_ping)."""
    if not content:
        return None, "", False, False
    parent_id: str | None = None
    pos = content.find("sender=")
    if pos >= 0:
        rest = content[pos + len("sender=") :]
        tok = rest.split(None, 1)[0] if rest.split(None, 1) else ""
        if _UUID_RE.match(tok):
            parent_id = tok
    preview = ""
    is_swarm = False
    is_swarm_ping = False
    cpos = content.find("content=")
    if cpos >= 0:
        body = content[cpos + len("content=") :]
        lines: list[str] = []
        for raw_line in body.splitlines():
            trimmed = raw_line.strip()
            if not trimmed or trimmed.startswith("</SYSTEM_MESSAGE>"):
                continue
            if trimmed.endswith("</SYSTEM_MESSAGE>"):
                trimmed = trimmed[: -len("</SYSTEM_MESSAGE>")].strip()
                if not trimmed:
                    continue
            lines.append(trimmed)
        if lines:
            first_raw = lines[0]
            if first_raw.startswith("[swarm] new board message"):
                is_swarm = True
                is_swarm_ping = True
                preview = _clean_single_line(first_raw, max_len=72)
            elif first_raw.startswith("You have just started. Your assignment"):
                is_swarm = True
                role = _extract_swarm_role(first_raw)
                brief = ""
                for candidate in lines[1:]:
                    cleaned = candidate.lstrip("#").strip().replace("**", "").replace("`", "")
                    if cleaned:
                        brief = cleaned
                        break
                if role and brief:
                    preview = _clean_single_line(f"{role} · {brief}", max_len=72)
                elif role:
                    preview = _clean_single_line(role, max_len=72)
                elif brief:
                    preview = _clean_single_line(brief, max_len=72)
                else:
                    preview = _clean_single_line(first_raw, max_len=72)
            else:
                for candidate in lines:
                    cleaned = candidate.lstrip("#").strip().replace("**", "").replace("`", "")
                    preview = _clean_single_line(cleaned, max_len=72)
                    if preview:
                        break
    return parent_id, preview, is_swarm, is_swarm_ping


def _parse_iso_timestamp(value: Any, tz: dt.tzinfo) -> dt.datetime | None:
    if type(value) is not str and not isinstance(value, str):
        return None
    s = value.strip()
    if not s:
        return None
    if "." in s:
        s = _FRAC_SEC_RE.sub(r"\1", s)
    if not _PY311_PLUS and s.endswith("Z"):
        s = s[:-1] + "+00:00"
    try:
        parsed = _FROMISOFORMAT(s)
    except ValueError:
        if _PY311_PLUS and s.endswith("Z"):
            try:
                parsed = _FROMISOFORMAT(s[:-1] + "+00:00")
            except ValueError:
                return None
        else:
            return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=_UTC)
    if parsed.tzinfo is tz or parsed.tzinfo == tz:
        return parsed
    return parsed.astimezone(tz)


def _parse_utc_timestamp_fast(value: Any) -> dt.datetime | None:
    """Fast-path UTC timestamp parser using Python 3.11+ C fromisoformat directly."""
    if _PY311_PLUS and type(value) is str:
        try:
            parsed = _FROMISOFORMAT(value)
            tzinfo = parsed.tzinfo
            if tzinfo is _UTC:
                return parsed
            if tzinfo is None:
                return parsed.replace(tzinfo=_UTC)
            return parsed.astimezone(_UTC)
        except ValueError:
            pass
    return _parse_iso_timestamp(value, _UTC)


def _to_non_neg_int(value: Any) -> int:
    t = type(value)
    if t is int:
        return value if value > 0 else 0
    if value is None or t is bool:
        return 0
    if t is float:
        return int(value) if value > 0 else 0
    if t is str:
        s = value.strip()
        if not s:
            return 0
        try:
            v = int(s)
            return v if v > 0 else 0
        except ValueError:
            return 0
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        return value if value > 0 else 0
    if isinstance(value, float):
        return int(value) if value > 0 else 0
    if isinstance(value, str):
        try:
            v = int(value.strip())
            return v if v > 0 else 0
        except ValueError:
            return 0
    return 0


def _load_metadata_cache(
    data_dir: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> dict[str, dict[str, Any]]:
    meta_path = data_dir / "cache" / "conversation_metadata.json"
    text = _safe_read_text(meta_path, data_dir, resolved_root=resolved_root)
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
    return (
        record.get("input_tokens") is not None
        or record.get("output_tokens") is not None
        or record.get("cache_read_tokens") is not None
        or record.get("cache_write_tokens") is not None
        or record.get("cache_creation_tokens") is not None
        or record.get("cache_creation_input_tokens") is not None
    )


def _parse_transcript_file(
    transcript_path: pathlib.Path,
    cid: str,
    *,
    scalar_only: bool = False,
    skip_meta: bool = False,
    retain_thinking_len: bool = True,
    utc_date_bounds: tuple[str | None, str | None] | None = None,
) -> tuple[list[TurnRecord], str, str | None, str, bool, int]:
    """Parse a single transcript JSONL file in UTC (tz conversion happens later)."""
    turns_by_step: dict[int, TurnRecord] = {}
    fallback_order = -1
    seen_user_input = False
    first_user_preview = ""
    subagent_parent_id: str | None = None
    subagent_preview = ""
    is_swarm_subagent = False
    subagent_meta_is_swarm_ping = False
    skipped_lines = 0
    intern = sys.intern
    decode = _JSON_DECODE
    fast_match = _AGY_FAST_RE.match
    parse_utc = _parse_utc_timestamp_fast
    to_non_neg_int = _to_non_neg_int
    turn_cls = TurnRecord
    zero_cost = _ZERO_COST
    utc = _UTC
    min_day_str = utc_date_bounds[0] if utc_date_bounds is not None else None
    max_day_str = utc_date_bounds[1] if utc_date_bounds is not None else None
    has_day_bounds = (min_day_str is not None) or (max_day_str is not None)
    last_raw_model = ""
    last_model = ""
    tool_cache: dict[str, str] = {}

    with transcript_path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=65536) as fh:
        if hasattr(fh, "_CHUNK_SIZE"):
            fh._CHUNK_SIZE = 65536
        for line in fh:
            if not line:
                continue
            if '_tokens"' not in line:
                if skip_meta:
                    if line[0] == "{" and (
                        (line[-1] == "\n" and (line[-2] == "}" or (line[-2] == "\r" and line[-3] == "}")))
                        or line[-1] == "}"
                    ):
                        continue
                    stripped = line.strip()
                    if not stripped or (stripped[0] == "{" and stripped[-1] == "}"):
                        continue
                else:
                    if seen_user_input or (
                        "USER_INPUT" not in line
                        and (
                            turns_by_step
                            or (
                                (subagent_parent_id is not None or subagent_preview)
                                and not subagent_meta_is_swarm_ping
                            )
                            or "SYSTEM_MESSAGE" not in line
                        )
                    ):
                        if line[0] == "{" and (
                            (line[-1] == "\n" and (line[-2] == "}" or (line[-2] == "\r" and line[-3] == "}")))
                            or line[-1] == "}"
                        ):
                            continue
                        stripped = line.strip()
                        if not stripped or (stripped[0] == "{" and stripped[-1] == "}"):
                            continue
            else:
                # Fast prefix-regex extraction for standard Antigravity token lines (0/21.7K scalar-after-blob).
                # Skip regex completely on in-date tool_calls lines when full tool extraction is required,
                # and use zero-copy str.find/startswith instead of slicing line[end_pos:].
                try_fast = scalar_only
                if not try_fast:
                    if '"tool_calls"' not in line:
                        try_fast = True
                    elif has_day_bounds:
                        ca_idx = line.find('"created_at":"', 0, 220)
                        if ca_idx != -1:
                            d_pfx = line[ca_idx + 14 : ca_idx + 24]
                            if len(d_pfx) == 10 and d_pfx[4] == "-" and d_pfx[7] == "-":
                                if (min_day_str is not None and d_pfx < min_day_str) or (
                                    max_day_str is not None and d_pfx > max_day_str
                                ):
                                    try_fast = True
                if try_fast and line[0] == "{" and (
                    (line[-1] == "\n" and (line[-2] == "}" or (line[-2] == "\r" and line[-3] == "}")))
                    or line[-1] == "}"
                ):
                    m = fast_match(line)
                    if m is not None:
                        end_pos = m.end()
                        if end_pos < len(line):
                            ch = line[end_pos]
                            if ch == "," or ch == "}":
                                use_fast = False
                                thinking_chars = 0
                                g_step, g_type, g_status, ca_str, g_model, g_in, g_cr, g_cw, g_out = m.groups()
                                out_of_date = False
                                if (
                                    has_day_bounds
                                    and len(ca_str) >= 10
                                    and ca_str[4] == "-"
                                    and ca_str[7] == "-"
                                    and ca_str.endswith("Z")
                                ):
                                    d_pfx = ca_str[:10]
                                    if (min_day_str is not None and d_pfx < min_day_str) or (
                                        max_day_str is not None and d_pfx > max_day_str
                                    ):
                                        out_of_date = True
                                if scalar_only or out_of_date:
                                    if ch == "}" or line.find('"cache_creation', end_pos) == -1:
                                        use_fast = True
                                elif ch == "}":
                                    use_fast = True
                                elif (
                                    line.find('"tool_calls"', end_pos) == -1
                                    and line.find('"cache_creation', end_pos) == -1
                                ):
                                    if line.find('"thinking"', end_pos) == -1:
                                        use_fast = True
                                    elif not retain_thinking_len and line.startswith(',"thinking":"', end_pos):
                                        thinking_chars = 0 if line.startswith(',"thinking":""', end_pos) else 1
                                        use_fast = True
                                if use_fast:
                                    in_tok = int(g_in)
                                    cache_read = int(g_cr)
                                    cache_write = int(g_cw) if g_cw else 0
                                    out_tok = int(g_out)
                                    if (in_tok + cache_read + cache_write + out_tok) == 0:
                                        continue
                                    step_key = int(g_step)
                                    step_type = "PLANNER_RESPONSE" if g_type == "PLANNER_RESPONSE" else intern(g_type)
                                    status_s = "DONE" if g_status == "DONE" else intern(g_status)
                                    if g_model:
                                        if g_model == last_raw_model:
                                            model = last_model
                                        else:
                                            raw_model = g_model.strip()
                                            model = (
                                                intern(raw_model)
                                                if raw_model
                                                else ("compaction" if step_type == "CHECKPOINT" else "unknown")
                                            )
                                            last_raw_model = g_model
                                            last_model = model
                                    elif step_type == "CHECKPOINT":
                                        model = "compaction"
                                    else:
                                        model = "unknown"
                                    turns_by_step[step_key] = turn_cls(
                                        cid,
                                        step_key,
                                        parse_utc(ca_str),
                                        model,
                                        step_type,
                                        status_s,
                                        in_tok,
                                        cache_read,
                                        cache_write,
                                        out_tok,
                                        (),
                                        thinking_chars,
                                        0,
                                        0,
                                        zero_cost,
                                    )
                                    continue
            try:
                obj = decode(line)
            except json.JSONDecodeError:
                if not line.strip():
                    continue
                skipped_lines += 1
                continue
            if type(obj) is not dict and not isinstance(obj, dict):
                continue

            obj_get = obj.get
            step_type_peek = obj_get("type")
            if not skip_meta and not seen_user_input:
                if step_type_peek == "USER_INPUT":
                    seen_user_input = True
                    if not first_user_preview:
                        raw_c = obj_get("content")
                        if isinstance(raw_c, str):
                            first_user_preview = _extract_user_preview(raw_c)
                elif (
                    step_type_peek == "SYSTEM_MESSAGE"
                    and (not turns_by_step)
                    and (
                        (subagent_parent_id is None and not subagent_preview)
                        or subagent_meta_is_swarm_ping
                    )
                ):
                    raw_c = obj_get("content")
                    if isinstance(raw_c, str):
                        pid, prev, is_swarm, is_ping = _extract_subagent_meta(raw_c)
                        if is_swarm:
                            is_swarm_subagent = True
                        if subagent_meta_is_swarm_ping:
                            if is_ping:
                                if subagent_parent_id is None and pid is not None:
                                    subagent_parent_id = pid
                            else:
                                if pid is not None:
                                    subagent_parent_id = pid
                                if prev:
                                    subagent_preview = prev
                                subagent_meta_is_swarm_ping = False
                        else:
                            subagent_parent_id = pid
                            subagent_preview = prev
                            subagent_meta_is_swarm_ping = is_ping

            raw_in = obj_get("input_tokens")
            in_tok = raw_in if (type(raw_in) is int and raw_in >= 0) else to_non_neg_int(raw_in)
            raw_cr = obj_get("cache_read_tokens")
            cache_read = raw_cr if (type(raw_cr) is int and raw_cr >= 0) else to_non_neg_int(raw_cr)
            raw_cw = obj_get("cache_write_tokens")
            if not raw_cw:
                raw_cw = obj_get("cache_creation_tokens") or obj_get("cache_creation_input_tokens")
            cache_write = raw_cw if (type(raw_cw) is int and raw_cw >= 0) else to_non_neg_int(raw_cw)
            raw_out = obj_get("output_tokens")
            out_tok = raw_out if (type(raw_out) is int and raw_out >= 0) else to_non_neg_int(raw_out)
            if (in_tok + cache_read + cache_write + out_tok) == 0:
                continue

            ts = parse_utc(obj_get("created_at"))
            if step_type_peek == "PLANNER_RESPONSE":
                step_type = "PLANNER_RESPONSE"
            elif type(step_type_peek) is str:
                step_type = intern(step_type_peek)
            else:
                step_type = intern(str(step_type_peek or ""))

            raw_model_val = obj_get("model")
            if type(raw_model_val) is str and raw_model_val:
                if raw_model_val == last_raw_model:
                    model = last_model
                else:
                    rm = raw_model_val.strip()
                    model = intern(rm) if rm else ("compaction" if step_type == "CHECKPOINT" else "unknown")
                    last_raw_model = raw_model_val
                    last_model = model
            else:
                rm = str(raw_model_val or "").strip()
                if rm:
                    model = intern(rm)
                elif step_type == "CHECKPOINT":
                    model = "compaction"
                else:
                    model = "unknown"

            raw_step = obj_get("step_index")
            if type(raw_step) is int:
                step_key = raw_step
            elif isinstance(raw_step, int) and not isinstance(raw_step, bool):
                step_key = raw_step
            else:
                step_key = fallback_order
                fallback_order -= 1

            if scalar_only:
                tool_names: tuple[str, ...] = ()
                thinking_chars = 0
            else:
                raw_tools = obj_get("tool_calls")
                if raw_tools and (type(raw_tools) is list or isinstance(raw_tools, list)):
                    tlist: list[str] = []
                    for item in raw_tools:
                        if type(item) is dict or isinstance(item, dict):
                            tn = item.get("name")
                            if type(tn) is str:
                                cached_tn = tool_cache.get(tn)
                                if cached_tn is not None:
                                    if cached_tn:
                                        tlist.append(cached_tn)
                                else:
                                    st_tn = tn.strip()
                                    val_tn = intern(st_tn) if st_tn else ""
                                    tool_cache[tn] = val_tn
                                    if val_tn:
                                        tlist.append(val_tn)
                            elif tn:
                                st_tn = str(tn).strip()
                                if st_tn:
                                    tlist.append(intern(st_tn))
                    tool_names = tuple(tlist) if tlist else ()
                else:
                    tool_names = ()
                raw_thinking = obj_get("thinking")
                if raw_thinking and (type(raw_thinking) is str or isinstance(raw_thinking, str)):
                    thinking_chars = len(raw_thinking)
                else:
                    thinking_chars = 0

            raw_status = obj_get("status")
            if raw_status == "DONE":
                status_s = "DONE"
            elif type(raw_status) is str:
                status_s = intern(raw_status)
            else:
                status_s = intern(str(raw_status or ""))

            turns_by_step[step_key] = turn_cls(
                cid,
                step_key,
                ts,
                model,
                step_type,
                status_s,
                in_tok,
                cache_read,
                cache_write,
                out_tok,
                tool_names,
                thinking_chars,
                0,
                0,
                zero_cost,
            )

    min_utc = dt.datetime.min.replace(tzinfo=utc)
    ordered = sorted(
        turns_by_step.values(),
        key=lambda t: (
            t.step_index if t.step_index >= 0 else 10**9,
            t.timestamp or min_utc,
        ),
    )
    if seen_user_input:
        subagent_parent_id = None
        subagent_preview = ""
        is_swarm_subagent = False
    return (
        ordered,
        first_user_preview,
        subagent_parent_id,
        subagent_preview,
        is_swarm_subagent,
        skipped_lines,
    )


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
    index_path: pathlib.Path,
    root_dir: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> dict[str, dict[str, str]]:
    """Read optional Claude Code projects/<proj>/sessions-index.json."""
    text = _safe_read_text(index_path, root_dir, resolved_root=resolved_root)
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


def _parse_claude_agent_meta(
    meta_path: pathlib.Path,
    root_dir: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> tuple[str, str]:
    """Read optional subagents/agent-<id>.meta.json -> (agent_type, description)."""
    text = _safe_read_text(meta_path, root_dir, resolved_root=resolved_root)
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
    intern = sys.intern
    decode = _JSON_DECODE
    parse_utc = _parse_utc_timestamp_fast

    with jsonl_path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=65536) as fh:
        if hasattr(fh, "_CHUNK_SIZE"):
            fh._CHUNK_SIZE = 65536
        for line_idx, line in enumerate(fh):
            if not line:
                continue
            if (
                '"assistant"' not in line
                and '"custom-title"' not in line
                and '"summary"' not in line
                and (first_user_preview or '"user"' not in line)
                and (cwd or '"cwd"' not in line)
                and (session_id_in_file or '"sessionId"' not in line)
                and (not is_subagent or agent_id_in_file or '"agentId"' not in line)
            ):
                if line[0] == "{" and (
                    line.endswith("}\n") or line.endswith("}\r\n") or line.endswith("}")
                ):
                    continue
                stripped = line.strip()
                if not stripped or (stripped[0] == "{" and stripped[-1] == "}"):
                    continue
            try:
                obj = decode(line)
            except json.JSONDecodeError:
                if not line.strip():
                    continue
                skipped_lines += 1
                continue
            if type(obj) is not dict and not isinstance(obj, dict):
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

            ts = parse_utc(obj.get("timestamp"))
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
                            chunk_tools.append((tid, intern(tname)))
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
                    "model": intern(raw_model) if raw_model else "unknown",
                    "status": intern(stop_reason) if stop_reason else "DONE",
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
                    existing["model"] = intern(raw_model)
                if stop_reason:
                    existing["status"] = intern(stop_reason)
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
        tools_t = tuple(entry["tools"]) if entry["tools"] else ()
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
                tool_names=tools_t,
                thinking_chars=entry["thinking_chars"],
                cache_write_1h_tokens=entry["cache_write_1h_tokens"],
                cost=_ZERO_COST,
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


def _load_codex_session_index(
    root_dir: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> dict[str, str]:
    """Read optional <codex_home>/session_index.jsonl -> {session_id: title}."""
    idx_path = root_dir / "session_index.jsonl"
    text = _safe_read_text(idx_path, root_dir, resolved_root=resolved_root)
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
    intern = sys.intern
    decode = _JSON_DECODE
    parse_utc = _parse_utc_timestamp_fast

    with rollout_path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=65536) as fh:
        if hasattr(fh, "_CHUNK_SIZE"):
            fh._CHUNK_SIZE = 65536
        for line in fh:
            if not line:
                continue
            if (
                '"token_count"' not in line
                and '"turn_context"' not in line
                and '"session_meta"' not in line
                and '"agent_reasoning"' not in line
                and '"reasoning"' not in line
                and '"function_call"' not in line
                and '"custom_tool_call"' not in line
                and '"local_shell_call"' not in line
                and '"web_search_call"' not in line
                and '"mcp_tool_call"' not in line
                and (first_user_preview or ('"user_message"' not in line and '"user"' not in line))
            ):
                if line[0] == "{" and (
                    line.endswith("}\n") or line.endswith("}\r\n") or line.endswith("}")
                ):
                    continue
                stripped = line.strip()
                if not stripped or (stripped[0] == "{" and stripped[-1] == "}"):
                    continue
            try:
                obj = decode(line)
            except json.JSONDecodeError:
                if not line.strip():
                    continue
                skipped_lines += 1
                continue
            if type(obj) is not dict and not isinstance(obj, dict):
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
                    current_model = intern(payload["model"].strip())
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
                    current_model = intern(mdl)
                if not cwd and isinstance(payload.get("cwd"), str) and payload["cwd"].strip():
                    cwd = payload["cwd"].strip()
                continue

            if rec_type == "response_item":
                ptype = str(payload.get("type") or "")
                if ptype in _CODEX_TOOL_ITEM_TYPES:
                    tname = str(payload.get("name") or ptype).strip()
                    if tname:
                        buffered_tools.append(intern(tname))
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

            ts = parse_utc(obj.get("timestamp"))
            if current_model:
                turn_model = current_model
            else:
                info_m = str(info.get("model") or "").strip()
                turn_model = intern(info_m) if info_m else "unknown"
            tools_t = tuple(buffered_tools) if buffered_tools else ()
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
                    tool_names=tools_t,
                    thinking_chars=buffered_thinking_chars,
                    reasoning_tokens=d_reason,
                    cost=_ZERO_COST,
                )
            )
            buffered_tools = []
            buffered_thinking_chars = 0

    if turns and (buffered_tools or buffered_thinking_chars > 0):
        last_t = turns[-1]
        if buffered_tools:
            last_t.tool_names = last_t.tool_names + tuple(buffered_tools)
        last_t.thinking_chars += buffered_thinking_chars

    # Ensure conversation_id on every turn reflects final session_id
    if turns and any(t.conversation_id != session_id for t in turns):
        for t in turns:
            t.conversation_id = session_id

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


def _load_gemini_projects_map(
    home_root: pathlib.Path,
    *,
    resolved_root: pathlib.Path | None = None,
) -> dict[str, str]:
    """Read optional <gemini_home>/projects.json -> {slug_or_hash: workspace_path}."""
    proj_path = home_root / "projects.json"
    text = _safe_read_text(proj_path, home_root, resolved_root=resolved_root)
    if not text:
        return {}
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(payload, dict):
        return {}
    import hashlib

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


def _rebuild_gemini_id_index(messages: list[dict[str, Any]]) -> dict[str, int]:
    idx_map: dict[str, int] = {}
    for idx, m in enumerate(messages):
        raw_id = str(m.get("id") or "")
        if raw_id and raw_id not in idx_map:
            idx_map[raw_id] = idx
    return idx_map


def _replay_gemini_jsonl(session_path: pathlib.Path) -> tuple[str, str, list[dict[str, Any]], int]:
    """Replay Gemini CLI append-only session-*.jsonl including $set and $rewindTo mutations."""
    session_id = session_path.stem
    summary = ""
    messages: list[dict[str, Any]] = []
    id_to_idx: dict[str, int] = {}
    skipped_lines = 0
    decode = _JSON_DECODE

    with session_path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=65536) as fh:
        if hasattr(fh, "_CHUNK_SIZE"):
            fh._CHUNK_SIZE = 65536
        for line in fh:
            if not line:
                continue
            try:
                obj = decode(line)
            except json.JSONDecodeError:
                if not line.strip():
                    continue
                skipped_lines += 1
                continue
            if type(obj) is not dict and not isinstance(obj, dict):
                continue

            if "$rewindTo" in obj:
                target = obj["$rewindTo"]
                if isinstance(target, dict):
                    target = target.get("messageId") or target.get("id")
                if isinstance(target, str) and target.strip():
                    tid = target.strip()
                    idx = id_to_idx.get(tid)
                    if idx is not None:
                        messages = messages[:idx]
                        id_to_idx = _rebuild_gemini_id_index(messages)
                elif isinstance(target, int) and not isinstance(target, bool) and target >= 0:
                    messages = messages[:target]
                    id_to_idx = _rebuild_gemini_id_index(messages)
                continue

            if "$set" in obj and isinstance(obj["$set"], dict):
                patch = obj["$set"]
                target_id = str(obj.get("id") or obj.get("messageId") or "").strip()
                if target_id:
                    idx = id_to_idx.get(target_id)
                    if idx is not None:
                        messages[idx].update(patch)
                        new_id = str(messages[idx].get("id") or "")
                        if new_id != target_id:
                            id_to_idx = _rebuild_gemini_id_index(messages)
                else:
                    if isinstance(patch.get("summary"), str):
                        summary = _clean_single_line(patch["summary"])
                    if isinstance(patch.get("sessionId"), str) and patch["sessionId"].strip():
                        session_id = patch["sessionId"].strip()
                    if isinstance(patch.get("messages"), list):
                        messages = [dict(m) for m in patch["messages"] if isinstance(m, dict)]
                        id_to_idx = _rebuild_gemini_id_index(messages)
                    elif isinstance(patch.get("id"), str) and patch["id"].strip():
                        pid = patch["id"].strip()
                        idx = id_to_idx.get(pid)
                        if idx is not None:
                            messages[idx].update(patch)
                continue

            rec_type = str(obj.get("type") or "")
            if rec_type in ("session_metadata", "session", "header", "metadata") or (
                "sessionId" in obj and not rec_type and "tokens" not in obj
            ):
                if isinstance(obj.get("sessionId"), str) and obj["sessionId"].strip():
                    session_id = obj["sessionId"].strip()
                if isinstance(obj.get("summary"), str) and obj["summary"].strip():
                    summary = _clean_single_line(obj["summary"])
                if isinstance(obj.get("messages"), list):
                    messages = [dict(m) for m in obj["messages"] if isinstance(m, dict)]
                    id_to_idx = _rebuild_gemini_id_index(messages)
                continue

            if rec_type == "message_update":
                target_id = str(obj.get("id") or obj.get("messageId") or "").strip()
                patch = (
                    obj.get("patch")
                    if isinstance(obj.get("patch"), dict)
                    else {k: v for k, v in obj.items() if k not in ("type", "messageId")}
                )
                if target_id:
                    idx = id_to_idx.get(target_id)
                    if idx is not None:
                        messages[idx].update(patch)
                        new_id = str(messages[idx].get("id") or "")
                        if new_id != target_id:
                            id_to_idx = _rebuild_gemini_id_index(messages)
                continue

            if rec_type or "id" in obj or "tokens" in obj:
                raw_id = str(obj.get("id") or "")
                mid = raw_id.strip()
                if mid:
                    idx = id_to_idx.get(mid)
                    if idx is not None:
                        messages[idx] = obj
                        if raw_id != mid:
                            id_to_idx = _rebuild_gemini_id_index(messages)
                    else:
                        if raw_id and raw_id not in id_to_idx:
                            id_to_idx[raw_id] = len(messages)
                        messages.append(obj)
                else:
                    messages.append(obj)

    return session_id, summary, messages, skipped_lines


def _parse_gemini_session_file(session_path: pathlib.Path) -> dict[str, Any]:
    """Parse a Gemini CLI session-*.json or session-*.jsonl file strictly read-only in UTC."""
    skipped_lines = 0
    intern = sys.intern
    parse_utc = _parse_utc_timestamp_fast
    if session_path.suffix.lower() == ".jsonl":
        session_id, summary, messages, skipped_lines = _replay_gemini_jsonl(session_path)
    else:
        try:
            with session_path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=65536) as fh:
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

        raw_tc = msg.get("toolCalls")
        if isinstance(raw_tc, list) and raw_tc:
            tlist: list[str] = []
            for tc in raw_tc:
                if isinstance(tc, dict):
                    tname = str(tc.get("name") or "").strip()
                    if tname:
                        tlist.append(intern(tname))
            tool_names: tuple[str, ...] = tuple(tlist) if tlist else ()
        else:
            tool_names = ()

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

        raw_m = str(msg.get("model") or "").strip()
        step_idx += 1
        turns.append(
            TurnRecord(
                conversation_id=session_id,
                step_index=step_idx,
                timestamp=parse_utc(msg.get("timestamp")),
                model=intern(raw_m) if raw_m else "unknown",
                step_type="GEMINI",
                status="DONE",
                input_tokens=uncached_in,
                cache_read_tokens=cache_read,
                cache_write_tokens=0,
                output_tokens=output_tok,
                tool_names=tool_names,
                thinking_chars=thinking_chars,
                reasoning_tokens=raw_thoughts,
                cost=_ZERO_COST,
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


def _compute_mtime_cutoff_ns(since: dt.date | None, tz: dt.tzinfo) -> int | None:
    """Return safe st_mtime_ns lower bound for `since`, or None if pruning is unsafe."""
    if since is None:
        return None
    since_start_ts = dt.datetime.combine(since, dt.time.min, tzinfo=tz).timestamp()
    # Guard synthetic future-dated fixtures when host clock precedes `since`.
    if since_start_ts > time.time():
        return None
    # 1-hour safety margin for DST shifts, filesystem timestamp granularity, and slight clock skew.
    return int((since_start_ts - 3600.0) * 1_000_000_000)


def _iter_archive_sessions(
    path: pathlib.Path | None,
    *,
    chunk_size: int = 262144,
) -> Iterator[tuple[str, Any]]:
    """Stream-decode top-level entries of archive.json's 'sessions' object in chunks.

    Avoids loading the entire file into one giant str (which inflates 2x-4x to
    UCS-2/UCS-4 if any title contains CJK/emoji) and holds only one session dict
    in memory at a time.
    """
    if path is None or _lstat_regular_file(path) is None:
        return
    dec = json.JSONDecoder()
    ws_match = _JSON_WS_RE.match
    step_size = max(16, chunk_size)
    try:
        with path.open("r", encoding="utf-8", errors="replace", newline="\n", buffering=max(65536, step_size)) as fh:
            buf = fh.read(step_size)
            if not buf:
                return
            idx = 0
            eof = False

            def skip_ws() -> bool:
                nonlocal buf, idx, eof
                while True:
                    if idx < len(buf) and buf[idx] > " ":
                        return True
                    m = ws_match(buf, idx)
                    if m is not None:
                        idx = m.end()
                    if idx < len(buf):
                        return True
                    if eof:
                        return False
                    more = fh.read(step_size)
                    if not more:
                        eof = True
                        return False
                    buf = more
                    idx = 0

            def decode_next() -> tuple[bool, Any]:
                nonlocal buf, idx, eof
                while True:
                    try:
                        ch0 = buf[idx]
                        val, next_idx = dec.raw_decode(buf, idx)
                        # Guard primitives (numbers/bools/null) split right at chunk end.
                        if next_idx == len(buf) and not eof and ch0 not in ('"', "{", "["):
                            more = fh.read(step_size)
                            if not more:
                                eof = True
                            else:
                                buf = buf[idx:] + more
                                idx = 0
                                continue
                        idx = next_idx
                        if idx >= step_size:
                            buf = buf[idx:]
                            idx = 0
                        return True, val
                    except (json.JSONDecodeError, IndexError):
                        if eof:
                            return False, None
                        more = fh.read(step_size)
                        if not more:
                            eof = True
                            return False, None
                        buf = buf[idx:] + more
                        idx = 0

            if not skip_ws() or buf[idx] != "{":
                return
            idx += 1
            while skip_ws() and buf[idx] != "}":
                if buf[idx] != '"':
                    return
                ok, key = decode_next()
                if not ok or not isinstance(key, str):
                    return
                if not skip_ws() or buf[idx] != ":":
                    return
                idx += 1
                if not skip_ws():
                    return
                if key == "sessions":
                    if buf[idx] != "{":
                        return
                    idx += 1
                    while skip_ws() and buf[idx] != "}":
                        if buf[idx] != '"':
                            return
                        ok_k, skey = decode_next()
                        if not ok_k or not isinstance(skey, str):
                            return
                        if not skip_ws() or buf[idx] != ":":
                            return
                        idx += 1
                        if not skip_ws():
                            return
                        ok_v, sval = decode_next()
                        if not ok_v:
                            return
                        yield skey, sval
                        if not skip_ws():
                            return
                        if buf[idx] == ",":
                            idx += 1
                        elif buf[idx] == "}":
                            break
                        else:
                            return
                    return
                ok_skip, _ = decode_next()
                if not ok_skip:
                    return
                if not skip_ws():
                    return
                if buf[idx] == ",":
                    idx += 1
                else:
                    break
    except OSError:
        return


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
        *,
        use_cache: bool = True,
    ) -> None:
        self.use_cache = use_cache
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
        self._transcript_cache: dict[
            str, tuple[int, int, list[TurnRecord], str, str | None, str, bool, int]
        ] = {}
        self._agent_file_cache: dict[str, tuple[int, int, dict[str, Any]]] = {}
        self._ann_cache: dict[str, tuple[int, int, str]] = {}
        self._meta_sigs: dict[str, tuple[int, int]] = {}
        self._meta_caches: dict[str, dict[str, dict[str, Any]]] = {}
        self._aux_json_cache: dict[str, tuple[int, int, Any]] = {}

    def _get_metadata_cache(
        self,
        root_dir: pathlib.Path,
        *,
        resolved_root: pathlib.Path | None = None,
    ) -> dict[str, dict[str, Any]]:
        rk = str(root_dir)
        meta_path = root_dir / "cache" / "conversation_metadata.json"
        st = _lstat_regular_file(meta_path)
        if st is None:
            self._meta_sigs.pop(rk, None)
            self._meta_caches[rk] = {}
            return self._meta_caches[rk]
        sig = (st.st_mtime_ns, st.st_size)
        if not self.use_cache or sig != self._meta_sigs.get(rk):
            self._meta_caches[rk] = _load_metadata_cache(root_dir, resolved_root=resolved_root)
            self._meta_sigs[rk] = sig
        return self._meta_caches[rk]

    def _get_annotation_title(
        self,
        root_dir: pathlib.Path,
        cid: str,
        *,
        resolved_root: pathlib.Path | None = None,
        ann_entries: dict[str, os.DirEntry[str]] | None = None,
    ) -> str:
        fname = f"{cid}.pbtxt"
        if ann_entries is not None:
            entry = ann_entries.get(fname)
            if entry is None:
                return ""
            try:
                if entry.is_symlink() or not entry.is_file(follow_symlinks=False):
                    return ""
                st = entry.stat(follow_symlinks=False)
                mtime_ns, size = st.st_mtime_ns, st.st_size
            except OSError:
                return ""
            ann_path = pathlib.Path(entry.path)
            key = entry.path
        else:
            ann_path = root_dir / "annotations" / fname
            key = str(ann_path)
            st = _lstat_regular_file(ann_path)
            if st is None:
                self._ann_cache.pop(key, None)
                return ""
            mtime_ns, size = st.st_mtime_ns, st.st_size

        if self.use_cache:
            cached = self._ann_cache.get(key)
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                return cached[2]
        title = _parse_annotation_title(
            ann_path, root_dir, resolved_root=resolved_root, skip_lstat=True
        )
        if self.use_cache:
            self._ann_cache[key] = (mtime_ns, size, title)
        return title

    def _get_cached_aux(
        self, path: pathlib.Path, loader: Any, default_val: Any
    ) -> Any:
        key = str(path)
        st = _lstat_regular_file(path)
        if st is None:
            self._aux_json_cache.pop(key, None)
            return default_val
        sig = (st.st_mtime_ns, st.st_size)
        if self.use_cache:
            cached = self._aux_json_cache.get(key)
            if cached is not None and cached[0] == sig[0] and cached[1] == sig[1]:
                return cached[2]
        val = loader()
        if self.use_cache:
            self._aux_json_cache[key] = (sig[0], sig[1], val)
        return val

    def _filter_and_price_turns(
        self,
        raw_turns: Sequence[TurnRecord],
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        *,
        mutate_in_place: bool = False,
    ) -> list[TurnRecord]:
        if not raw_turns:
            return []
        has_cp_or_unknown = False
        for t in raw_turns:
            if t.step_type == "CHECKPOINT" or t.model in ("compaction", "unknown"):
                has_cp_or_unknown = True
                break
        fallback_model = ""
        if has_cp_or_unknown:
            real_totals: dict[str, int] = defaultdict(int)
            for t in raw_turns:
                if t.step_type != "CHECKPOINT" and t.model not in ("compaction", "unknown"):
                    real_totals[t.model] += t.total_tokens
            if real_totals:
                fallback_model = max(real_totals.items(), key=lambda kv: kv[1])[0]
        last_real_model = ""
        price_turn = self.pricing.price_turn
        is_utc = tz == dt.timezone.utc

        filtered_turns: list[TurnRecord] = []
        for turn in raw_turns:
            tm = turn.model
            if has_cp_or_unknown:
                is_cp = turn.step_type == "CHECKPOINT" or tm == "compaction"
                if not is_cp and tm not in ("compaction", "unknown"):
                    last_real_model = tm
                    eff_model = tm
                elif tm not in ("compaction", "unknown"):
                    eff_model = tm
                else:
                    eff_model = last_real_model or fallback_model or tm
            else:
                eff_model = tm

            ts = turn.timestamp
            if ts is None:
                local_ts = None
            elif is_utc and (ts.tzinfo is tz or ts.tzinfo == tz):
                local_ts = ts
            else:
                local_ts = ts.astimezone(tz)

            local_date = local_ts.date() if local_ts is not None else None
            if since is not None and (local_date is None or local_date < since):
                continue
            if until is not None and (local_date is None or local_date > until):
                continue
            if model_needle and model_needle not in tm.lower():
                continue
            turn_cost = price_turn(
                eff_model,
                turn.input_tokens,
                turn.cache_read_tokens,
                turn.cache_write_tokens,
                turn.output_tokens,
                local_date,
                cache_write_1h_tokens=turn.cache_write_1h_tokens,
            )
            if mutate_in_place:
                turn.timestamp = local_ts
                turn.cost = turn_cost
                filtered_turns.append(turn)
            else:
                filtered_turns.append(
                    TurnRecord(
                        conversation_id=turn.conversation_id,
                        step_index=turn.step_index,
                        timestamp=local_ts,
                        model=tm,
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

    def _filter_and_fold_turns(
        self,
        raw_turns: Sequence[TurnRecord],
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
    ) -> tuple[TokenCounts, set[str], dt.datetime | None, dt.datetime | None, _SessionDigest] | None:
        if not raw_turns:
            return None
        has_cp_or_unknown = False
        for t in raw_turns:
            if t.step_type == "CHECKPOINT" or t.model in ("compaction", "unknown"):
                has_cp_or_unknown = True
                break
        fallback_model = ""
        if has_cp_or_unknown:
            real_totals: dict[str, int] = defaultdict(int)
            for t in raw_turns:
                if t.step_type != "CHECKPOINT" and t.model not in ("compaction", "unknown"):
                    real_totals[t.model] += t.total_tokens
            if real_totals:
                fallback_model = max(real_totals.items(), key=lambda kv: kv[1])[0]

        last_real_model = ""
        resolve_rate = self.pricing.resolve_rate
        is_utc = tz == _UTC
        inv_m = 1e-6

        counts = TokenCounts()
        models: set[str] = set()
        first_time: dt.datetime | None = None
        last_time: dt.datetime | None = None
        times: list[dt.datetime] = []
        by_model: dict[str, TokenCounts] = {}
        by_day: dict[str, TokenCounts] = {}
        by_hour: dict[int, TokenCounts] = {}
        day_to_wm: dict[str, tuple[str, str]] = {}
        date_cache: dict[tuple[int, int, int], str] = {}
        tool_counts: dict[str, int] = {}
        model_tokens: dict[str, tuple[bool, int]] = {}
        thinking_turns = 0

        for turn in raw_turns:
            tm = turn.model
            if has_cp_or_unknown:
                is_cp = turn.step_type == "CHECKPOINT" or tm == "compaction"
                if not is_cp and tm not in ("compaction", "unknown"):
                    last_real_model = tm
                    eff_model = tm
                elif tm not in ("compaction", "unknown"):
                    eff_model = tm
                else:
                    eff_model = last_real_model or fallback_model or tm
            else:
                eff_model = tm

            ts = turn.timestamp
            if ts is None:
                local_ts = None
            elif is_utc and (ts.tzinfo is tz or ts.tzinfo == tz):
                local_ts = ts
            else:
                local_ts = ts.astimezone(tz)

            local_date = local_ts.date() if local_ts is not None else None
            if since is not None and (local_date is None or local_date < since):
                continue
            if until is not None and (local_date is None or local_date > until):
                continue
            if model_needle and model_needle not in tm.lower():
                continue

            in_t = turn.input_tokens
            cr_t = turn.cache_read_tokens
            cw_t = turn.cache_write_tokens
            out_t = turn.output_tokens
            ctx = in_t + cr_t + cw_t
            rate = resolve_rate(eff_model, ctx, local_date)
            if rate is None:
                in_usd = 0.0
                cr_usd = 0.0
                cw_usd = 0.0
                out_usd = 0.0
                saved_usd = 0.0
                unpriced = 1
            else:
                in_usd = in_t * inv_m * rate.input
                cr_usd = cr_t * inv_m * rate.cache_read
                if cw_t > 0:
                    cw_1h_raw = turn.cache_write_1h_tokens
                    if cw_1h_raw > 0:
                        cw_1h = cw_1h_raw if cw_1h_raw < cw_t else cw_t
                        cw_usd = (
                            (cw_t - cw_1h) * inv_m * rate.cache_write
                            + cw_1h * inv_m * (rate.input * 2.0)
                        )
                    else:
                        cw_usd = cw_t * inv_m * rate.cache_write
                else:
                    cw_usd = 0.0
                out_usd = out_t * inv_m * rate.output
                saved_usd = (
                    cr_t * inv_m * (rate.input - rate.cache_read)
                    if cr_t > 0 and rate.input > rate.cache_read
                    else 0.0
                )
                unpriced = 0

            counts.add_call_inline(
                in_t, cr_t, cw_t, out_t, ctx, in_usd, cr_usd, cw_usd, out_usd, saved_usd, unpriced
            )
            if tm:
                models.add(tm)

            bm = by_model.get(tm)
            if bm is None:
                bm = TokenCounts()
                by_model[tm] = bm
            bm.add_call_inline(
                in_t, cr_t, cw_t, out_t, ctx, in_usd, cr_usd, cw_usd, out_usd, saved_usd, unpriced
            )

            is_cp_step = turn.step_type == "CHECKPOINT"
            tot_tok = ctx + out_t
            prev_mt = model_tokens.get(tm)
            if prev_mt is None:
                model_tokens[tm] = (is_cp_step, tot_tok)
            else:
                model_tokens[tm] = (prev_mt[0] and is_cp_step, prev_mt[1] + tot_tok)

            if turn.thinking_chars > 0 or turn.reasoning_tokens > 0:
                thinking_turns += 1
            if turn.tool_names:
                for tn in turn.tool_names:
                    tool_counts[tn] = tool_counts.get(tn, 0) + 1

            if local_ts is not None:
                times.append(local_ts)
                if first_time is None or local_ts < first_time:
                    first_time = local_ts
                if last_time is None or local_ts > last_time:
                    last_time = local_ts
                ymd = (local_ts.year, local_ts.month, local_ts.day)
                day_key = date_cache.get(ymd)
                if day_key is None:
                    day_key = f"{local_ts.year:04d}-{local_ts.month:02d}-{local_ts.day:02d}"
                    iso = local_ts.isocalendar()
                    day_to_wm[day_key] = (f"{iso.year}-W{iso.week:02d}", day_key[:7])
                    date_cache[ymd] = day_key
                hr = local_ts.hour
                bh = by_hour.get(hr)
                if bh is None:
                    bh = TokenCounts()
                    by_hour[hr] = bh
                bh.add_call_inline(
                    in_t, cr_t, cw_t, out_t, ctx, in_usd, cr_usd, cw_usd, out_usd, saved_usd, unpriced
                )
            else:
                day_key = "unknown"

            bd = by_day.get(day_key)
            if bd is None:
                bd = TokenCounts()
                by_day[day_key] = bd
            bd.add_call_inline(
                in_t, cr_t, cw_t, out_t, ctx, in_usd, cr_usd, cw_usd, out_usd, saved_usd, unpriced
            )

        if counts.calls == 0:
            return None
        digest = _SessionDigest(
            by_model=by_model,
            by_day=by_day,
            by_hour=by_hour,
            day_to_wm=day_to_wm,
            tool_counts=tool_counts,
            model_tokens=model_tokens,
            active_duration_secs=_active_duration_from_times(times),
            thinking_turns=thinking_turns,
        )
        return counts, models, first_time, last_time, digest

    def _load_archived_backfill(
        self,
        live_keys: set[tuple[str, str]],
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        model_needle: str | None,
        session_needle: str | None,
        *,
        retain_turns: bool = True,
        scalar_only: bool = False,
    ) -> list[SessionInfo]:
        if self.archive_path is None:
            return []
        min_day_prefix = (since - dt.timedelta(days=1)).isoformat() if since is not None else None
        max_day_prefix = (until + dt.timedelta(days=1)).isoformat() if until is not None else None
        has_date_bounds = (min_day_prefix is not None) or (max_day_prefix is not None)
        intern = sys.intern
        parse_utc = _parse_utc_timestamp_fast

        out: list[SessionInfo] = []
        for raw_key, s_obj in _iter_archive_sessions(self.archive_path):
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

            if has_date_bounds:
                any_in_window = False
                for t_obj in raw_turns:
                    if not isinstance(t_obj, dict):
                        continue
                    raw_ts = t_obj.get("ts")
                    if not isinstance(raw_ts, str):
                        continue
                    s_ts = raw_ts.strip()
                    if len(s_ts) < 10 or s_ts[4] != "-" or s_ts[7] != "-":
                        any_in_window = True
                        break
                    d_pfx = s_ts[:10]
                    if (min_day_prefix is None or d_pfx >= min_day_prefix) and (
                        max_day_prefix is None or d_pfx <= max_day_prefix
                    ):
                        any_in_window = True
                        break
                if not any_in_window:
                    continue

            parsed_turns: list[TurnRecord] = []
            for t_obj in raw_turns:
                if not isinstance(t_obj, dict):
                    continue
                ts = parse_utc(t_obj.get("ts"))
                is_cp = bool(t_obj.get("cp"))
                raw_m = t_obj.get("m")
                model = intern(str(raw_m)) if raw_m else ("compaction" if is_cp else "unknown")
                if scalar_only:
                    tools: tuple[str, ...] = ()
                    th_val = 0
                else:
                    tools_raw = t_obj.get("tools")
                    if isinstance(tools_raw, list) and tools_raw:
                        tools = tuple(intern(x) for x in tools_raw if isinstance(x, str))
                    else:
                        tools = ()
                    th_val = _to_non_neg_int(t_obj.get("th"))
                cw = _to_non_neg_int(t_obj.get("cw"))
                cw1h = min(_to_non_neg_int(t_obj.get("cw1h")), cw) if cw > 0 else 0
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
                        thinking_chars=th_val,
                        reasoning_tokens=_to_non_neg_int(t_obj.get("rt")),
                        cache_write_1h_tokens=cw1h,
                        cost=_ZERO_COST,
                    )
                )
            if retain_turns:
                filtered_turns = self._filter_and_price_turns(
                    parsed_turns, tz, since, until, model_needle, mutate_in_place=True
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
            else:
                folded = self._filter_and_fold_turns(
                    parsed_turns, tz, since, until, model_needle
                )
                if folded is None:
                    continue
                counts, models, first_time, last_time, digest = folded
                sess = SessionInfo(
                    conversation_id=cid,
                    title=str(s_obj.get("title") or "(archived)"),
                    workspace=str(s_obj.get("workspace") or "(unknown)"),
                    source=src,
                    is_archived=True,
                    is_subagent=bool(s_obj.get("is_subagent")),
                    parent_id=str(s_obj["parent_id"]) if s_obj.get("parent_id") else None,
                    parent_title=str(s_obj.get("parent_title") or ""),
                    turns=[],
                    counts=counts,
                    models=models,
                    first_time=first_time,
                    last_time=last_time,
                    _digest=digest,
                )
            out.append(sess)
        return out

    def _scan_antigravity_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        mtime_cutoff_ns: int | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        discovered_live_keys: set[tuple[str, str]],
        seen_agy_keys: set[str],
        ann_indices: dict[str, tuple[dict[str, os.DirEntry[str]], pathlib.Path]],
        warnings: list[str],
        *,
        retain_turns: bool = True,
        scalar_only: bool = False,
        skip_meta: bool = False,
        retain_thinking_len: bool = True,
        utc_date_bounds: tuple[str | None, str | None] | None = None,
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if _lstat_dir(root_dir) is None:
            return False, 0, 0, 0, 0
        brain_dir = root_dir / "brain"
        if _lstat_dir(brain_dir) is None:
            return False, 0, 0, 0, 0
        try:
            resolved_root = root_dir.resolve(strict=True)
        except OSError:
            return False, 0, 0, 0, 0

        source_name = spec.source_name
        ann_entries: dict[str, os.DirEntry[str]] = {}
        if skip_meta:
            meta_cache: dict[str, dict[str, Any]] = {}
        else:
            meta_cache = self._get_metadata_cache(root_dir, resolved_root=resolved_root)
            ann_dir = root_dir / "annotations"
            if _lstat_dir(ann_dir) is not None:
                try:
                    with os.scandir(ann_dir) as it:
                        for ent in it:
                            if ent.name.endswith(".pbtxt"):
                                ann_entries[ent.name] = ent
                except OSError:
                    pass
            ann_indices[str(root_dir)] = (ann_entries, resolved_root)

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        try:
            with os.scandir(brain_dir) as it:
                conv_entries = sorted(it, key=lambda e: e.name)
        except OSError as exc:
            warnings.append(f"Failed to list {brain_dir}: {exc}")
            return True, 0, 0, 0, 0

        mutate_turns = not self.use_cache
        path_join = os.path.join
        lstat_reg_str = _lstat_regular_file_str
        for entry in conv_entries:
            try:
                if entry.is_symlink() or not entry.is_dir(follow_symlinks=False):
                    continue
            except OSError:
                continue
            cid = entry.name
            if session_needle and not cid.lower().startswith(session_needle):
                continue

            logs_dir_str = path_join(entry.path, ".system_generated", "logs")
            transcript_str = path_join(logs_dir_str, "transcript.jsonl")
            st = lstat_reg_str(transcript_str)
            if st is None:
                fallback_str = path_join(logs_dir_str, "transcript_full.jsonl")
                st = lstat_reg_str(fallback_str)
                if st is not None:
                    transcript_str = fallback_str
                else:
                    skipped_empty += 1
                    continue

            cache_key = transcript_str
            seen_agy_keys.add(cache_key)
            mtime_ns, size = st.st_mtime_ns, st.st_size

            if mtime_cutoff_ns is not None and mtime_ns < mtime_cutoff_ns:
                if size > 0:
                    discovered_live_keys.add(("antigravity", cid))
                skipped_empty += 1
                continue

            transcript_path = pathlib.Path(transcript_str)
            if not _is_within_dir(transcript_path, root_dir, resolved_root=resolved_root):
                continue

            cached = self._transcript_cache.get(cache_key) if self.use_cache else None
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                (
                    raw_turns,
                    first_user_preview,
                    subagent_parent_id,
                    subagent_preview,
                    is_swarm_subagent,
                    file_skipped,
                ) = (
                    cached[2],
                    cached[3],
                    cached[4],
                    cached[5],
                    cached[6],
                    cached[7],
                )
                files_cached += 1
            else:
                try:
                    (
                        raw_turns,
                        first_user_preview,
                        subagent_parent_id,
                        subagent_preview,
                        is_swarm_subagent,
                        file_skipped,
                    ) = _parse_transcript_file(
                        transcript_path,
                        cid,
                        scalar_only=scalar_only,
                        skip_meta=skip_meta,
                        retain_thinking_len=retain_thinking_len,
                        utc_date_bounds=utc_date_bounds,
                    )
                except OSError as exc:
                    warnings.append(f"Could not read {transcript_path}: {exc}")
                    continue
                if self.use_cache:
                    self._transcript_cache[cache_key] = (
                        mtime_ns,
                        size,
                        raw_turns,
                        first_user_preview,
                        subagent_parent_id,
                        subagent_preview,
                        is_swarm_subagent,
                        file_skipped,
                    )
                files_reparsed += 1

            skipped_lines += file_skipped
            if raw_turns:
                discovered_live_keys.add(("antigravity", cid))

            if retain_turns:
                filtered_turns = self._filter_and_price_turns(
                    raw_turns, tz, since, until, model_needle, mutate_in_place=mutate_turns
                )
                if not filtered_turns:
                    skipped_empty += 1
                    continue
                folded_tuple = None
            else:
                folded_tuple = self._filter_and_fold_turns(
                    raw_turns, tz, since, until, model_needle
                )
                if folded_tuple is None:
                    skipped_empty += 1
                    continue
                filtered_turns = []

            if skip_meta:
                is_subagent = False
                parent_id = None
                title = ""
                workspace = "(unknown)"
            else:
                ann_title = self._get_annotation_title(
                    root_dir, cid, resolved_root=resolved_root, ann_entries=ann_entries
                )
                meta = meta_cache.get(cid)
                if meta is not None:
                    is_subagent = bool(meta.get("is_internal"))
                else:
                    is_subagent = (subagent_parent_id is not None) or is_swarm_subagent
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

            if folded_tuple is not None:
                counts, models, first_time, last_time, digest = folded_tuple
                session = SessionInfo(
                    conversation_id=cid,
                    title=title,
                    workspace=workspace,
                    transcript_path=cache_key,
                    source=source_name,
                    is_subagent=is_subagent,
                    parent_id=parent_id,
                    turns=[],
                    counts=counts,
                    models=models,
                    first_time=first_time,
                    last_time=last_time,
                    _digest=digest,
                )
            else:
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
            if prev is None or session.counts.calls > prev.counts.calls:
                sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_claude_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        mtime_cutoff_ns: int | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        discovered_live_keys: set[tuple[str, str]],
        seen_agent_keys: set[str],
        claude_parent_meta: dict[str, tuple[str, str]],
        pruned_claude_parents: dict[str, tuple[pathlib.Path, pathlib.Path, pathlib.Path]],
        warnings: list[str],
        *,
        retain_turns: bool = True,
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if _lstat_dir(root_dir) is None:
            return False, 0, 0, 0, 0
        projects_dir = root_dir / "projects"
        if _lstat_dir(projects_dir) is None:
            return False, 0, 0, 0, 0
        try:
            resolved_root = root_dir.resolve(strict=True)
        except OSError:
            return False, 0, 0, 0, 0

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        try:
            with os.scandir(projects_dir) as it:
                proj_entries = sorted(it, key=lambda e: e.name)
        except OSError as exc:
            warnings.append(f"Failed to list {projects_dir}: {exc}")
            return True, 0, 0, 0, 0

        mutate_turns = not self.use_cache
        for proj_ent in proj_entries:
            try:
                if proj_ent.is_symlink() or not proj_ent.is_dir(follow_symlinks=False):
                    continue
            except OSError:
                continue
            proj_dir = pathlib.Path(proj_ent.path)
            if not _is_within_dir(proj_dir, root_dir, resolved_root=resolved_root):
                continue

            idx_path = proj_dir / "sessions-index.json"
            sess_index: dict[str, dict[str, str]] = self._get_cached_aux(
                idx_path,
                lambda p=idx_path, r=root_dir, rr=resolved_root: _load_claude_sessions_index(
                    p, r, resolved_root=rr
                ),
                {},
            )
            for sid, info in sess_index.items():
                if info.get("title") or info.get("workspace"):
                    claude_parent_meta.setdefault(sid, (info.get("title", ""), info.get("workspace", "")))

            # Collect main session JSONL files and subagent JSONL files strictly under projects/<proj>/
            file_jobs: list[tuple[pathlib.Path, os.stat_result, bool, str | None, str]] = []
            try:
                with os.scandir(proj_dir) as it:
                    children = sorted(it, key=lambda e: e.name)
            except OSError as exc:
                warnings.append(f"Failed to list {proj_dir}: {exc}")
                continue

            for child in children:
                try:
                    if child.is_symlink():
                        continue
                    cname = child.name
                    if child.is_file(follow_symlinks=False) and cname.lower().endswith(".jsonl"):
                        stem = cname[:-6]
                        if _is_allowed_claude_session_stem(stem):
                            file_jobs.append((pathlib.Path(child.path), child.stat(follow_symlinks=False), False, None, ""))
                    elif child.is_dir(follow_symlinks=False) and _is_allowed_claude_session_stem(cname):
                        sub_dir = pathlib.Path(child.path) / "subagents"
                        if _lstat_dir(sub_dir) is None:
                            continue
                        try:
                            with os.scandir(sub_dir) as sub_it:
                                sub_files = sorted(sub_it, key=lambda e: e.name)
                        except OSError:
                            continue
                        for sf in sub_files:
                            try:
                                sf_name = sf.name
                                if (
                                    not sf.is_symlink()
                                    and sf.is_file(follow_symlinks=False)
                                    and sf_name.startswith("agent-")
                                    and sf_name.lower().endswith(".jsonl")
                                ):
                                    aid_hint = sf_name[len("agent-") : -6]
                                    if _is_allowed_claude_agent_id(aid_hint):
                                        file_jobs.append(
                                            (pathlib.Path(sf.path), sf.stat(follow_symlinks=False), True, cname, aid_hint)
                                        )
                            except OSError:
                                continue
                except OSError:
                    continue

            for fpath, st, is_sub, parent_hint, aid_hint in file_jobs:
                cache_key = f"claude:{fpath}"
                seen_agent_keys.add(cache_key)
                mtime_ns, size = st.st_mtime_ns, st.st_size

                if mtime_cutoff_ns is not None and mtime_ns < mtime_cutoff_ns:
                    if size > 0:
                        est_cid = f"{parent_hint}__{aid_hint}" if (is_sub and parent_hint and aid_hint) else fpath.stem
                        discovered_live_keys.add(("claude", est_cid))
                        if not is_sub:
                            pruned_claude_parents[fpath.stem] = (fpath, root_dir, resolved_root)
                    skipped_empty += 1
                    continue

                if not _is_within_dir(fpath, root_dir, resolved_root=resolved_root):
                    continue

                cached = self._agent_file_cache.get(cache_key) if self.use_cache else None
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
                    if self.use_cache:
                        self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                    files_reparsed += 1

                skipped_lines += int(parsed.get("skipped_lines", 0))
                cid = str(parsed["cid"])
                if parsed["turns"]:
                    discovered_live_keys.add(("claude", cid))
                if not is_sub and (parsed.get("title_hint") or parsed.get("cwd")):
                    prev_pm = claude_parent_meta.get(cid, ("", ""))
                    claude_parent_meta[cid] = (
                        prev_pm[0] or str(parsed.get("title_hint") or ""),
                        prev_pm[1] or str(parsed.get("cwd") or ""),
                    )

                if session_needle and not cid.lower().startswith(session_needle):
                    continue

                raw_turns: list[TurnRecord] = parsed["turns"]
                if retain_turns:
                    filtered_turns = self._filter_and_price_turns(
                        raw_turns, tz, since, until, model_needle, mutate_in_place=mutate_turns
                    )
                    if not filtered_turns:
                        skipped_empty += 1
                        continue
                    folded_tuple = None
                else:
                    folded_tuple = self._filter_and_fold_turns(
                        raw_turns, tz, since, until, model_needle
                    )
                    if folded_tuple is None:
                        skipped_empty += 1
                        continue
                    filtered_turns = []

                parent_id: str | None = parsed.get("parent_id")
                idx_info = sess_index.get(parent_id or cid, {}) if is_sub else sess_index.get(cid, {})
                title = ""
                if is_sub:
                    meta_path = fpath.with_name(f"{fpath.stem}.meta.json")
                    agent_type, desc = self._get_cached_aux(
                        meta_path,
                        lambda p=meta_path, r=root_dir, rr=resolved_root: _parse_claude_agent_meta(
                            p, r, resolved_root=rr
                        ),
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

                if folded_tuple is not None:
                    counts, models, first_time, last_time, digest = folded_tuple
                    session = SessionInfo(
                        conversation_id=cid,
                        title=title,
                        workspace=workspace,
                        transcript_path=str(fpath),
                        source="claude",
                        is_subagent=is_sub,
                        parent_id=parent_id,
                        turns=[],
                        counts=counts,
                        models=models,
                        first_time=first_time,
                        last_time=last_time,
                        _digest=digest,
                    )
                else:
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
                if prev is None or session.counts.calls > prev.counts.calls:
                    sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_codex_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        mtime_cutoff_ns: int | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        discovered_live_keys: set[tuple[str, str]],
        seen_agent_keys: set[str],
        codex_parent_meta: dict[str, tuple[str, str]],
        pruned_codex_parents: dict[str, tuple[pathlib.Path, pathlib.Path, pathlib.Path]],
        warnings: list[str],
        *,
        retain_turns: bool = True,
    ) -> tuple[bool, int, int, int, int]:
        root_dir = spec.root
        if _lstat_dir(root_dir) is None:
            return False, 0, 0, 0, 0
        sessions_dir = root_dir / "sessions"
        archived_dir = root_dir / "archived_sessions"
        has_sessions = _lstat_dir(sessions_dir) is not None
        has_archived = _lstat_dir(archived_dir) is not None
        if not has_sessions and not has_archived:
            return False, 0, 0, 0, 0
        try:
            resolved_root = root_dir.resolve(strict=True)
        except OSError:
            return False, 0, 0, 0, 0

        idx_path = root_dir / "session_index.jsonl"
        index_titles: dict[str, str] = self._get_cached_aux(
            idx_path,
            lambda r=root_dir, rr=resolved_root: _load_codex_session_index(r, resolved_root=rr),
            {},
        )
        for sid, t_val in index_titles.items():
            codex_parent_meta.setdefault(sid, (t_val, ""))

        until_plus_1 = (until + dt.timedelta(days=1)) if until is not None else None
        rollout_files: list[tuple[pathlib.Path, os.stat_result]] = []
        if has_sessions:
            try:
                with os.scandir(sessions_dir) as it:
                    year_dirs = sorted(it, key=lambda e: e.name)
            except OSError as exc:
                warnings.append(f"Failed to list {sessions_dir}: {exc}")
                year_dirs = []
            for y_dir in year_dirs:
                try:
                    if y_dir.is_symlink() or not y_dir.is_dir(follow_symlinks=False):
                        continue
                except OSError:
                    continue
                y_num: int | None = None
                if len(y_dir.name) == 4 and y_dir.name.isdigit():
                    y_num = int(y_dir.name)
                    if until_plus_1 is not None and y_num > until_plus_1.year:
                        continue
                try:
                    with os.scandir(y_dir.path) as it:
                        month_dirs = sorted(it, key=lambda e: e.name)
                except OSError:
                    continue
                for m_dir in month_dirs:
                    try:
                        if m_dir.is_symlink() or not m_dir.is_dir(follow_symlinks=False):
                            continue
                    except OSError:
                        continue
                    m_num: int | None = None
                    if y_num is not None and m_dir.name.isdigit():
                        mv = int(m_dir.name)
                        if 1 <= mv <= 12:
                            m_num = mv
                            if until_plus_1 is not None and (y_num, m_num) > (until_plus_1.year, until_plus_1.month):
                                continue
                    try:
                        with os.scandir(m_dir.path) as it:
                            day_dirs = sorted(it, key=lambda e: e.name)
                    except OSError:
                        continue
                    for d_dir in day_dirs:
                        try:
                            if d_dir.is_symlink() or not d_dir.is_dir(follow_symlinks=False):
                                continue
                        except OSError:
                            continue
                        if until_plus_1 is not None and y_num is not None and m_num is not None and d_dir.name.isdigit():
                            dv = int(d_dir.name)
                            if 1 <= dv <= 31 and (y_num, m_num, dv) > (
                                until_plus_1.year,
                                until_plus_1.month,
                                until_plus_1.day,
                            ):
                                continue
                        try:
                            with os.scandir(d_dir.path) as it:
                                files = sorted(it, key=lambda e: e.name)
                        except OSError:
                            continue
                        for f in files:
                            try:
                                fn = f.name
                                if (
                                    not f.is_symlink()
                                    and f.is_file(follow_symlinks=False)
                                    and fn.startswith("rollout-")
                                    and fn.lower().endswith(".jsonl")
                                ):
                                    rollout_files.append((pathlib.Path(f.path), f.stat(follow_symlinks=False)))
                            except OSError:
                                continue

        if has_archived:
            try:
                with os.scandir(archived_dir) as it:
                    a_files = sorted(it, key=lambda e: e.name)
            except OSError as exc:
                warnings.append(f"Failed to list {archived_dir}: {exc}")
                a_files = []
            for f in a_files:
                try:
                    fn = f.name
                    if (
                        not f.is_symlink()
                        and f.is_file(follow_symlinks=False)
                        and fn.startswith("rollout-")
                        and fn.lower().endswith(".jsonl")
                    ):
                        rollout_files.append((pathlib.Path(f.path), f.stat(follow_symlinks=False)))
                except OSError:
                    continue

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0
        mutate_turns = not self.use_cache

        for fpath, st in rollout_files:
            cache_key = f"codex:{fpath}"
            seen_agent_keys.add(cache_key)
            mtime_ns, size = st.st_mtime_ns, st.st_size

            if mtime_cutoff_ns is not None and mtime_ns < mtime_cutoff_ns:
                if size > 0:
                    stem = fpath.stem
                    m_uuid = _CODEX_UUID_TAIL_RE.search(stem)
                    est_cid = m_uuid.group(1) if m_uuid else stem
                    discovered_live_keys.add(("codex", est_cid))
                    pruned_codex_parents[est_cid] = (fpath, root_dir, resolved_root)
                skipped_empty += 1
                continue

            if not _is_within_dir(fpath, root_dir, resolved_root=resolved_root):
                continue

            cached = self._agent_file_cache.get(cache_key) if self.use_cache else None
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                parsed = cached[2]
                files_cached += 1
            else:
                try:
                    parsed = _parse_codex_jsonl(fpath)
                except OSError as exc:
                    warnings.append(f"Could not read {fpath}: {exc}")
                    continue
                if self.use_cache:
                    self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                files_reparsed += 1

            skipped_lines += int(parsed.get("skipped_lines", 0))
            cid = str(parsed["cid"])
            if parsed["turns"]:
                discovered_live_keys.add(("codex", cid))
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

            if retain_turns:
                filtered_turns = self._filter_and_price_turns(
                    parsed["turns"], tz, since, until, model_needle, mutate_in_place=mutate_turns
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
            else:
                folded_tuple = self._filter_and_fold_turns(
                    parsed["turns"], tz, since, until, model_needle
                )
                if folded_tuple is None:
                    skipped_empty += 1
                    continue
                counts, models, first_time, last_time, digest = folded_tuple
                session = SessionInfo(
                    conversation_id=cid,
                    title=title,
                    workspace=workspace,
                    transcript_path=str(fpath),
                    source="codex",
                    is_subagent=is_sub,
                    parent_id=parsed.get("parent_id"),
                    turns=[],
                    counts=counts,
                    models=models,
                    first_time=first_time,
                    last_time=last_time,
                    _digest=digest,
                )

            skey = ("codex", cid)
            prev = sessions_by_key.get(skey)
            if prev is None or session.counts.calls > prev.counts.calls:
                sessions_by_key[skey] = session

        return True, skipped_empty, skipped_lines, files_cached, files_reparsed

    def _scan_gemini_source(
        self,
        spec: SourceSpec,
        tz: dt.tzinfo,
        since: dt.date | None,
        until: dt.date | None,
        mtime_cutoff_ns: int | None,
        model_needle: str | None,
        session_needle: str | None,
        sessions_by_key: dict[tuple[str, str], SessionInfo],
        discovered_live_keys: set[tuple[str, str]],
        seen_agent_keys: set[str],
        warnings: list[str],
        *,
        retain_turns: bool = True,
    ) -> tuple[bool, int, int, int, int]:
        root = spec.root
        if _lstat_dir(root) is None:
            return False, 0, 0, 0, 0
        tmp_candidate = root / "tmp"
        if _lstat_dir(tmp_candidate) is not None:
            tmp_dir = tmp_candidate
            home_root = root
        else:
            tmp_dir = root
            home_root = root.parent
        try:
            resolved_tmp = tmp_dir.resolve(strict=True)
            resolved_home = home_root.resolve(strict=True)
        except OSError:
            return False, 0, 0, 0, 0

        proj_json_path = home_root / "projects.json"
        projects_map: dict[str, str] = self._get_cached_aux(
            proj_json_path,
            lambda hr=home_root, rh=resolved_home: _load_gemini_projects_map(hr, resolved_root=rh),
            {},
        )

        try:
            with os.scandir(tmp_dir) as it:
                proj_entries = sorted(it, key=lambda e: e.name)
        except OSError as exc:
            warnings.append(f"Failed to list {tmp_dir}: {exc}")
            return True, 0, 0, 0, 0

        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0
        mutate_turns = not self.use_cache

        for proj_ent in proj_entries:
            try:
                if proj_ent.is_symlink() or not proj_ent.is_dir(follow_symlinks=False):
                    continue
            except OSError:
                continue
            proj_dir = pathlib.Path(proj_ent.path)
            if not _is_within_dir(proj_dir, tmp_dir, resolved_root=resolved_tmp):
                continue
            chats_dir = proj_dir / "chats"
            if _lstat_dir(chats_dir) is None:
                continue

            pr_path = proj_dir / ".project_root"
            proj_root_txt: str = self._get_cached_aux(
                pr_path,
                lambda p=pr_path, r=tmp_dir, rr=resolved_tmp: (
                    _safe_read_text(p, r, resolved_root=rr) or ""
                ).strip(),
                "",
            )
            workspace = proj_root_txt or projects_map.get(proj_ent.name, "") or "(unknown)"

            try:
                with os.scandir(chats_dir) as it:
                    chat_entries = sorted(it, key=lambda e: e.name)
            except OSError as exc:
                warnings.append(f"Failed to list {chats_dir}: {exc}")
                continue

            for cf in chat_entries:
                try:
                    cf_name = cf.name
                    low_name = cf_name.lower()
                    if (
                        cf.is_symlink()
                        or not cf.is_file(follow_symlinks=False)
                        or not cf_name.startswith("session-")
                        or not (low_name.endswith(".json") or low_name.endswith(".jsonl"))
                    ):
                        continue
                    st = cf.stat(follow_symlinks=False)
                except OSError:
                    continue
                fpath = pathlib.Path(cf.path)
                cache_key = f"gemini:{fpath}"
                seen_agent_keys.add(cache_key)
                mtime_ns, size = st.st_mtime_ns, st.st_size

                if mtime_cutoff_ns is not None and mtime_ns < mtime_cutoff_ns:
                    if size > 0:
                        discovered_live_keys.add(("gemini", fpath.stem))
                    skipped_empty += 1
                    continue

                if not _is_within_dir(fpath, tmp_dir, resolved_root=resolved_tmp):
                    continue

                cached = self._agent_file_cache.get(cache_key) if self.use_cache else None
                if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                    parsed = cached[2]
                    files_cached += 1
                else:
                    try:
                        parsed = _parse_gemini_session_file(fpath)
                    except OSError as exc:
                        warnings.append(f"Could not read {fpath}: {exc}")
                        continue
                    if self.use_cache:
                        self._agent_file_cache[cache_key] = (mtime_ns, size, parsed)
                    files_reparsed += 1

                skipped_lines += int(parsed.get("skipped_lines", 0))
                cid = str(parsed["cid"])
                if parsed["turns"]:
                    discovered_live_keys.add(("gemini", cid))
                if session_needle and not cid.lower().startswith(session_needle):
                    continue

                if retain_turns:
                    filtered_turns = self._filter_and_price_turns(
                        parsed["turns"], tz, since, until, model_needle, mutate_in_place=mutate_turns
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
                else:
                    folded_tuple = self._filter_and_fold_turns(
                        parsed["turns"], tz, since, until, model_needle
                    )
                    if folded_tuple is None:
                        skipped_empty += 1
                        continue
                    counts, models, first_time, last_time, digest = folded_tuple
                    session = SessionInfo(
                        conversation_id=cid,
                        title=str(parsed.get("title_hint") or "") or "(untitled)",
                        workspace=workspace,
                        transcript_path=str(fpath),
                        source="gemini",
                        is_subagent=False,
                        parent_id=None,
                        turns=[],
                        counts=counts,
                        models=models,
                        first_time=first_time,
                        last_time=last_time,
                        _digest=digest,
                    )

                skey = ("gemini", cid)
                prev = sessions_by_key.get(skey)
                if prev is None or session.counts.calls > prev.counts.calls:
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
        *,
        retain_turns: bool = True,
        scalar_only: bool = False,
        skip_meta: bool = False,
    ) -> ScanReport:
        t0 = time.perf_counter()
        self.pricing.refresh()
        warnings: list[str] = []
        sessions_by_key: dict[tuple[str, str], SessionInfo] = {}
        discovered_live_keys: set[tuple[str, str]] = set()
        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0
        found_any_source = False

        model_needle = model_filter.lower().strip() if model_filter else None
        session_needle = session_filter.lower().strip() if session_filter else None
        workspace_needle = workspace_filter.lower().strip() if workspace_filter else None
        mtime_cutoff_ns = _compute_mtime_cutoff_ns(since, tz)
        seen_agy_keys: set[str] = set()
        seen_agent_keys: set[str] = set()
        claude_parent_meta: dict[str, tuple[str, str]] = {}
        codex_parent_meta: dict[str, tuple[str, str]] = {}
        pruned_claude_parents: dict[str, tuple[pathlib.Path, pathlib.Path, pathlib.Path]] = {}
        pruned_codex_parents: dict[str, tuple[pathlib.Path, pathlib.Path, pathlib.Path]] = {}
        ann_indices: dict[str, tuple[dict[str, os.DirEntry[str]], pathlib.Path]] = {}

        # Never pollute reusable per-file cache with partial/date-bounded parses.
        effective_scalar_only = scalar_only and (not self.use_cache)
        effective_skip_meta = skip_meta and (not self.use_cache) and (workspace_needle is None)
        effective_retain_thinking_len = retain_turns or self.use_cache
        utc_date_bounds: tuple[str | None, str | None] | None = None
        if not self.use_cache and (since is not None or until is not None):
            utc_date_bounds = (
                (since - dt.timedelta(days=1)).isoformat() if since is not None else None,
                (until + dt.timedelta(days=1)).isoformat() if until is not None else None,
            )

        for spec in self.sources:
            if self.agent_filter is not None and spec.kind not in self.agent_filter:
                continue
            if spec.kind == "antigravity":
                found, se, sl, fc, fr = self._scan_antigravity_source(
                    spec, tz, since, until, mtime_cutoff_ns, model_needle, session_needle,
                    sessions_by_key, discovered_live_keys, seen_agy_keys, ann_indices, warnings,
                    retain_turns=retain_turns,
                    scalar_only=effective_scalar_only,
                    skip_meta=effective_skip_meta,
                    retain_thinking_len=effective_retain_thinking_len,
                    utc_date_bounds=utc_date_bounds,
                )
            elif spec.kind == "claude":
                found, se, sl, fc, fr = self._scan_claude_source(
                    spec, tz, since, until, mtime_cutoff_ns, model_needle, session_needle,
                    sessions_by_key, discovered_live_keys, seen_agent_keys,
                    claude_parent_meta, pruned_claude_parents, warnings,
                    retain_turns=retain_turns,
                )
            elif spec.kind == "codex":
                found, se, sl, fc, fr = self._scan_codex_source(
                    spec, tz, since, until, mtime_cutoff_ns, model_needle, session_needle,
                    sessions_by_key, discovered_live_keys, seen_agent_keys,
                    codex_parent_meta, pruned_codex_parents, warnings,
                    retain_turns=retain_turns,
                )
            elif spec.kind == "gemini":
                found, se, sl, fc, fr = self._scan_gemini_source(
                    spec, tz, since, until, mtime_cutoff_ns, model_needle, session_needle,
                    sessions_by_key, discovered_live_keys, seen_agent_keys, warnings,
                    retain_turns=retain_turns,
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
        if not effective_skip_meta:
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
                            rk = str(root_dir)
                            ann_pair = ann_indices.get(rk)
                            if ann_pair is not None:
                                p_ann = self._get_annotation_title(
                                    root_dir, pid, resolved_root=ann_pair[1], ann_entries=ann_pair[0]
                                )
                            else:
                                p_ann = self._get_annotation_title(root_dir, pid)
                            p_meta = self._meta_caches.get(rk, {}).get(pid, {})
                            ptitle = p_ann or str(p_meta.get("title") or "") or str(p_meta.get("preview") or "")
                            pws = str(p_meta.get("workspace") or "")
                            if ptitle or pws:
                                resolved = (ptitle, pws)
                                break
                    elif fam == "claude":
                        resolved = claude_parent_meta.get(pid)
                        if resolved is None and pid in pruned_claude_parents:
                            pfpath, proot, pres_root = pruned_claude_parents[pid]
                            if _is_within_dir(pfpath, proot, resolved_root=pres_root):
                                try:
                                    p_parsed = _parse_claude_jsonl(
                                        pfpath, fallback_cid=pfpath.stem, is_subagent=False
                                    )
                                    resolved = (
                                        str(p_parsed.get("title_hint") or ""),
                                        str(p_parsed.get("cwd") or ""),
                                    )
                                    claude_parent_meta[pid] = resolved
                                except OSError:
                                    pass
                    elif fam == "codex":
                        resolved = codex_parent_meta.get(pid)
                        if resolved is None and pid in pruned_codex_parents:
                            pfpath, proot, pres_root = pruned_codex_parents[pid]
                            if _is_within_dir(pfpath, proot, resolved_root=pres_root):
                                try:
                                    p_parsed = _parse_codex_jsonl(pfpath)
                                    resolved = (
                                        str(p_parsed.get("title_hint") or ""),
                                        str(p_parsed.get("cwd") or ""),
                                    )
                                    codex_parent_meta[pid] = resolved
                                except OSError:
                                    pass
                if resolved is not None:
                    ptitle, pws = resolved
                    if ptitle:
                        s.parent_title = ptitle
                    if (not s.workspace or s.workspace == "(unknown)") and pws and pws != "(unknown)":
                        s.workspace = pws
                    if s.title in ("(subagent)", "(untitled)") and ptitle:
                        s.title = f"subtask · {ptitle}"

        if not self.use_cache:
            self._meta_caches.clear()
            self._meta_sigs.clear()
            ann_indices.clear()

        live_keys = discovered_live_keys | set(sessions_by_key.keys())
        archived_backfill = self._load_archived_backfill(
            live_keys,
            tz,
            since,
            until,
            model_needle,
            session_needle,
            retain_turns=retain_turns,
            scalar_only=effective_scalar_only,
        )
        sessions.extend(archived_backfill)

        if workspace_needle:
            sessions = [s for s in sessions if workspace_needle in s.workspace.lower()]

        if not found_any_source and not archived_backfill:
            warnings.append(f"Data directory not found: {self.data_dir}")

        if self.use_cache and not session_needle:
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
    *,
    retain_turns: bool = True,
) -> ScanReport:
    return IncrementalScanner(
        data_dir,
        pricing_path=pricing_path,
        extra_dirs=extra_dirs,
        archive_path=archive_path,
        sources=sources,
        agent_filter=agent_filter,
        use_cache=False,
    ).scan(
        tz=tz,
        since=since,
        until=until,
        model_filter=model_filter,
        session_filter=session_filter,
        workspace_filter=workspace_filter,
        retain_turns=retain_turns,
    )


# ---------- Display & formatting helpers ----------


def _display_width(text: str) -> int:
    if text.isascii():
        return len(text)
    import unicodedata

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
    if text.isascii():
        if len(text) <= max_width:
            return text
        if max_width == 1:
            return "…"
        return text[: max_width - 1] + "…"
    if _display_width(text) <= max_width:
        return text
    if max_width == 1:
        return "…"
    import unicodedata

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
    if text.isascii():
        n = len(text)
        if n > width:
            if width <= 0:
                return ""
            text = "…" if width == 1 else text[: width - 1] + "…"
            n = width
        pad = width - n
        if align == "right":
            return (" " * pad) + text
        return text + (" " * pad)
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

    date_cache: dict[tuple[int, int, int], tuple[str, str, str]] = {}
    day_to_wm: dict[str, tuple[str, str]] = {}

    for s in sessions:
        sc = s.counts
        total.merge(sc)
        total_active_secs += s.active_duration_secs
        ws = s.workspace
        by_workspace[ws].merge(sc)
        workspace_sessions[ws] += 1
        src = s.source
        by_source[src].merge(sc)
        source_sessions[src] += 1
        fam = s.agent_family
        by_agent[fam].merge(sc)
        agent_sessions[fam] += 1
        digest = s._digest
        if digest is not None and not s.turns:
            total_thinking_turns += digest.thinking_turns
            for tool_name, cnt in digest.tool_counts.items():
                by_tool[tool_name] += cnt
            for m_key, m_cnt in digest.by_model.items():
                by_model[m_key].merge(m_cnt)
            for h_key, h_cnt in digest.by_hour.items():
                by_hour[h_key].merge(h_cnt)
            for d_key, d_cnt in digest.by_day.items():
                by_day[d_key].merge(d_cnt)
            if digest.day_to_wm:
                day_to_wm.update(digest.day_to_wm)
            continue
        for t in s.turns:
            if t.thinking_chars > 0 or t.reasoning_tokens > 0:
                total_thinking_turns += 1
            if t.tool_names:
                for tool_name in t.tool_names:
                    by_tool[tool_name] += 1
            in_t = t.input_tokens
            cr_t = t.cache_read_tokens
            cw_t = t.cache_write_tokens
            out_t = t.output_tokens
            cost_t = t.cost
            by_model[t.model].add_call(in_t, cr_t, cw_t, out_t, cost_t)
            ts = t.timestamp
            if ts is not None:
                ymd = (ts.year, ts.month, ts.day)
                dk_entry = date_cache.get(ymd)
                if dk_entry is None:
                    day_key = f"{ts.year:04d}-{ts.month:02d}-{ts.day:02d}"
                    iso = ts.isocalendar()
                    week_key = f"{iso.year}-W{iso.week:02d}"
                    month_key = day_key[:7]
                    dk_entry = (day_key, week_key, month_key)
                    date_cache[ymd] = dk_entry
                    day_to_wm[day_key] = (week_key, month_key)
                else:
                    day_key = dk_entry[0]
                by_hour[ts.hour].add_call(in_t, cr_t, cw_t, out_t, cost_t)
            else:
                day_key = "unknown"
            by_day[day_key].add_call(in_t, cr_t, cw_t, out_t, cost_t)

    # Roll up by_day into by_week and by_month (every calendar day maps to exactly one week & month).
    for day_key, day_counts in by_day.items():
        if day_key == "unknown":
            by_week["unknown"].merge(day_counts)
            by_month["unknown"].merge(day_counts)
        else:
            wk, mk = day_to_wm[day_key]
            week_days[wk].add(day_key)
            by_week[wk].merge(day_counts)
            by_month[mk].merge(day_counts)

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
    digest = s._digest
    if digest is not None and not s.turns:
        by_m = digest.model_tokens
    else:
        by_m = {}
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
            src_digest = src._digest
            if src_digest is not None and not src.turns:
                for m_name, (m_cp, m_tok) in src_digest.model_tokens.items():
                    prev_mt = by_m.get(m_name)
                    if prev_mt is None:
                        by_m[m_name] = (m_cp, m_tok)
                    else:
                        by_m[m_name] = (prev_mt[0] and m_cp, prev_mt[1] + m_tok)
            else:
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
    import csv

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
        import argparse

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
    import argparse

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
    loaded_bcfg: dict[str, float] | None = None
    if args.today:
        since = today
        until = today
    elif args.days is not None:
        since = today - dt.timedelta(days=args.days - 1)
        if until is None:
            until = today
    elif args.compact and since is None and until is None:
        need_weekly_budget = args.budget_weekly is not None
        if not need_weekly_budget and args.check_budget:
            loaded_bcfg = load_budget_config(DEFAULT_BUDGET_PATH)
            need_weekly_budget = loaded_bcfg["weekly_usd"] > 0.0
        if need_weekly_budget:
            iso = today.isocalendar()
            since = dt.date.fromisocalendar(iso.year, iso.week, 1)
        else:
            since = today
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
        agent_filter = {"antigravity"}
    elif args.single_source:
        primary_dir = DEFAULT_DATA_DIR
        sources = [SourceSpec("antigravity", primary_dir, "cli")]
        agent_filter = {"antigravity"}
    else:
        sources = discover_default_sources(agent_filter)
        primary_dir = sources[0].root if sources else DEFAULT_DATA_DIR

    archive_path = None if (args.no_archive or args.data_dir is not None) else DEFAULT_ARCHIVE_PATH
    scanner = IncrementalScanner(
        data_dir=primary_dir,
        pricing_path=pricing_path,
        extra_dirs=extra_dirs,
        archive_path=archive_path,
        sources=sources,
        agent_filter=agent_filter,
        use_cache=False,
    )
    retain_turns = bool(args.session)
    is_compact_fast = bool(args.compact and not args.session and not args.workspace)
    is_pricing_fast = bool(
        args.view == "pricing"
        and args.format == "table"
        and not args.compact
        and not args.workspace
        and not args.session
    )
    scalar_only = bool(is_compact_fast or is_pricing_fast or args.format == "csv")
    skip_meta = bool(is_compact_fast or is_pricing_fast)

    report = scanner.scan(
        tz=tz,
        since=since,
        until=until,
        model_filter=args.model,
        session_filter=args.session,
        workspace_filter=args.workspace,
        retain_turns=retain_turns,
        scalar_only=scalar_only,
        skip_meta=skip_meta,
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
                scanner.pricing,
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
        bcfg = loaded_bcfg if loaded_bcfg is not None else load_budget_config(DEFAULT_BUDGET_PATH)
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
