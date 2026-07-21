#!/usr/bin/env bash
# Stop hook: catch the agent bailing out of work it could finish, citing
# nebulous reasons (context budget, token limits, "this is getting long",
# "should I continue", etc.). Reads the transcript, takes the last assistant
# message, asks Sonnet to judge whether it was a lazy bail. If so, returns a
# block+reason JSON to push the model to keep going.
#
# Always fails open: a hook error / network blip should never strand a stop.

set -u

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$PWD}"
project_slug="$(basename "$PROJECT_DIR" | tr -c 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//')"
[ -z "$project_slug" ] && project_slug="root"
LOG_FILE="/tmp/claude-lazy-stop-watcher-$project_slug.log"

input="$(cat)"
transcript_path="$(printf '%s' "$input" | jq -r '.transcript_path // ""')"

# Loop-breaker: track timestamps of recent BAILs for THIS working directory.
# If we've blocked it 3+ times in the last 15 minutes (across any sessions),
# give up — the nudge isn't working, let the stop through, log loudly.
BAIL_LOG="/tmp/claude-lazy-stop-watcher-$project_slug.bails"
WINDOW_SECONDS=900   # 15 minutes
MAX_BAILS_IN_WINDOW=3

now=$(date +%s)
recent_bail_count=0
if [ -f "$BAIL_LOG" ]; then
  # Each line is an epoch timestamp. Keep lines within the window.
  recent_bails="$(awk -v cutoff="$((now - WINDOW_SECONDS))" '$1+0 >= cutoff' "$BAIL_LOG" 2>/dev/null)"
  recent_bail_count="$(printf '%s' "$recent_bails" | grep -c . || echo 0)"
  # Prune the file so it doesn't grow forever.
  printf '%s\n' "$recent_bails" > "$BAIL_LOG"
fi

if [ -z "$transcript_path" ] || [ ! -f "$transcript_path" ]; then
  exit 0
fi

# Pull the last assistant message text from the JSONL transcript. Transcripts
# evolve across versions — handle both top-level .content and nested
# .message.content shapes. macOS doesn't have tac, so filter forward and tail.
last_assistant="$(jq -c 'select((.type == "assistant") or (.role == "assistant") or (.message.role == "assistant"))' "$transcript_path" 2>/dev/null \
  | tail -n1)"

if [ -z "$last_assistant" ]; then
  exit 0
fi

last_text="$(printf '%s' "$last_assistant" | jq -r '
  def textOf(c):
    if c | type == "array" then
      [c[] | select(.type == "text") | .text] | join("\n")
    else
      c // ""
    end;
  if .content then textOf(.content)
  elif .message.content then textOf(.message.content)
  else "" end
')"

if [ -z "$last_text" ]; then
  exit 0
fi

# Only the closing chunk matters — bail-outs live at the end of the message.
last_chunk="$(printf '%s' "$last_text" | tail -c 4000)"

prompt="You are watching a Claude Code agent that just finished its turn.
Your job: detect whether the agent LAZILY BAILED OUT.

A lazy bail-out is stopping while real work is within reach, citing nebulous
reasons like 'context budget,' 'token limit,' 'this is taking a lot of
tokens,' 'I'm running out of room,' 'this is getting long,' 'should I
continue,' 'I'll stop here for now,' or similar — when the user clearly just
wants the task done.

NOT a lazy bail-out:
- Stopping because the user explicitly said to stop / 'looks good' / etc.
- Stopping because the work is genuinely complete (the assistant summarized
  finished work, no obvious next step was deferred).
- A real blocker that's been clearly surfaced: a question waiting on the
  user, a permission denial, an external failure (tests broken with a
  reason, command errored, missing credentials).
- Asking a clarifying question that's genuinely needed.

Last assistant message (truncated to closing 4000 chars):

---
$last_chunk
---

Reply with EXACTLY one line. Either:
  OK
or:
  BAIL: <one short sentence, addressed to the agent, naming what it punted on>

Do not preamble. Do not explain unless flagging."

verdict="$(printf '%s' "$prompt" | claude -p --model claude-sonnet-4-6 2>/dev/null | head -n1)"

{
  echo "---"
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ)  $verdict"
  echo "    transcript: $transcript_path"
} >> "$LOG_FILE" 2>/dev/null

if [ -z "$verdict" ]; then
  exit 0
fi

case "$verdict" in
  OK*)
    exit 0
    ;;
  BAIL*)
    reason="${verdict#BAIL:}"
    reason="${reason# }"
    next_count=$((recent_bail_count + 1))
    if [ "$next_count" -gt "$MAX_BAILS_IN_WINDOW" ]; then
      # Loop-breaker: $MAX_BAILS_IN_WINDOW BAILs already in the last
      # ${WINDOW_SECONDS}s for this project. Stop nudging.
      {
        echo "    !! loop-breaker: $next_count BAILs in last ${WINDOW_SECONDS}s, allowing stop"
        echo "    bail-reason=$reason"
      } >> "$LOG_FILE" 2>/dev/null
      # Don't append this BAIL — we're letting it through, no need to count it.
      exit 0
    fi
    # Record the BAIL timestamp.
    printf '%s\n' "$now" >> "$BAIL_LOG" 2>/dev/null
    jq -nc --arg r "$reason" --argjson n "$next_count" --argjson max "$MAX_BAILS_IN_WINDOW" '{
      decision: "block",
      reason: ("You appear to have lazily bailed out (" + ($n|tostring) + "/" + ($max|tostring) + " in the last 15 min): " + $r + " Don'\''t apologize and stop again — actually do the deferred work, or surface a concrete blocker (a real question, a real failure).")
    }'
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
