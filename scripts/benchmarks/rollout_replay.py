#!/usr/bin/env python3
"""Effective-history replay for Codex rollout files.

Shared library for density/hygiene analysis of managed-context benchmark
artifacts. Replays a `rollout-*.jsonl` stream into the *effective* model
history at every model request boundary, mirroring the lineage fork's
`effective_response_items_in_rollout` semantics
(codex-rs/core/src/thread_rollout_truncation.rs) and Intendant's catalog
replay (src/bin/caller/main.rs `scan_context_rewind_anchor_catalog`):

- `response_item` lines append to history.
- `compacted` lines replace history with `payload.replacement_history`
  (or, legacy, with collected user messages + the summary message).
- `event_msg`/`thread_rolled_back` lines cut history. An anchor
  (`anchor.itemId` + `anchor.position`) is resolved against the *current
  effective* history: `before` cuts at the first item matching the anchor
  id, `after` keeps the whole matching call/output group (cut after the
  last matching item). If the anchor does not resolve, fall back to
  dropping the last `num_turns` user turns.
- `event_msg`/`token_count` lines mark model-request boundaries; each one
  snapshots the live history.

Token estimation uses tiktoken `o200k_base` when available, with a
chars/4 fallback (flagged in the output). Reasoning items are special:
their text channels are usually empty (`encrypted_content` only), so when
a request reports `reasoning_output_tokens`, that amount is spread over
the reasoning items the model emitted in that response.

Known approximations (documented for downstream consumers):
- The fork's `trim_pre_turn_context_updates` (contextual fragments glued
  to a turn boundary) is not replayed; num_turns cuts may keep a few small
  contextual items the fork would drop.
- User-turn boundaries treat any user message whose first content text
  starts with a known contextual tag as non-boundary; the fork resolves
  this through registered fragment parsers.
- The system prompt + tool definitions are not persisted in rollouts
  (session_meta.base_instructions is empty in real artifacts), so absolute
  estimates carry an unobservable constant offset. Calibration must be
  done with a fitted offset (see managed_density_report.py).
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

# ---------------------------------------------------------------------------
# Token estimation
# ---------------------------------------------------------------------------


class TokenEstimator:
    """tiktoken o200k_base estimator with a chars/4 fallback."""

    def __init__(self) -> None:
        self.mode = "chars4"
        self._encoding = None
        try:
            import tiktoken  # type: ignore

            self._encoding = tiktoken.get_encoding("o200k_base")
            self.mode = "tiktoken-o200k_base"
        except Exception:
            self._encoding = None

    def estimate_text(self, text: str) -> int:
        if not text:
            return 0
        if self._encoding is not None:
            try:
                return len(self._encoding.encode(text, disallowed_special=()))
            except Exception:
                pass
        return max(1, (len(text) + 3) // 4)

    def estimate_item(self, payload: dict[str, Any]) -> int:
        """Estimate the prompt-token weight of one response item.

        Counts the human-readable text channels with the tokenizer; opaque
        blobs (`encrypted_content`) are counted at chars/4 since their true
        token cost is invisible. Reasoning items frequently end up with a
        near-zero estimate here and are re-estimated from
        `reasoning_output_tokens` during replay.
        """
        total = self.estimate_text(item_text(payload))
        encrypted = payload.get("encrypted_content")
        if isinstance(encrypted, str) and encrypted:
            total += max(1, len(encrypted) // 4)
        # Small per-item structural overhead (role/type/framing).
        return total + 4


# ---------------------------------------------------------------------------
# Response-item helpers (mirror src/bin/caller/main.rs + the fork)
# ---------------------------------------------------------------------------

MODEL_EMITTED_TYPES = {
    "reasoning",
    "function_call",
    "local_shell_call",
    "custom_tool_call",
    "tool_search_call",
    "web_search_call",
    "image_generation_call",
}

OUTPUT_ITEM_TYPES = {
    "function_call_output",
    "custom_tool_call_output",
    "tool_search_output",
    "local_shell_call_output",
}

# User messages whose first text fragment opens with one of these tags are
# contextual fragments, not real user turns (fork:
# core/src/context/contextual_user_message.rs + event_mapping.rs).
CONTEXTUAL_USER_TAGS = (
    "<user_instructions>",
    "<environment_context>",
    "<skill_instructions>",
    "<user_shell_command>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<goal_context>",
    "<permissions instructions>",
    "<model_switch>",
    "<collaboration_mode>",
    "<realtime_conversation>",
    "<personality_spec>",
    "<hook_prompt",
    "<unified_exec_process_limit_warning>",
    "<apply_patch_exec_command_warning>",
    "<model_mismatch_warning>",
)

PRIMER_MARKERS = (
    "<model_context_rewind_primer>",
    "<managed_context_recovery>",
)

HYGIENE_TOOL_NAMES = {
    "get_status",
    "list_rewind_anchors",
    "inspect_rewind_anchor",
    "rewind_context",
    "rewind_backout",
    "context_rewind_anchors",
    "context_rewind_anchor_inspect",
    "context_rewind",
    "context_rewind_backout",
}


def load_rollout_lines(path: Path) -> list[dict[str, Any]]:
    lines: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for raw in handle:
            raw = raw.strip()
            if not raw:
                continue
            try:
                value = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                lines.append(value)
    return lines


def item_text(payload: dict[str, Any]) -> str:
    """Concatenated text channels of a response item (for estimation and
    identifier extraction)."""
    kind = payload.get("type")
    parts: list[str] = []
    if kind == "message" or kind == "compaction":
        content = payload.get("content")
        if isinstance(content, list):
            for fragment in content:
                if isinstance(fragment, dict):
                    text = (
                        fragment.get("text")
                        or fragment.get("input_text")
                        or fragment.get("output_text")
                    )
                    if isinstance(text, str):
                        parts.append(text)
        elif isinstance(content, str):
            parts.append(content)
        # Legacy compaction items may carry the summary under "message".
        message = payload.get("message")
        if isinstance(message, str):
            parts.append(message)
    elif kind == "reasoning":
        summary = payload.get("summary")
        if isinstance(summary, list):
            for fragment in summary:
                if isinstance(fragment, dict):
                    text = fragment.get("text")
                    if isinstance(text, str):
                        parts.append(text)
        content = payload.get("content")
        if isinstance(content, list):
            for fragment in content:
                if isinstance(fragment, dict):
                    text = fragment.get("text")
                    if isinstance(text, str):
                        parts.append(text)
    elif kind in ("function_call", "custom_tool_call", "tool_search_call"):
        name = payload.get("name")
        if isinstance(name, str):
            parts.append(name)
        arguments = payload.get("arguments") or payload.get("input")
        if isinstance(arguments, str):
            parts.append(arguments)
        elif arguments is not None:
            parts.append(json.dumps(arguments))
    elif kind in OUTPUT_ITEM_TYPES:
        output = payload.get("output")
        if isinstance(output, str):
            parts.append(output)
        elif isinstance(output, dict):
            # Some shapes nest {"content": ..., "success": ...}.
            inner = output.get("content") or output.get("output")
            if isinstance(inner, str):
                parts.append(inner)
            else:
                parts.append(json.dumps(output))
        elif output is not None:
            parts.append(json.dumps(output))
    elif kind == "local_shell_call":
        action = payload.get("action")
        if action is not None:
            parts.append(json.dumps(action))
    else:
        # Unknown item type: serialize everything except obvious id noise.
        scrubbed = {
            key: value
            for key, value in payload.items()
            if key not in ("id", "call_id", "callId", "type", "status")
        }
        if scrubbed:
            parts.append(json.dumps(scrubbed))
    return "\n".join(parts)


def anchor_ids_for_item(payload: dict[str, Any]) -> list[str]:
    """Anchor ids a `thread_rolled_back` marker can resolve against
    (mirrors `response_item_anchor_ids` in src/bin/caller/main.rs)."""
    kind = payload.get("type")
    keys: tuple[str, ...]
    if kind in ("message", "reasoning", "web_search_call", "image_generation_call"):
        keys = ("id",)
    elif kind in ("local_shell_call", "function_call", "tool_search_call", "custom_tool_call"):
        keys = ("id", "call_id", "callId")
    elif kind in ("function_call_output", "tool_search_output", "custom_tool_call_output"):
        keys = ("call_id", "callId")
    else:
        keys = ()
    ids: list[str] = []
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str):
            value = value.strip()
            if value and value not in ids:
                ids.append(value)
    return ids


def message_role(payload: dict[str, Any]) -> str | None:
    role = payload.get("role")
    return role if isinstance(role, str) else None


def first_content_text(payload: dict[str, Any]) -> str:
    content = payload.get("content")
    if isinstance(content, list):
        for fragment in content:
            if isinstance(fragment, dict):
                text = (
                    fragment.get("text")
                    or fragment.get("input_text")
                    or fragment.get("output_text")
                )
                if isinstance(text, str):
                    return text
    return ""


def is_contextual_user_message(payload: dict[str, Any]) -> bool:
    if payload.get("type") != "message" or message_role(payload) != "user":
        return False
    text = first_content_text(payload).lstrip()
    lowered = text.lower()
    return any(lowered.startswith(tag) for tag in CONTEXTUAL_USER_TAGS)


def is_user_turn_boundary(payload: dict[str, Any]) -> bool:
    """User-turn boundary for num_turns rollbacks (fork:
    `is_user_turn_boundary`). Assistant inter-agent envelopes also count in
    the fork; they do not occur in these benchmark artifacts."""
    if payload.get("type") != "message" or message_role(payload) != "user":
        return False
    return not is_contextual_user_message(payload)


def is_primer_message(payload: dict[str, Any]) -> bool:
    if payload.get("type") != "message":
        return False
    text = item_text(payload)
    return any(marker in text for marker in PRIMER_MARKERS)


def is_model_emitted(payload: dict[str, Any]) -> bool:
    kind = payload.get("type")
    if kind == "message":
        role = message_role(payload)
        return role is not None and role.lower() == "assistant"
    return kind in MODEL_EMITTED_TYPES


def parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    text = value.strip()
    try:
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        # Python's fromisoformat only takes up to 6 fractional digits.
        match = re.match(r"^([^.]+)\.(\d+)(\+.*|-\d\d:\d\d)?$", text)
        if match and len(match.group(2)) > 6:
            text = f"{match.group(1)}.{match.group(2)[:6]}{match.group(3) or ''}"
        parsed = datetime.fromisoformat(text)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed
    except ValueError:
        return None


# ---------------------------------------------------------------------------
# Identifier extraction (staleness / fact matching)
# ---------------------------------------------------------------------------

_PATH_RE = re.compile(r"(?:/[\w.+\-]+){2,}|\b[\w\-]+\.(?:py|rs|js|ts|tsx|c|h|cpp|hpp|cc|json|toml|ya?ml|txt|md|sh|wad|bmp|csv|stan|pyx|so|o|a|log|html|css|sql|gcov|cfg|ini|lock)\b")
_TOKEN_RE = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]{5,}\b")
_ERROR_LINE_RE = re.compile(r"(?i)\b(error|exception|panic|traceback|fail(?:ed|ure)?)\b")

# Common words that would otherwise flood the identifier sets.
_STOPWORDS = frozenset(
    """should would could against between because through before after
    without within running return returns returned import imports
    package module function method object string number value values
    result results output outputs input inputs command commands process
    system version available default options option please change
    changes update updated create created delete deleted remove removed
    contains containing content contents message messages warning
    warnings second seconds minute minutes python cargo target release
    debug build builds building install installed installing directory
    directories exists existing exited status stdout stderr stdin
    expected actual assert assertion verify verified verifying complete
    completed include included including provide provided requires
    required requirement license copyright general public differ
    different argument arguments parameter parameters variable variables
    request requests response responses session sessions current latest
    """.split()
)


def extract_identifiers(text: str, cap: int = 400) -> set[str]:
    """Distinctive identifiers of a blob of text: file paths, error-line
    tokens, and >=6-char symbols, lowercased, minus stopwords."""
    if not text:
        return set()
    identifiers: set[str] = set()
    for match in _PATH_RE.finditer(text):
        identifiers.add(match.group(0).lower())
        if len(identifiers) >= cap:
            return identifiers
    for line in text.splitlines():
        if _ERROR_LINE_RE.search(line):
            for token in _TOKEN_RE.findall(line):
                lowered = token.lower()
                if lowered not in _STOPWORDS:
                    identifiers.add(lowered)
            if len(identifiers) >= cap:
                return identifiers
    for token in _TOKEN_RE.findall(text):
        lowered = token.lower()
        if lowered not in _STOPWORDS:
            identifiers.add(lowered)
            if len(identifiers) >= cap:
                break
    return identifiers


# ---------------------------------------------------------------------------
# Replay
# ---------------------------------------------------------------------------


@dataclass
class HistoryItem:
    uid: int
    line_no: int  # 1-based rollout line; -1 for synthetic (replacement_history)
    payload: dict[str, Any]
    kind: str
    est_tokens: int
    birth_request: int  # number of token_count events seen before this item
    is_model: bool
    timestamp: datetime | None = None
    synthetic: bool = False
    removed_at_request: int | None = None
    removed_at_line: int | None = None
    removal_cause: str | None = None  # "rewind_anchor" | "rewind_num_turns" | "compaction"
    # Lazily computed caches:
    _identifiers: set[str] | None = field(default=None, repr=False)
    _text: str | None = field(default=None, repr=False)

    @property
    def name(self) -> str | None:
        name = self.payload.get("name")
        return name if isinstance(name, str) else None

    @property
    def role(self) -> str | None:
        return message_role(self.payload)

    def text(self) -> str:
        if self._text is None:
            self._text = item_text(self.payload)
        return self._text

    def identifiers(self) -> set[str]:
        if self._identifiers is None:
            self._identifiers = extract_identifiers(self.text())
        return self._identifiers


@dataclass
class RequestSnapshot:
    """State at one model-request boundary (a `token_count` event).

    `token_count` events with all-zero usage (the fork resets token info
    right after a rollback) are *not* requests and never produce a
    snapshot.
    """

    index: int  # 0-based request index
    line_no: int
    timestamp: datetime | None
    input_tokens: int | None
    cached_input_tokens: int | None
    output_tokens: int | None
    reasoning_output_tokens: int | None
    total_tokens: int | None
    context_window: int | None
    hard_context_window: int | None
    history_uids: list[int]
    # Items strictly before the current model response's first item: this is
    # what `input_tokens` actually measured (see main.rs
    # `context_rewind_usage_covers_anchor`).
    input_side_uids: list[int]
    # Rollout line of the most recent turn boundary (`task_started` event or
    # persisted non-contextual user message) at this request. Reasoning items
    # persisted before this line belong to completed turns and are dropped
    # from the prompt codex actually sends (the server-billed view; compare
    # the fork's `get_non_last_reasoning_items_tokens`).
    last_turn_start_line: int = 0


@dataclass
class RollbackEvent:
    line_no: int
    timestamp: datetime | None
    request_index: int
    num_turns: int
    anchor_item_id: str | None
    anchor_position: str | None
    resolved_by: str  # "anchor" | "num_turns" | "noop"
    removed_uids: list[int]


@dataclass
class CompactionEvent:
    line_no: int
    timestamp: datetime | None
    request_index: int
    replacement_len: int | None  # None = legacy summary bridge
    removed_uids: list[int]


@dataclass
class RolloutReplay:
    path: Path
    session_meta: dict[str, Any] | None
    items: dict[int, HistoryItem]
    requests: list[RequestSnapshot]
    rollbacks: list[RollbackEvent]
    compactions: list[CompactionEvent]
    final_history_uids: list[int]
    call_name_by_call_id: dict[str, str]
    estimator_mode: str

    def item(self, uid: int) -> HistoryItem:
        return self.items[uid]

    def live_items(self, snapshot: RequestSnapshot, input_side: bool = False) -> list[HistoryItem]:
        uids = snapshot.input_side_uids if input_side else snapshot.history_uids
        return [self.items[uid] for uid in uids]

    def billed_input_uids(self, snapshot: RequestSnapshot) -> list[int]:
        """The server-billed prompt view at this request: the input-side
        effective history minus reasoning items persisted before the last
        turn boundary. Codex re-sends encrypted reasoning only within the
        active turn; completed turns' reasoning is dropped from the prompt
        (verified against May-27 single-turn rollouts, where keeping all
        reasoning calibrates to ~1.000, and June-12 multi-turn rollouts,
        where it over-counts by exactly the prior turns' reasoning)."""
        boundary = snapshot.last_turn_start_line
        return [
            uid
            for uid in snapshot.input_side_uids
            if not (
                self.items[uid].kind == "reasoning"
                and 0 <= self.items[uid].line_no < boundary
            )
        ]


def _token_count_fields(payload: dict[str, Any]) -> dict[str, int | None] | None:
    if payload.get("type") != "token_count":
        return None
    info = payload.get("info")
    if not isinstance(info, dict):
        return None  # heartbeat token_count without usage info
    last = info.get("last_token_usage") or info.get("lastTokenUsage")
    if not isinstance(last, dict):
        return None

    def grab(source: dict[str, Any], *keys: str) -> int | None:
        for key in keys:
            value = source.get(key)
            if isinstance(value, int):
                return value
        return None

    return {
        "input_tokens": grab(last, "input_tokens", "inputTokens"),
        "cached_input_tokens": grab(last, "cached_input_tokens", "cachedInputTokens"),
        "output_tokens": grab(last, "output_tokens", "outputTokens"),
        "reasoning_output_tokens": grab(last, "reasoning_output_tokens", "reasoningOutputTokens"),
        "total_tokens": grab(last, "total_tokens", "totalTokens"),
        "context_window": grab(info, "model_context_window", "modelContextWindow"),
        "hard_context_window": grab(info, "model_hard_context_window", "modelHardContextWindow"),
    }


def _rollback_fields(payload: dict[str, Any]) -> tuple[int, str | None, str | None] | None:
    if payload.get("type") != "thread_rolled_back":
        return None
    num_turns = payload.get("num_turns")
    num_turns = num_turns if isinstance(num_turns, int) else 0
    anchor = payload.get("anchor")
    item_id = position = None
    if isinstance(anchor, dict):
        raw_id = anchor.get("itemId") or anchor.get("item_id")
        if isinstance(raw_id, str) and raw_id.strip():
            item_id = raw_id.strip()
        raw_pos = anchor.get("position")
        if isinstance(raw_pos, str):
            position = raw_pos.strip().lower()
    return num_turns, item_id, position


def replay_rollout(
    path: Path,
    estimator: TokenEstimator | None = None,
) -> RolloutReplay:
    """Replay one rollout file into per-request effective-history snapshots."""
    estimator = estimator or TokenEstimator()
    lines = load_rollout_lines(path)

    items: dict[int, HistoryItem] = {}
    history: list[int] = []  # live uids, in order
    requests: list[RequestSnapshot] = []
    rollbacks: list[RollbackEvent] = []
    compactions: list[CompactionEvent] = []
    call_name_by_call_id: dict[str, str] = {}
    session_meta: dict[str, Any] | None = None

    next_uid = 0
    request_counter = 0
    # Track where the current model response began (uid of the first
    # model-emitted item of the current run), mirroring main.rs.
    current_response_start_uid: int | None = None
    previous_item_was_model = False
    # Reasoning items of the current run awaiting reasoning_output_tokens.
    current_run_reasoning_uids: list[int] = []
    # Most recent turn boundary: a `task_started` event or a persisted
    # non-contextual user message (recovery turns start without one).
    last_turn_start_line = 0

    def add_item(
        payload: dict[str, Any],
        line_no: int,
        synthetic: bool = False,
        timestamp: datetime | None = None,
    ) -> HistoryItem:
        nonlocal next_uid, current_response_start_uid, previous_item_was_model
        kind = str(payload.get("type") or "unknown")
        model = is_model_emitted(payload)
        item = HistoryItem(
            uid=next_uid,
            line_no=line_no,
            payload=payload,
            kind=kind,
            est_tokens=estimator.estimate_item(payload),
            birth_request=request_counter,
            is_model=model,
            timestamp=timestamp,
            synthetic=synthetic,
        )
        next_uid += 1
        items[item.uid] = item
        history.append(item.uid)
        if kind in ("function_call", "custom_tool_call", "local_shell_call", "tool_search_call"):
            for key in ("call_id", "callId"):
                call_id = payload.get(key)
                if isinstance(call_id, str) and call_id.strip() and item.name:
                    call_name_by_call_id[call_id.strip()] = item.name
        if not synthetic:
            if model and not previous_item_was_model:
                current_response_start_uid = item.uid
                current_run_reasoning_uids.clear()
            previous_item_was_model = model
            if model and kind == "reasoning":
                current_run_reasoning_uids.append(item.uid)
        return item

    def remove_uids(uids: Iterable[int], cause: str, line_no: int) -> list[int]:
        removed = []
        for uid in uids:
            entry = items[uid]
            entry.removed_at_request = request_counter
            entry.removed_at_line = line_no
            entry.removal_cause = cause
            removed.append(uid)
        return removed

    for line_index, line in enumerate(lines):
        line_no = line_index + 1
        line_type = line.get("type")
        timestamp = parse_timestamp(line.get("timestamp"))
        payload = line.get("payload")

        if line_type == "session_meta" and isinstance(payload, dict):
            session_meta = payload
            continue

        if line_type == "response_item" and isinstance(payload, dict):
            if is_user_turn_boundary(payload):
                last_turn_start_line = line_no
            add_item(payload, line_no, timestamp=timestamp)
            continue

        if line_type == "compacted" and isinstance(payload, dict):
            replacement = payload.get("replacement_history")
            removed = remove_uids(list(history), "compaction", line_no)
            history.clear()
            if isinstance(replacement, list):
                for entry in replacement:
                    if isinstance(entry, dict):
                        add_item(entry, -1, synthetic=True, timestamp=timestamp)
                replacement_len = len(replacement)
            else:
                # Legacy bridge: prior user messages + the summary message.
                replacement_len = None
                user_texts = [
                    items[uid].text()
                    for uid in removed
                    if is_user_turn_boundary(items[uid].payload)
                ]
                summary = payload.get("message")
                for text in user_texts:
                    add_item(
                        {
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": text}],
                        },
                        -1,
                        synthetic=True,
                        timestamp=timestamp,
                    )
                add_item(
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": summary if isinstance(summary, str) else "",
                            }
                        ],
                    },
                    -1,
                    synthetic=True,
                    timestamp=timestamp,
                )
            # A compaction resets the response-run tracking.
            current_response_start_uid = None
            previous_item_was_model = False
            current_run_reasoning_uids.clear()
            compactions.append(
                CompactionEvent(
                    line_no=line_no,
                    timestamp=timestamp,
                    request_index=request_counter,
                    replacement_len=replacement_len,
                    removed_uids=removed,
                )
            )
            continue

        if line_type != "event_msg" or not isinstance(payload, dict):
            continue

        rollback = _rollback_fields(payload)
        if rollback is not None:
            num_turns, anchor_id, anchor_position = rollback
            cut_index: int | None = None
            resolved_by = "noop"
            if anchor_id is not None:
                matches = [
                    idx
                    for idx, uid in enumerate(history)
                    if anchor_id in anchor_ids_for_item(items[uid].payload)
                ]
                if matches:
                    if anchor_position == "before":
                        cut_index = matches[0]
                    else:  # "after" (default, mirrors the fork)
                        cut_index = matches[-1] + 1
                    resolved_by = "anchor"
            if cut_index is None and num_turns > 0:
                user_positions = [
                    idx
                    for idx, uid in enumerate(history)
                    if is_user_turn_boundary(items[uid].payload)
                ]
                if user_positions:
                    if num_turns >= len(user_positions):
                        cut_index = user_positions[0]
                    else:
                        cut_index = user_positions[len(user_positions) - num_turns]
                    resolved_by = "num_turns"
            removed: list[int] = []
            if cut_index is not None:
                removed = remove_uids(history[cut_index:], f"rewind_{resolved_by}", line_no)
                del history[cut_index:]
                # The cut may have removed the current run; reset tracking.
                if current_response_start_uid is not None and current_response_start_uid in set(
                    removed
                ):
                    current_response_start_uid = None
                    previous_item_was_model = False
                    current_run_reasoning_uids.clear()
            rollbacks.append(
                RollbackEvent(
                    line_no=line_no,
                    timestamp=timestamp,
                    request_index=request_counter,
                    num_turns=num_turns,
                    anchor_item_id=anchor_id,
                    anchor_position=anchor_position,
                    resolved_by=resolved_by,
                    removed_uids=removed,
                )
            )
            continue

        if payload.get("type") == "task_started":
            last_turn_start_line = line_no
            continue

        usage = _token_count_fields(payload)
        if usage is not None:
            if not usage["input_tokens"]:
                # Post-rollback token-info reset: the fork zeroes
                # last_token_usage's input/output and re-estimates only
                # total_tokens for the surviving history. Not a billed
                # model request; never snapshot it.
                continue
            # Spread this response's reasoning tokens over its reasoning
            # items (their text channels are empty/encrypted).
            reasoning_tokens = usage.get("reasoning_output_tokens")
            live_run_reasoning = [
                uid for uid in current_run_reasoning_uids if items[uid].removed_at_request is None
            ]
            if (
                isinstance(reasoning_tokens, int)
                and reasoning_tokens > 0
                and live_run_reasoning
            ):
                share = max(1, reasoning_tokens // len(live_run_reasoning))
                for uid in live_run_reasoning:
                    items[uid].est_tokens = share + 4
            input_side_uids = list(history)
            if current_response_start_uid is not None:
                try:
                    boundary = history.index(current_response_start_uid)
                    input_side_uids = history[:boundary]
                except ValueError:
                    pass
            requests.append(
                RequestSnapshot(
                    index=request_counter,
                    line_no=line_no,
                    timestamp=timestamp,
                    input_tokens=usage["input_tokens"],
                    cached_input_tokens=usage["cached_input_tokens"],
                    output_tokens=usage["output_tokens"],
                    reasoning_output_tokens=usage["reasoning_output_tokens"],
                    total_tokens=usage["total_tokens"],
                    context_window=usage["context_window"],
                    hard_context_window=usage["hard_context_window"],
                    history_uids=list(history),
                    input_side_uids=input_side_uids,
                    last_turn_start_line=last_turn_start_line,
                )
            )
            request_counter += 1
            continue

    return RolloutReplay(
        path=path,
        session_meta=session_meta,
        items=items,
        requests=requests,
        rollbacks=rollbacks,
        compactions=compactions,
        final_history_uids=list(history),
        call_name_by_call_id=call_name_by_call_id,
        estimator_mode=estimator.mode,
    )


# ---------------------------------------------------------------------------
# Tool-call normalization (M3a)
# ---------------------------------------------------------------------------


def _canonicalize_value(value: Any) -> Any:
    if isinstance(value, str):
        return " ".join(value.split()).strip("\"' ")
    if isinstance(value, dict):
        return {key: _canonicalize_value(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [_canonicalize_value(entry) for entry in value]
    return value


def normalize_call(name: str | None, arguments: Any) -> str:
    """Canonical duplicate-detection key for a tool call: tool name plus
    whitespace/quote-normalized, key-sorted arguments."""
    label = name or "?"
    if isinstance(arguments, str):
        try:
            parsed = json.loads(arguments)
        except json.JSONDecodeError:
            parsed = arguments
    else:
        parsed = arguments
    canon = _canonicalize_value(parsed) if parsed is not None else ""
    return f"{label}::{json.dumps(canon, sort_keys=True, ensure_ascii=False)}"


# ---------------------------------------------------------------------------
# Self-test against the hand-constructed fixtures
# ---------------------------------------------------------------------------


def _self_test() -> None:
    """Verify H_i reconstruction against scripts/benchmarks/fixtures/.

    The fixtures are constructed so every effective-history snapshot is
    hand-checkable. Managed fixture uid map (in add order):
      u0 user turn-1 | u1 reasoning | u2 call_1 | u3 call_1 output (big noise)
      [req0] u4 reasoning | u5 assistant [req1] u6 user turn-2 | u7 reasoning
      | u8 call_2 | u9 call_2 output (big, magic_token_xyz123) | u10 assistant
      chatter [req2] -> anchor-after(call_2) rewind drops u10 -> u11 primer |
      u12 reasoning | u13 assistant [req3] -> num_turns=1 rollback drops the
      primer turn (u11..u13) -> u14 user turn-3 | u15 reasoning [req4] ->
      compacted(replacement_history: 2 items -> u16, u17) -> u18 reasoning |
      u19 assistant [req5] -> u20 call_3 (dup of call_1 modulo whitespace) |
      u21 output | u22 assistant [req6] -> anchor-before(call_3) rewind drops
      u20..u22 [req7].
    """
    fixtures = Path(__file__).resolve().parent / "fixtures" / "density_replay"
    managed = next(
        (fixtures / "managed-trial" / "agent" / "sessions").glob("**/rollout-*.jsonl")
    )
    replay = replay_rollout(managed)

    assert len(replay.requests) == 8, f"expected 8 requests, got {len(replay.requests)}"
    reqs = replay.requests
    assert reqs[0].history_uids == [0, 1, 2, 3]
    assert reqs[0].input_side_uids == [0]
    assert reqs[1].history_uids == [0, 1, 2, 3, 4, 5]
    assert reqs[1].input_side_uids == [0, 1, 2, 3]
    assert reqs[2].history_uids == list(range(11))
    assert reqs[2].input_side_uids == list(range(10))
    assert reqs[3].history_uids == list(range(10)) + [11, 12, 13]
    assert reqs[4].history_uids == list(range(10)) + [14, 15]
    assert reqs[5].history_uids == [16, 17, 18, 19]
    assert reqs[5].input_side_uids == [16, 17]
    assert reqs[6].history_uids == [16, 17, 18, 19, 20, 21, 22]
    assert reqs[7].history_uids == [16, 17, 18, 19]
    # Run-start tracking resets when a rewind removes the current run.
    assert reqs[7].input_side_uids == [16, 17, 18, 19]

    assert [r.resolved_by for r in replay.rollbacks] == ["anchor", "num_turns", "anchor"]
    assert replay.rollbacks[0].removed_uids == [10]
    assert replay.rollbacks[1].removed_uids == [11, 12, 13]
    assert replay.rollbacks[2].removed_uids == [20, 21, 22]
    assert len(replay.compactions) == 1
    assert replay.compactions[0].replacement_len == 2
    assert sorted(replay.compactions[0].removed_uids) == list(range(10)) + [14, 15]
    assert replay.items[10].removal_cause == "rewind_anchor"
    assert replay.items[11].removal_cause == "rewind_num_turns"
    assert replay.items[0].removal_cause == "compaction"
    assert replay.items[16].synthetic and replay.items[17].synthetic

    # Reasoning items get re-estimated from reasoning_output_tokens
    # (share + 4 framing tokens); u7's run never reached a token_count
    # before being broken, so it keeps its encrypted-content estimate.
    assert replay.items[1].est_tokens == 104, replay.items[1].est_tokens
    assert replay.items[4].est_tokens == 44, replay.items[4].est_tokens
    assert replay.items[7].est_tokens == 104, replay.items[7].est_tokens
    # The big outputs clear the 500-token noise threshold under both
    # estimator modes.
    assert replay.items[3].est_tokens > 500
    assert replay.items[9].est_tokens > 500

    # Pressure-band inputs surfaced per request.
    assert reqs[2].total_tokens == 9000
    assert reqs[2].context_window == 10000
    assert reqs[2].hard_context_window == 12000

    # Duplicate-call normalization: whitespace-only differences collapse.
    call_1 = replay.items[2]
    call_3 = replay.items[20]
    assert normalize_call(call_1.name, call_1.payload.get("arguments")) == normalize_call(
        call_3.name, call_3.payload.get("arguments")
    )

    # Vanilla fixture: legacy compaction bridge + aging.
    vanilla = next(
        (fixtures / "vanilla-trial" / "agent" / "sessions").glob("**/rollout-*.jsonl")
    )
    vreplay = replay_rollout(vanilla)
    assert len(vreplay.requests) == 8
    assert vreplay.requests[6].history_uids == list(range(15))
    assert len(vreplay.compactions) == 1
    assert vreplay.compactions[0].replacement_len is None  # legacy bridge
    # Bridge = 1 surviving user message + 1 summary message.
    assert vreplay.requests[7].history_uids == [15, 16, 17, 18, 19]
    assert vreplay.items[15].synthetic and vreplay.items[16].synthetic
    big_output = vreplay.items[2]
    assert big_output.kind == "function_call_output"
    assert big_output.birth_request == 0
    assert big_output.removed_at_request == 7 and big_output.removal_cause == "compaction"

    print(f"rollout_replay self-test OK (estimator: {replay.estimator_mode})")


if __name__ == "__main__":
    import sys

    if "--self-test" in sys.argv:
        _self_test()
    else:
        print(__doc__)
        print("Run with --self-test to verify against the committed fixtures.")
