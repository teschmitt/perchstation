#!/bin/bash
# PostToolUse hook for Bash: log cargo build/test/check/clippy/fmt/nextest
# pass/fail to .git/claude/branches/<branch>/attempts.jsonl, which is
# consumed by .claude/scripts/generate-reasoning.sh during /commit.

INPUT=$(cat)

# Always emit {} so we never break the tool call, even on internal errors.
trap 'echo "{}"; exit 0' EXIT

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""' 2>/dev/null)

# Early exit for non-Rust-build commands — keeps Bash latency negligible.
if ! [[ "$COMMAND" =~ ^[[:space:]]*cargo[[:space:]]+(build|test|check|clippy|fmt|nextest) ]]; then
    exit 0
fi

# Require git; bail silently if outside a repo.
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    exit 0
fi

# Determine exit code — try the documented field, then fall back to scanning
# Rust toolchain output for the standard failure markers.
EXIT_CODE=$(echo "$INPUT" | jq -r '.tool_response.exit_code // .tool_response.exitCode // empty' 2>/dev/null)
if [[ -z "$EXIT_CODE" ]]; then
    OUT=$(echo "$INPUT" | jq -r '(.tool_response.stderr // "") + "\n" + (.tool_response.stdout // "")' 2>/dev/null)
    if echo "$OUT" | grep -qE 'error(\[E[0-9]+\])?:|^FAILED|test result: FAILED'; then
        EXIT_CODE=1
    else
        EXIT_CODE=0
    fi
fi

CURRENT_BRANCH=$(git branch --show-current 2>/dev/null || echo "detached")
SAFE_BRANCH=$(echo "$CURRENT_BRANCH" | tr '/' '-')
ATTEMPTS_DIR="$(git rev-parse --git-dir)/claude/branches/$SAFE_BRANCH"
ATTEMPTS_FILE="$ATTEMPTS_DIR/attempts.jsonl"
mkdir -p "$ATTEMPTS_DIR"

if [[ "$EXIT_CODE" == "0" ]]; then
    TYPE="build_pass"
    ERROR=""
else
    TYPE="build_fail"
    ERROR=$(echo "$INPUT" | jq -r '.tool_response.stderr // ""' | head -c 2000)
fi

jq -n \
    --arg type "$TYPE" \
    --arg command "$COMMAND" \
    --arg error "$ERROR" \
    --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{type: $type, command: $command, error: $error, timestamp: $ts}' \
    >> "$ATTEMPTS_FILE" 2>/dev/null

exit 0
