"""Vibeke plugin for Hermes Agent.

Installed by `vibeke integration install hermes` to <HERMES_HOME>/plugins/vibeke/ and enabled in
config.yaml (plugins.enabled). Inert outside a Vibeke pane: it needs VIBEKE=1, VIBEKE_SOCKET and
VIBEKE_PANE_TOKEN.

Every forwarded hook runs `$VIBEKE_BIN hook hermes <hook>` with the hook's keyword arguments as
JSON on stdin, the same shim the Claude, Codex and OpenCode integrations use. A single worker
thread sends them in order, so a slow server never blocks the agent loop. The server maps the
hooks in crates/vk-server/src/agents/hermes.rs. Hermes approvals are observe-only for plugins, so
Vibeke answers them with keystrokes in the pane.
"""

from __future__ import annotations

import atexit
import json
import os
import queue
import subprocess
import threading

_BIN = os.environ.get("VIBEKE_BIN") or "vibeke"
_ON = (
    os.environ.get("VIBEKE") == "1"
    and bool(os.environ.get("VIBEKE_SOCKET"))
    and bool(os.environ.get("VIBEKE_PANE_TOKEN"))
)
# The terminal interfaces a pane runs. ACP sessions report through Vibeke's ACP host, and a
# messaging gateway started in a pane serves other people's chats.
_PLATFORMS = {"cli", "tui"}
_MAX_TEXT = 4000

_queue: "queue.Queue[tuple[str, dict] | None]" = queue.Queue()
_worker: threading.Thread | None = None
_lock = threading.Lock()
# Sessions that are not this pane's conversation: subagents and other platforms.
_foreign: set[str] = set()


def _run() -> None:
    while True:
        item = _queue.get()
        try:
            if item is None:
                return
            hook, payload = item
            subprocess.run(
                [_BIN, "hook", "hermes", hook],
                input=json.dumps(payload, default=str).encode(),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
                check=False,
            )
        except Exception:
            pass
        finally:
            _queue.task_done()


def _flush() -> None:
    # Give the last hooks (the session end at exit) a moment to reach the server.
    if _worker is None:
        return
    done = threading.Event()

    def wait() -> None:
        _queue.join()
        done.set()

    threading.Thread(target=wait, daemon=True).start()
    done.wait(2)


def _send(hook: str, payload: dict) -> None:
    global _worker
    with _lock:
        if _worker is None:
            _worker = threading.Thread(target=_run, name="vibeke-hooks", daemon=True)
            _worker.start()
            atexit.register(_flush)
    _queue.put((hook, payload))


def _text(value) -> str | None:
    """The text of a message: a string, or the text parts of a multimodal list."""
    if isinstance(value, str):
        return value[:_MAX_TEXT]
    if isinstance(value, list):
        parts = [
            p.get("text", "")
            for p in value
            if isinstance(p, dict) and p.get("type") == "text"
        ]
        return "\n".join(parts)[:_MAX_TEXT]
    return None


def _pick(kwargs: dict, *keys: str) -> dict:
    return {k: kwargs[k] for k in keys if kwargs.get(k) is not None}


def _ours(kwargs: dict) -> bool:
    """Whether a hook belongs to this pane's conversation."""
    platform = kwargs.get("platform")
    sid = kwargs.get("session_id")
    if platform and platform not in _PLATFORMS:
        if isinstance(sid, str) and sid:
            _foreign.add(sid)
        return False
    return not (isinstance(sid, str) and sid in _foreign)


def _session(hook: str):
    def cb(**kwargs) -> None:
        if _ours(kwargs):
            _send(
                hook,
                _pick(
                    kwargs,
                    "session_id",
                    "model",
                    "platform",
                    "reason",
                    "completed",
                    "failed",
                    "interrupted",
                    "turn_exit_reason",
                ),
            )

    return cb


def _pre_llm_call(**kwargs) -> None:
    if _ours(kwargs):
        p = _pick(kwargs, "session_id", "model", "platform", "is_first_turn")
        p["prompt"] = _text(kwargs.get("user_message"))
        _send("pre_llm_call", p)


def _post_llm_call(**kwargs) -> None:
    if _ours(kwargs):
        p = _pick(kwargs, "session_id", "model", "platform")
        p["assistant_response"] = _text(kwargs.get("assistant_response"))
        _send("post_llm_call", p)


def _tool(hook: str):
    def cb(**kwargs) -> None:
        if not _ours(kwargs):
            return
        p = _pick(
            kwargs,
            "tool_name",
            "args",
            "session_id",
            "tool_call_id",
            "status",
            "error_type",
            "error_message",
            "duration_ms",
        )
        if "result" in kwargs:
            p["result"] = _text(kwargs.get("result"))
        _send(hook, p)

    return cb


def _approval(hook: str):
    def cb(**kwargs) -> None:
        # Smart approvals are decided by a model, without a prompt in the pane.
        if kwargs.get("surface") == "smart":
            return
        _send(
            hook,
            _pick(
                kwargs,
                "command",
                "description",
                "pattern_key",
                "surface",
                "tool_call_id",
                "choice",
            ),
        )

    return cb


def _human_input(hook: str):
    def cb(**kwargs) -> None:
        # Approvals are reported by the approval hooks, with their tool call.
        if kwargs.get("kind") != "approval" and _ours(kwargs):
            _send(
                hook,
                _pick(
                    kwargs, "kind", "request_id", "session_id", "platform", "prompt", "outcome"
                ),
            )

    return cb


def _subagent_start(**kwargs) -> None:
    child = kwargs.get("child_session_id")
    if isinstance(child, str) and child:
        _foreign.add(child)
    if kwargs.get("parent_session_id") not in _foreign:
        _send(
            "subagent_start",
            _pick(kwargs, "parent_session_id", "child_session_id", "child_role", "child_goal"),
        )


def _subagent_stop(**kwargs) -> None:
    if kwargs.get("parent_session_id") not in _foreign:
        _send(
            "subagent_stop",
            _pick(
                kwargs, "parent_session_id", "child_session_id", "child_status", "duration_ms"
            ),
        )


def _usage(**kwargs) -> None:
    if _ours(kwargs) and kwargs.get("usage") is not None:
        _send("post_api_request", _pick(kwargs, "session_id", "model", "provider", "usage"))


_HOOKS = {
    "on_session_start": _session("on_session_start"),
    "on_session_reset": _session("on_session_reset"),
    "on_session_end": _session("on_session_end"),
    "on_session_finalize": _session("on_session_finalize"),
    "pre_llm_call": _pre_llm_call,
    "post_llm_call": _post_llm_call,
    "pre_tool_call": _tool("pre_tool_call"),
    "post_tool_call": _tool("post_tool_call"),
    "pre_approval_request": _approval("pre_approval_request"),
    "post_approval_response": _approval("post_approval_response"),
    "on_human_input_request": _human_input("on_human_input_request"),
    "on_human_input_resolved": _human_input("on_human_input_resolved"),
    "subagent_start": _subagent_start,
    "subagent_stop": _subagent_stop,
    "post_api_request": _usage,
}


def register(ctx) -> None:
    if not _ON:
        return
    try:
        from hermes_cli.plugins import VALID_HOOKS
    except Exception:
        VALID_HOOKS = None
    for name, cb in _HOOKS.items():
        # Older Hermes versions lack some hooks; skip them instead of logging warnings.
        if VALID_HOOKS is None or name in VALID_HOOKS:
            ctx.register_hook(name, cb)
