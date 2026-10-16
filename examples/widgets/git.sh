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
# A partial clone fetches missing objects through the project's remote config
# (core.sshCommand, protocol.*.allow): never fetch (git >= 2.44).
export GIT_NO_LAZY_FETCH=1
cd "${OMP_WIDGET_PROJECT:-.}"
# -I: never import from the (untrusted) project cwd. Bytes are decoded leniently
# so a non-UTF-8 commit subject cannot abort the script.
json_escape() { python3 -I -c 'import json,sys; print(json.dumps(sys.stdin.buffer.read().decode("utf-8","replace").rstrip("\n")))' ; }
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf '{"version":1,"status":"neutral","blocks":[{"type":"text","text":"not a git repository"}]}\n'; exit 0
fi
# `git status` cleans files it re-reads through the filter driver the project's
# attributes name, i.e. runs `filter.<driver>.clean`/`.process` from git config.
# Empty every driver the project's own config (.git/config, its includes,
# config.worktree) touches; drivers only the user's global/system config defines
# (git-lfs) stay, or their files would count as changed. GIT_CONFIG_* pairs: a
# driver name may hold `=` or `.`; an empty command is no filter, and `required`
# would abort. Exit 1 = no filter configured; any other failure (git < 2.26 has
# no --show-scope) leaves the drivers unknown, so stop before `git status`.
rc=0; filter_keys=$(git config --show-scope --name-only --get-regexp '^filter\.') || rc=$?
if [ "$rc" -gt 1 ]; then echo "git.sh: cannot list git filter drivers (git config exit $rc)" >&2; exit 1; fi
n=${GIT_CONFIG_COUNT:-0}; seen=$'\n'
while IFS= read -r line; do
  scope=${line%%$'\t'*}; key=${line#*$'\t'}
  case $scope in system|global|command|'') continue ;; esac
  driver=${key#filter.}; driver=${driver%.*}
  case $seen in *$'\n'"$driver"$'\n'*) continue ;; esac
  seen+="$driver"$'\n'
  for setting in clean= process= required=false; do
    export "GIT_CONFIG_KEY_$n=filter.$driver.${setting%%=*}" "GIT_CONFIG_VALUE_$n=${setting#*=}"
    n=$((n + 1))
  done
done <<<"$filter_keys"
export GIT_CONFIG_COUNT=$n
if ! branch=$(git symbolic-ref --quiet --short HEAD 2>/dev/null); then
  branch="detached @ $(git rev-parse --short HEAD 2>/dev/null || echo '?')"
fi
# --ignore-submodules=dirty: a dirty-check runs `git status` inside each
# submodule, under that submodule's own filter drivers.
changed=$(git status --porcelain --ignore-submodules=dirty | wc -l | tr -d ' ')
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
done < <(git log --no-show-signature --format='%h%x09%s' -n 10 "${up:+$up..}HEAD" 2>/dev/null)
printf '{"version":1,"status":"%s","blocks":[' "$tone"
printf '{"type":"kv","rows":[{"k":"branch","v":%s},{"k":"upstream","v":%s}]},' "$(printf %s "$branch" | json_escape)" "$(printf %s "$upstream" | json_escape)"
printf '{"type":"number","label":"changed files","value":%s}' "$changed"
[ -n "$up" ] && printf ',{"type":"status","label":"ahead / behind","value":"%s / %s","tone":"%s"}' "$ahead" "$behind" "$tone"
[ -n "$items" ] && printf ',{"type":"list","title":"%s","items":[%s]}' "$list_title" "$items"
printf ']}\n'
