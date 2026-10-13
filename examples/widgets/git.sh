#!/usr/bin/env bash
# omp-desktop rail widget: branch, upstream delta, changed files, unpushed commits.
# Contract: docs/widgets.md. Needs git and python3; prints JSON v1 to stdout.
set -euo pipefail
# Never take .git/index.lock: the agent may be running `git add`/`commit` here.
export GIT_OPTIONAL_LOCKS=0
# Never run a command named by the (untrusted) project's git config, and never
# treat a bare-repo-shaped project directory as a repository: its `config`
# could set core.fsmonitor, which `git status` executes.
git() { command git -c safe.bareRepository=explicit -c core.fsmonitor=false "$@"; }
cd "${OMP_WIDGET_PROJECT:-.}"
# -I: never import from the (untrusted) project cwd. Bytes are decoded leniently
# so a non-UTF-8 commit subject cannot abort the script.
json_escape() { python3 -I -c 'import json,sys; print(json.dumps(sys.stdin.buffer.read().decode("utf-8","replace").rstrip("\n")))' ; }
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf '{"version":1,"status":"neutral","blocks":[{"type":"text","text":"not a git repository"}]}\n'; exit 0
fi
if ! branch=$(git symbolic-ref --quiet --short HEAD 2>/dev/null); then
  branch="detached @ $(git rev-parse --short HEAD 2>/dev/null || echo '?')"
fi
changed=$(git status --porcelain | wc -l | tr -d ' ')
ahead=0; behind=0; tone="neutral"; upstream="(no upstream)"; up=""
if up=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null); then
  upstream=$up
  read -r behind ahead < <(git rev-list --left-right --count "$up...HEAD")
  if [ "$ahead" -gt 0 ]; then tone="warn"; else tone="ok"; fi
  list_title="unpushed commits"
else
  up=""
  list_title="recent commits (no upstream)"
fi
items=""
while IFS=$'\t' read -r sha subject; do
  [ -z "$sha" ] && continue
  label=$(printf '%s %s' "$sha" "$subject" | json_escape)
  items+="${items:+,}{\"label\":$label,\"insert\":$label}"
done < <(git log --format='%h%x09%s' -n 10 "${up:+$up..}HEAD" 2>/dev/null)
printf '{"version":1,"status":"%s","blocks":[' "$tone"
printf '{"type":"kv","rows":[{"k":"branch","v":%s},{"k":"upstream","v":%s}]},' "$(printf %s "$branch" | json_escape)" "$(printf %s "$upstream" | json_escape)"
printf '{"type":"number","label":"changed files","value":%s}' "$changed"
[ -n "$up" ] && printf ',{"type":"status","label":"ahead / behind","value":"%s / %s","tone":"%s"}' "$ahead" "$behind" "$tone"
[ -n "$items" ] && printf ',{"type":"list","title":"%s","items":[%s]}' "$list_title" "$items"
printf ']}\n'
