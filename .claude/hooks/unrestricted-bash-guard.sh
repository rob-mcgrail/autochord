#!/usr/bin/env bash
# PreToolUse hook for the Bash tool. Reads the proposed command + description
# from stdin (Claude Code passes the tool call as JSON), asks Haiku to judge
# it against .claude/unrestricted-bash-policy.md, and exits:
#   0  → ALLOW (Claude proceeds)
#   2  → DENY  (Claude is told to stop; reason printed to stderr)
#
# Verdicts are cached for an hour, keyed by (policy_hash + command). Repeats
# of common commands (`bun test`, `git status`, ...) skip the model call.
#
# The hook is silent on allow so it doesn't pollute Claude's terminal.

set -u

# Resolve the project Claude is operating in. CLAUDE_PROJECT_DIR is set by the
# harness; fall back to $PWD for ad-hoc / piped testing. The script itself can
# live anywhere — same hook works across every repo when installed at user
# level (~/.claude/hooks/).
PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$PWD}"
POLICY_FILE="$PROJECT_DIR/.claude/unrestricted-bash-policy.md"

# Namespace the log + cache per project so concurrent Claude sessions don't
# interleave and policy edits in one project don't poison another's cache.
project_slug="$(basename "$PROJECT_DIR" | tr -c 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//')"
[ -z "$project_slug" ] && project_slug="root"
LOG_FILE="/tmp/claude-unrestricted-bash-guard-$project_slug.log"
CACHE_DIR="/tmp/claude-bash-guard-cache-$project_slug"
CACHE_TTL_SECONDS=3600   # 1 hour

# Read the tool-call JSON from stdin.
input="$(cat)"

# Pull out the command + description. jq is the only external dep here.
command="$(printf '%s' "$input" | jq -r '.tool_input.command // ""')"
description="$(printf '%s' "$input" | jq -r '.tool_input.description // ""')"

# Empty / unparseable input → allow (don't block on a malformed event).
if [ -z "$command" ]; then
  exit 0
fi

policy="$(cat "$POLICY_FILE" 2>/dev/null || echo '(no policy file found — allowing all)')"

# Cache key: policy fingerprint + the literal command. Description is NOT
# included — the verdict depends on the command itself, not on Claude's prose
# phrasing of what it's for. Including the policy fingerprint means any edit
# to the policy file invalidates every cached verdict automatically.
policy_hash="$(printf '%s' "$policy" | shasum -a 256 | cut -d' ' -f1 | head -c 16)"
cmd_hash="$(printf '%s\n%s' "$policy_hash" "$command" | shasum -a 256 | cut -d' ' -f1)"
mkdir -p "$CACHE_DIR" 2>/dev/null
cache_file="$CACHE_DIR/$cmd_hash"

# stat -f for BSD/macOS, -c for GNU/Linux. Fall through to 0 (treat as no file).
file_mtime() {
  stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

verdict=""
cache_state="miss"
if [ -f "$cache_file" ]; then
  age=$(( $(date +%s) - $(file_mtime "$cache_file") ))
  if [ "$age" -ge 0 ] && [ "$age" -lt "$CACHE_TTL_SECONDS" ]; then
    verdict="$(cat "$cache_file" 2>/dev/null)"
    cache_state="hit"
  else
    cache_state="stale"
  fi
fi

if [ -z "$verdict" ]; then
  prompt="You are a guard for the Bash tool running in unrestricted mode.
Decide if the proposed command should be ALLOWED or DENIED.

POLICY (the user's list of things they don't want to happen):
$policy

PROPOSED COMMAND:
$command

DESCRIPTION (what Claude says it's for):
$description

Reply with EXACTLY one line. Either:
  ALLOW
or:
  DENY: <one short sentence explaining which policy rule it hits>

Do not preamble. Do not explain unless denying."

  verdict="$(printf '%s' "$prompt" | claude -p --model claude-haiku-4-5-20251001 2>/dev/null | head -n1)"

  # Cache the verdict (only if non-empty — empty implies API/network failure
  # and we don't want to memoize "I couldn't reach the model").
  if [ -n "$verdict" ]; then
    printf '%s' "$verdict" > "$cache_file" 2>/dev/null
  fi
fi

# Log every decision so the user can tail / refine the policy.
{
  echo "---"
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ)  [$cache_state] $verdict"
  echo "    cmd: $command"
  [ -n "$description" ] && echo "    desc: $description"
} >> "$LOG_FILE" 2>/dev/null

# If the model is unreachable / returns nothing, fail CLOSED (deny). A
# security check that fails open isn't a security check; it's a status light.
# Better to interrupt the session on a network blip and let the user retry
# than to silently wave through a command the guard never actually saw.
if [ -z "$verdict" ]; then
  {
    echo "Blocked by unrestricted-bash-guard: empty verdict from \`claude -p\` (model: claude-haiku-4-5-20251001)."
    echo "  The guard couldn't reach the model to evaluate this command — failing closed."
    echo "  Likely cause: network blip, claude CLI not on PATH, transient API failure."
    echo "  Command:  $command"
    echo "  policy:   $POLICY_FILE"
    echo "  log:      $LOG_FILE"
    echo "  retry:    rerun the same command and the guard will try again."
  } >&2
  exit 2
fi

case "$verdict" in
  ALLOW*)
    exit 0
    ;;
  DENY*)
    reason="${verdict#DENY:}"
    reason="${reason# }"
    echo "Blocked by unrestricted-bash-guard: ${reason:-no reason given}" >&2
    echo "  policy: $POLICY_FILE" >&2
    echo "  log:    $LOG_FILE" >&2
    exit 2
    ;;
  *)
    # Unparseable — fail CLOSED. If the model returned something other
    # than "ALLOW" / "DENY: ..." the prompt or model is misbehaving; we
    # can't infer intent and shouldn't pretend a wave-through was decided.
    {
      echo "Blocked by unrestricted-bash-guard: unparseable verdict — neither ALLOW nor DENY."
      echo "  Got: $verdict"
      echo "  The guard's reply is malformed; failing closed rather than guessing."
      echo "  Command:  $command"
      echo "  policy:   $POLICY_FILE"
      echo "  log:      $LOG_FILE"
    } >&2
    exit 2
    ;;
esac
