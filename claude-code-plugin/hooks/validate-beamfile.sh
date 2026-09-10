#!/usr/bin/env bash
# PostToolUse hook: validate a Beamfile right after it is edited.
# Reads the hook event JSON on stdin, extracts the edited file path, and if
# it is a Beamfile runs `alba check --file <path>`, which loads the project
# and validates it without running anything.
# Degrades gracefully: exits 0 without blocking when alba is not installed.

set -euo pipefail

input="$(cat)"

# Regex extraction rather than a JSON parser; on a miss the hook simply
# skips validation.
file_path="$(printf '%s' "$input" \
  | sed -n 's/.*"file_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  | head -n1)"

case "$(basename "$file_path" 2>/dev/null)" in
  Beamfile) ;;
  *) exit 0 ;;
esac

command -v alba >/dev/null 2>&1 || exit 0

if ! output="$(alba check --file "$file_path" 2>&1)"; then
  printf 'Beamfile validation failed (alba check):\n%s\n' "$output" >&2
  exit 2
fi

exit 0
