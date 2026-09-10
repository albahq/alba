#!/usr/bin/env bash
# SessionStart hook: when the project has a Beamfile, tell Claude that Alba
# is in use and list the declared beams. The beam names come from the
# Beamfile's own text (a bare `alba` would run the default beam, and there
# is no list subcommand); `alba check` adds its summary when the binary is
# installed. Stays silent when there is no Beamfile.

set -euo pipefail

input="$(cat)"

cwd="${CLAUDE_PROJECT_DIR:-}"
if [ -z "$cwd" ]; then
  cwd="$(printf '%s' "$input" \
    | sed -n 's/.*"cwd"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n1)"
fi
[ -n "$cwd" ] || cwd="$(pwd)"

beamfile="$cwd/Beamfile"
[ -f "$beamfile" ] || exit 0

beams="$(grep -E '^[[:space:]]*beam[[:space:]]+[A-Za-z_][A-Za-z0-9_]*' "$beamfile" \
  | sed -E 's/^[[:space:]]*beam[[:space:]]+([A-Za-z_][A-Za-z0-9_]*(\([^)]*\))?).*/\1/' \
  | tr '\n' ' ')" || beams=""

context="This project uses Alba (a Beamfile is present). Use the using-alba skill to read or edit it and to run the CLI."
[ -n "$beams" ] && context="$context"$'\n'"Beams: $beams"

if command -v alba >/dev/null 2>&1; then
  summary="$(alba check --file "$beamfile" 2>&1)" || true
  [ -n "$summary" ] && context="$context"$'\n'"$summary"
else
  context="$context"$'\n'"The 'alba' binary is not installed: ask the user to install it (cargo install --path crates/alba-cli) before running beams."
fi

escape_for_json() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  s="${s//$'\n'/\\n}"
  s="${s//$'\r'/\\r}"
  s="${s//$'\t'/\\t}"
  s="$(printf '%s' "$s" | LC_ALL=C tr -d '\000-\010\013\014\016-\037')"
  printf '%s' "$s"
}

escaped="$(escape_for_json "$context")"
printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$escaped"

exit 0
