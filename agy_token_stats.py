#!/usr/bin/env python3
"""Strictly read-only token usage analyzer for Antigravity CLI (agy).

Reads only plain-text local transcript and metadata files under
~/.gemini/antigravity-cli:
  - brain/<conversation-id>/.system_generated/logs/transcript.jsonl
    (falls back to transcript_full.jsonl only if transcript.jsonl is absent)
  - annotations/<conversation-id>.pbtxt (optional, for conversation titles)
  - cache/conversation_metadata.json (optional, for titles/previews/workspaces)

Safety guarantees:
  - Strictly read-only: performs zero file writes, creates no temp/cache files.
  - Never opens SQLite databases (avoids WAL/SHM creation or lock contention
    with live `agy` CLI processes).
  - Never reads credentials, OAuth tokens, settings, or network endpoints.
  - Never invokes subprocesses; uses only the Python standard library.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
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

_USER_REQUEST_RE = re.compile(r"<USER_REQUEST>\s*(.*?)\s*</USER_REQUEST>", re.DOTALL)
_PBTXT_TITLE_RE = re.compile(r'(?m)^\s*title\s*:\s*"((?:\\.|[^"\\])*)"')
_PBTXT_ESCAPE_RE = re.compile(r"\\([0-7]{1,3}|x[0-9a-fA-F]{2}|[abfnrtv\\\"'?])")


@dataclass
class TokenCounts:
    calls: int = 0
    input_tokens: int = 0  # Uncached new input tokens
    cache_read_tokens: int = 0  # Prompt cache hit input tokens
    cache_write_tokens: int = 0  # Prompt cache creation tokens (if reported)
    output_tokens: int = 0  # Output tokens (thinking + response + tool calls)
    peak_context: int = 0  # Max single-call total input tokens

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

    def add_call(
        self,
        input_tokens: int,
        cache_read_tokens: int,
        cache_write_tokens: int,
        output_tokens: int,
    ) -> None:
        self.calls += 1
        self.input_tokens += input_tokens
        self.cache_read_tokens += cache_read_tokens
        self.cache_write_tokens += cache_write_tokens
        self.output_tokens += output_tokens
        ctx = input_tokens + cache_read_tokens + cache_write_tokens
        if ctx > self.peak_context:
            self.peak_context = ctx

    def merge(self, other: "TokenCounts") -> None:
        self.calls += other.calls
        self.input_tokens += other.input_tokens
        self.cache_read_tokens += other.cache_read_tokens
        self.cache_write_tokens += other.cache_write_tokens
        self.output_tokens += other.output_tokens
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


@dataclass
class SessionInfo:
    conversation_id: str
    title: str = ""
    workspace: str = ""
    transcript_path: str = ""
    turns: list[TurnRecord] = field(default_factory=list)
    counts: TokenCounts = field(default_factory=TokenCounts)
    models: set[str] = field(default_factory=set)
    first_time: dt.datetime | None = None
    last_time: dt.datetime | None = None

    def add_turn(self, turn: TurnRecord) -> None:
        self.turns.append(turn)
        self.counts.add_call(
            turn.input_tokens,
            turn.cache_read_tokens,
            turn.cache_write_tokens,
            turn.output_tokens,
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


def _load_metadata_cache(data_dir: pathlib.Path) -> dict[str, dict[str, str]]:
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
    result: dict[str, dict[str, str]] = {}
    for cid, entry in conversations.items():
        if not isinstance(entry, dict):
            continue
        summary = entry.get("summary")
        if not isinstance(summary, dict):
            continue
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
) -> tuple[list[TurnRecord], str, int]:
    """Parse a single transcript JSONL file in UTC (tz conversion happens later)."""
    turns_by_step: dict[int, TurnRecord] = {}
    fallback_order = -1
    first_user_preview = ""
    skipped_lines = 0

    with transcript_path.open("r", encoding="utf-8", errors="replace") as fh:
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

            if (
                not first_user_preview
                and obj.get("type") == "USER_INPUT"
                and isinstance(obj.get("content"), str)
            ):
                first_user_preview = _extract_user_preview(obj["content"])

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
            model = str(obj.get("model") or "unknown").strip() or "unknown"

            raw_step = obj.get("step_index")
            if isinstance(raw_step, int) and not isinstance(raw_step, bool):
                step_key = raw_step
            else:
                step_key = fallback_order
                fallback_order -= 1

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
            )

    ordered = sorted(
        turns_by_step.values(),
        key=lambda t: (
            t.step_index if t.step_index >= 0 else 10**9,
            t.timestamp or dt.datetime.min.replace(tzinfo=dt.timezone.utc),
        ),
    )
    return ordered, first_user_preview, skipped_lines


class IncrementalScanner:
    """In-memory mtime/size cached read-only scanner for low-CPU live polling.

    Never writes to disk; caches parsed immutable per-file results in RAM only.
    """

    def __init__(self, data_dir: pathlib.Path = DEFAULT_DATA_DIR) -> None:
        self.data_dir = data_dir.expanduser()
        self._transcript_cache: dict[str, tuple[int, int, list[TurnRecord], str, int]] = {}
        self._ann_cache: dict[str, tuple[int, int, str]] = {}
        self._meta_sig: tuple[int, int] | None = None
        self._meta_cache: dict[str, dict[str, str]] = {}

    def _get_metadata_cache(self) -> dict[str, dict[str, str]]:
        meta_path = self.data_dir / "cache" / "conversation_metadata.json"
        if meta_path.is_symlink() or not meta_path.is_file():
            self._meta_sig = None
            self._meta_cache = {}
            return self._meta_cache
        try:
            st = meta_path.stat()
            sig = (st.st_mtime_ns, st.st_size)
        except OSError:
            return self._meta_cache
        if sig != self._meta_sig:
            self._meta_cache = _load_metadata_cache(self.data_dir)
            self._meta_sig = sig
        return self._meta_cache

    def _get_annotation_title(self, cid: str) -> str:
        ann_path = self.data_dir / "annotations" / f"{cid}.pbtxt"
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
        title = _parse_annotation_title(ann_path, self.data_dir)
        self._ann_cache[key] = (mtime_ns, size, title)
        return title

    def scan(
        self,
        tz: dt.tzinfo,
        since: dt.date | None = None,
        until: dt.date | None = None,
        model_filter: str | None = None,
        session_filter: str | None = None,
    ) -> ScanReport:
        t0 = time.perf_counter()
        warnings: list[str] = []
        sessions: list[SessionInfo] = []
        skipped_empty = 0
        skipped_lines = 0
        files_cached = 0
        files_reparsed = 0

        if not self.data_dir.exists() or not self.data_dir.is_dir():
            warnings.append(f"Data directory not found: {self.data_dir}")
            return ScanReport(data_dir=self.data_dir, sessions=[], warnings=warnings)

        brain_dir = self.data_dir / "brain"
        if not brain_dir.exists() or not brain_dir.is_dir():
            warnings.append(f"Brain directory not found: {brain_dir}")
            return ScanReport(data_dir=self.data_dir, sessions=[], warnings=warnings)

        meta_cache = self._get_metadata_cache()
        model_needle = model_filter.lower().strip() if model_filter else None
        session_needle = session_filter.lower().strip() if session_filter else None

        try:
            conv_dirs = sorted(brain_dir.iterdir(), key=lambda p: p.name)
        except OSError as exc:
            warnings.append(f"Failed to list {brain_dir}: {exc}")
            return ScanReport(data_dir=self.data_dir, sessions=[], warnings=warnings)

        seen_keys: set[str] = set()

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

            if not _is_within_dir(transcript_path, self.data_dir):
                continue

            cache_key = str(transcript_path)
            seen_keys.add(cache_key)
            try:
                st = transcript_path.stat()
                mtime_ns, size = st.st_mtime_ns, st.st_size
            except OSError as exc:
                warnings.append(f"Could not stat {transcript_path}: {exc}")
                continue

            cached = self._transcript_cache.get(cache_key)
            if cached is not None and cached[0] == mtime_ns and cached[1] == size:
                raw_turns, first_user_preview, file_skipped = cached[2], cached[3], cached[4]
                files_cached += 1
            else:
                try:
                    raw_turns, first_user_preview, file_skipped = _parse_transcript_file(
                        transcript_path, cid
                    )
                except OSError as exc:
                    warnings.append(f"Could not read {transcript_path}: {exc}")
                    continue
                self._transcript_cache[cache_key] = (
                    mtime_ns,
                    size,
                    raw_turns,
                    first_user_preview,
                    file_skipped,
                )
                files_reparsed += 1

            skipped_lines += file_skipped

            filtered_turns: list[TurnRecord] = []
            for turn in raw_turns:
                local_ts = turn.timestamp.astimezone(tz) if turn.timestamp is not None else None
                if since is not None:
                    if local_ts is None or local_ts.date() < since:
                        continue
                if until is not None:
                    if local_ts is None or local_ts.date() > until:
                        continue
                if model_needle and model_needle not in turn.model.lower():
                    continue
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
                    )
                )

            if not filtered_turns:
                skipped_empty += 1
                continue

            ann_title = self._get_annotation_title(cid)
            meta = meta_cache.get(cid, {})
            title = ann_title or meta.get("title") or meta.get("preview") or first_user_preview or "(untitled)"
            workspace = meta.get("workspace") or "(unknown)"

            session = SessionInfo(
                conversation_id=cid,
                title=title,
                workspace=workspace,
                transcript_path=cache_key,
            )
            for t in filtered_turns:
                session.add_turn(t)
            sessions.append(session)

        # Prune deleted transcript entries when scanning unfiltered sessions
        if not session_needle:
            for stale_key in list(self._transcript_cache.keys()):
                if stale_key not in seen_keys:
                    self._transcript_cache.pop(stale_key, None)

        if skipped_lines > 0:
            warnings.append(
                f"Skipped {skipped_lines} malformed/incomplete JSONL line(s) "
                "(normal if an agy CLI session is actively writing)."
            )

        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        return ScanReport(
            data_dir=self.data_dir,
            sessions=sessions,
            warnings=warnings,
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
) -> ScanReport:
    return IncrementalScanner(data_dir).scan(
        tz=tz,
        since=since,
        until=until,
        model_filter=model_filter,
        session_filter=session_filter,
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

    for s in sessions:
        total.merge(s.counts)
        by_workspace[s.workspace].merge(s.counts)
        workspace_sessions[s.workspace] += 1
        for t in s.turns:
            by_model[t.model].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens
            )
            if t.timestamp is not None:
                day_key = t.timestamp.strftime("%Y-%m-%d")
                week_key = iso_week_key(t.timestamp.date())
                month_key = t.timestamp.strftime("%Y-%m")
                by_hour[t.timestamp.hour].add_call(
                    t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens
                )
                week_days[week_key].add(day_key)
            else:
                day_key = "unknown"
                week_key = "unknown"
                month_key = "unknown"
            by_day[day_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens
            )
            by_week[week_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens
            )
            by_month[month_key].add_call(
                t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens
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
    }


_aggregate = aggregate_sessions


def _sort_sessions(sessions: Sequence[SessionInfo], sort_by: str) -> list[SessionInfo]:
    min_dt = dt.datetime.min.replace(tzinfo=dt.timezone.utc)
    if sort_by == "total":
        return sorted(sessions, key=lambda s: s.counts.total_tokens, reverse=True)
    if sort_by == "output":
        return sorted(sessions, key=lambda s: s.counts.output_tokens, reverse=True)
    if sort_by == "input":
        return sorted(sessions, key=lambda s: s.counts.input_tokens, reverse=True)
    if sort_by == "calls":
        return sorted(sessions, key=lambda s: s.counts.calls, reverse=True)
    if sort_by == "peak":
        return sorted(sessions, key=lambda s: s.counts.peak_context, reverse=True)
    return sorted(sessions, key=lambda s: s.last_time or min_dt, reverse=True)


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
    headers.extend(["Output", "Total", "Hit%", "PeakCtx"])
    aligns.extend(["right", "right", "right", "right"])
    return headers, aligns


# ---------- Output formatters ----------


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
) -> str:
    total: TokenCounts = agg["total"]
    include_cw = total.cache_write_tokens > 0
    out: list[str] = []

    header = _style("Antigravity CLI (agy) Token Usage [Read-Only]", "1;36", color)
    out.append(header)
    out.append(f"Data Dir : {report.data_dir}")

    filters: list[str] = [f"TZ={tz_label}"]
    if since or until:
        filters.append(f"Date={since or '*'}..{until or '*'}")
    if model_filter:
        filters.append(f"Model~={model_filter}")
    if session_filter:
        filters.append(f"Session={session_filter}*")
    out.append(f"Scope    : {', '.join(filters)}")
    out.append("")

    if not report.sessions:
        out.append("No matching token usage records found.")
        return "\n".join(out)

    # Overview block
    out.append(_style("Overview", "1;33", color))
    overview_lines = [
        f"  Sessions         : {len(report.sessions)} (LLM calls: {total.calls:,})",
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
        ]
    )
    out.extend(overview_lines)

    # Single-session detailed turn-by-turn view
    if session_filter and len(report.sessions) == 1:
        s = report.sessions[0]
        out.append("")
        out.append(_style(f"Session Detail: {s.conversation_id}", "1;33", color))
        out.append(f"  Title     : {s.title}")
        out.append(f"  Workspace : {s.workspace}")
        out.append(f"  Range     : {_fmt_time_sec(s.first_time)} -> {_fmt_time_sec(s.last_time)}")
        out.append("")
        t_headers = ["Step", "Time", "Model", "Input(new)", "CacheRead"]
        t_aligns = ["right", "left", "left", "right", "right"]
        if include_cw:
            t_headers.append("CacheWrite")
            t_aligns.append("right")
        t_headers.extend(["Output", "CtxSize", "Hit%"])
        t_aligns.extend(["right", "right", "right"])
        t_rows = []
        for t in s.turns:
            r = [
                str(t.step_index) if t.step_index >= 0 else "-",
                _fmt_time_sec(t.timestamp),
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

    if show_model:
        out.append("")
        out.append(_style("By Model", "1;33", color))
        headers, aligns = _counts_headers("Model", include_cw)
        items = sorted(
            agg["by_model"].items(),
            key=lambda kv: kv[1].total_tokens,
            reverse=True,
        )
        rows = [_format_counts_row(k, v, human, include_cw) for k, v in items]
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
        headers = ["Workspace", "Sessions", "Calls", "Input(new)", "CacheRead", "Output", "Total", "Hit%"]
        aligns = ["left", "right", "right", "right", "right", "right", "right", "right"]
        items = sorted(
            agg["by_workspace"].items(),
            key=lambda kv: kv[1].total_tokens,
            reverse=True,
        )
        rows = [
            [
                ws,
                str(agg["workspace_sessions"][ws]),
                str(c.calls),
                _fmt_num(c.input_tokens, human),
                _fmt_num(c.cache_read_tokens, human),
                _fmt_num(c.output_tokens, human),
                _fmt_num(c.total_tokens, human),
                _fmt_pct(c.cache_hit_rate),
            ]
            for ws, c in items
        ]
        out.append(_render_table(headers, rows, aligns, max_widths=[44, None, None, None, None, None, None, None]))

    if show_sessions:
        ordered = _sort_sessions(report.sessions, sort_by)
        total_cnt = len(ordered)
        shown = ordered if (view == "session" or limit <= 0) else ordered[:limit]
        out.append("")
        title_suffix = f" (showing {len(shown)}/{total_cnt}, sort={sort_by})" if len(shown) < total_cnt else f" ({total_cnt}, sort={sort_by})"
        out.append(_style(f"Sessions{title_suffix}", "1;33", color))
        s_headers = ["ID", "Last Active", "Calls", "Model", "Input(new)", "CacheRead", "Output", "Total", "PeakCtx", "Title"]
        s_aligns = ["left", "left", "right", "left", "right", "right", "right", "right", "right", "left"]
        s_rows = []
        for s in shown:
            models_str = ",".join(sorted(s.models)) if len(s.models) <= 1 else f"{sorted(s.models)[0]}+{len(s.models)-1}"
            s_rows.append(
                [
                    s.conversation_id[:8],
                    _fmt_time(s.last_time),
                    str(s.counts.calls),
                    models_str,
                    _fmt_num(s.counts.input_tokens, human),
                    _fmt_num(s.counts.cache_read_tokens, human),
                    _fmt_num(s.counts.output_tokens, human),
                    _fmt_num(s.counts.total_tokens, human),
                    _fmt_num(s.counts.peak_context, human),
                    s.title,
                ]
            )
        out.append(
            _render_table(
                s_headers,
                s_rows,
                s_aligns,
                max_widths=[8, 16, None, 22, None, None, None, None, None, 34],
            )
        )

    return "\n".join(out)


def render_json(
    report: ScanReport,
    agg: dict[str, Any],
    *,
    sort_by: str,
    tz_label: str,
    include_turns: bool,
) -> str:
    ordered = _sort_sessions(report.sessions, sort_by)
    sessions_json = []
    for s in ordered:
        entry: dict[str, Any] = {
            "conversation_id": s.conversation_id,
            "title": s.title,
            "workspace": s.workspace,
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
                    "total_input_tokens": t.total_input_tokens,
                    "output_tokens": t.output_tokens,
                    "total_tokens": t.total_tokens,
                    "cache_hit_rate_pct": round(t.cache_hit_rate, 2),
                }
                for t in s.turns
            ]
        sessions_json.append(entry)

    payload = {
        "data_dir": str(report.data_dir),
        "timezone": tz_label,
        "session_count": len(report.sessions),
        "overview": agg["total"].to_dict(),
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
        "by_workspace": {
            k: {"sessions": agg["workspace_sessions"][k], **v.to_dict()}
            for k, v in sorted(agg["by_workspace"].items())
        },
        "sessions": sessions_json,
        "warnings": report.warnings,
    }
    return json.dumps(payload, ensure_ascii=False, indent=2)


def render_csv(report: ScanReport, agg: dict[str, Any], *, view: str, sort_by: str) -> str:
    buf = io.StringIO()
    writer = csv.writer(buf, lineterminator="\n")

    if view == "model":
        writer.writerow(["model", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context"])
        for k, c in sorted(agg["by_model"].items(), key=lambda kv: kv[1].total_tokens, reverse=True):
            writer.writerow([k, c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context])
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
        writer.writerow([key_col, "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context"])
        for k, c in items:
            writer.writerow([k, c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context])
    elif view == "workspace":
        writer.writerow(["workspace", "sessions", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context"])
        for k, c in sorted(agg["by_workspace"].items(), key=lambda kv: kv[1].total_tokens, reverse=True):
            writer.writerow([k, agg["workspace_sessions"][k], c.calls, c.input_tokens, c.cache_read_tokens, c.cache_write_tokens, c.total_input_tokens, c.output_tokens, c.total_tokens, c.non_cached_tokens, round(c.cache_hit_rate, 2), c.peak_context])
    else:
        writer.writerow(["conversation_id", "first_time", "last_time", "models", "calls", "input_tokens", "cache_read_tokens", "cache_write_tokens", "total_input_tokens", "output_tokens", "total_tokens", "non_cached_tokens", "cache_hit_rate_pct", "peak_context", "workspace", "title"])
        for s in _sort_sessions(report.sessions, sort_by):
            c = s.counts
            writer.writerow([
                s.conversation_id,
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
                s.workspace,
                s.title,
            ])
    return buf.getvalue().rstrip("\n")


# ---------- CLI ----------


def _parse_date_arg(value: str) -> dt.date:
    try:
        return dt.date.fromisoformat(value.strip())
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"Invalid date '{value}', expected YYYY-MM-DD.") from exc


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="agy_token_stats.py",
        description="Strictly read-only token usage analyzer for Antigravity CLI (agy).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Examples:\n"
            "  python agy_token_stats.py                     # Overview + model + daily + recent sessions\n"
            "  python agy_token_stats.py --today             # Only today's usage (local time)\n"
            "  python agy_token_stats.py --days 7 --human    # Last 7 days in K/M format\n"
            "  python agy_token_stats.py --session e54a0f44  # Turn-by-turn breakdown for one session\n"
            "  python agy_token_stats.py --view all          # Include monthly & workspace tables\n"
            "  python agy_token_stats.py --format json       # Export structured JSON to stdout\n"
        ),
    )
    parser.add_argument(
        "--data-dir",
        type=pathlib.Path,
        default=DEFAULT_DATA_DIR,
        help=f"Antigravity CLI data directory (default: {DEFAULT_DATA_DIR})",
    )
    parser.add_argument(
        "--view",
        choices=("summary", "all", "daily", "weekly", "monthly", "hourly", "model", "session", "workspace"),
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
        "--sort",
        choices=("time", "total", "output", "input", "calls", "peak"),
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

    report = scan_usage(
        data_dir=args.data_dir.expanduser(),
        tz=tz,
        since=since,
        until=until,
        model_filter=args.model,
        session_filter=args.session,
    )
    agg = _aggregate(report.sessions)

    if args.format == "json":
        include_turns = bool(args.session)
        print(
            render_json(
                report,
                agg,
                sort_by=args.sort,
                tz_label=tz_label,
                include_turns=include_turns,
            )
        )
    elif args.format == "csv":
        print(render_csv(report, agg, view=args.view, sort_by=args.sort))
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
            )
        )

    for warn in report.warnings:
        print(f"Note: {warn}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
