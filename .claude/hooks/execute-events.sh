#!/usr/bin/env bash
# PreToolUse hook (matcher: *). Writes one JSON line per tool call to a
# run-scoped event file the parent CLI tails to render the live TUI during
# `roadmap execute`.
#
# Active only when CLAUDE_EXECUTE_RUN_ID is set in the env (the CLI sets it
# on the spawned Claude subprocess). When unset, exits 0 silently so the
# hook is a no-op for hand-launched Claude sessions.
#
# Always fails open: any error path exits 0 so a hook glitch doesn't block
# tool calls.

set -u

if [ -z "${CLAUDE_EXECUTE_RUN_ID:-}" ]; then
  exit 0
fi

EVENTS_FILE="/tmp/claude-execute-events-${CLAUDE_EXECUTE_RUN_ID}.jsonl"

input="$(cat 2>/dev/null || true)"
if [ -z "$input" ]; then
  exit 0
fi

# Pull tool name + a small, single-line summary of the input. We keep
# arg_summary short (<= 200 chars, single line) so the parent TUI can render
# it in one row without surprises.
event="$(printf '%s' "$input" | jq -c '
  def short(s): if (s|type) == "string" then s | gsub("\\n";" ") | .[0:200] else (s|tostring|.[0:200]) end;
  def summarize(ti):
    if ti == null then ""
    elif (ti.command // null) != null then short(ti.command)
    elif (ti.file_path // null) != null then short(ti.file_path)
    elif (ti.path // null) != null then short(ti.path)
    elif (ti.url // null) != null then short(ti.url)
    elif (ti.pattern // null) != null then short(ti.pattern)
    elif (ti.query // null) != null then short(ti.query)
    elif (ti.description // null) != null then short(ti.description)
    elif (ti.subject // null) != null then short(ti.subject)
    else short(ti)
    end;
  {
    ts: (now | todate),
    tool: (.tool_name // ""),
    summary: summarize(.tool_input // null),
    session_id: (.session_id // ""),
  }
' 2>/dev/null)"

if [ -z "$event" ]; then
  exit 0
fi

# Append-only; the parent reads with a tail-style cursor so a partial write
# at most delays the next render.
printf '%s\n' "$event" >> "$EVENTS_FILE" 2>/dev/null

exit 0
