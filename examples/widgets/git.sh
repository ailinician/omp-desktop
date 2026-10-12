#!/usr/bin/env bash
# omp-desktop rail widget: branch, upstream delta, changed files, unpushed commits.
# Contract: docs/widgets.md. Needs git; prints JSON v1 to stdout.
set -euo pipefail
cd "${OMP_WIDGET_PROJECT:-.}"
json_escape() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().rstrip("\n")))' ; }
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf '{"version":1,"status":"neutral","blocks":[{"type":"text","text":"not a git repository"}]}\n'; exit 0
fi
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "detached")
changed=$(git status --porcelain | wc -l | tr -d ' ')
ahead=0; behind=0; tone="neutral"; upstream="(no upstream)"
if up=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null); then
  upstream=$up
  read -r behind ahead < <(git rev-list --left-right --count "$up...HEAD")
  [ "$ahead" -gt 0 ] && tone="warn" || tone="ok"
fi
items=""
while IFS=$'\t' read -r sha subject; do
  [ -z "$sha" ] && continue
  label=$(printf '%s %s' "$sha" "$subject" | json_escape)
  items+="${items:+,}{\"label\":$label,\"insert\":$label}"
done < <(git log --format='%h%x09%s' -n 10 "${up:+$up..}HEAD" 2>/dev/null)
printf '{"version":1,"status":"%s","blocks":[' "$tone"
printf '{"type":"kv","rows":[{"k":"branch","v":%s},{"k":"upstream","v":%s}]},' "$(printf %s "$branch" | json_escape)" "$(printf %s "$upstream" | json_escape)"
printf '{"type":"number","label":"changed files","value":%s},' "$changed"
printf '{"type":"status","label":"ahead / behind","value":"%s / %s","tone":"%s"}' "$ahead" "$behind" "$tone"
[ -n "$items" ] && printf ',{"type":"list","items":[%s]}' "$items"
printf ']}\n'
