# Rail widgets

## What a widget is

A widget is a small program, in any language, that the desktop runs on demand. It prints one JSON document to stdout; the desktop renders it as a card in the ambient rail using a fixed set of block types. Commands come only from your user-wide `widgets.json`; widgets cannot run code inside the app.

## Five-minute start

1. Copy `examples/widgets/hello.py` to `~/.omp-desktop/widgets/` (`mkdir -p ~/.omp-desktop/widgets && chmod +x` as needed; see [Where to keep your widgets](#where-to-keep-your-widgets)).
2. Open Tweaks → Rail. The `plugins` row shows the path of `widgets.json` (on macOS `~/Library/Application Support/dev.ohMyPi.desktop/widgets.json`). Create that file:

   ```json
   { "version": 1, "plugins": [
     { "id": "hello", "title": "Hello", "command": ["python3", "-I", "~/.omp-desktop/widgets/hello.py"] }
   ] }
   ```

3. Click "reload widgets" in the same row. The "Hello" card appears in the rail of the active tab.

A ten-line shell script works the same way:

```bash
#!/usr/bin/env bash
python3 -I - "$OMP_WIDGET_PROJECT" "$OMP_WIDGET_BRANCH" <<'PY'
import json, sys
project, branch = sys.argv[1], sys.argv[2]
print(json.dumps({"version": 1, "status": "ok", "blocks": [
    {"type": "kv", "rows": [{"k": "project", "v": project}, {"k": "branch", "v": branch}]}]}))
PY
```

## Config reference

`<app config dir>/widgets.json`:

```json
{
  "version": 1,
  "plugins": [
    {
      "id": "git",
      "title": "Git",
      "command": ["bash", "/Users/me/.config/omp-desktop/widgets/git.sh"],
      "match": { "path": "/Users/me/GIT", "remote": "gitlab.example.com" },
      "triggers": ["activate", "turn_end", "branch", "manual"],
      "timeoutMs": 10000,
      "enabled": true
    }
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `version` | yes | Must be `1`. Anything else rejects the whole file with one error card in the rail. |
| `id` | yes | `[a-z0-9-]{1,32}`, unique. Also the key of the card's Tweaks toggle (`widget:<id>`). |
| `title` | no | Shown in the card head; default = `id`. |
| `command` | yes | argv array; element 0 is a bare program name (resolved via PATH) or an absolute path; a relative path such as `./x` or `bin/x` is rejected, because the working directory is the untrusted project. No shell interpretation. A leading `~/` in any element (program or argument) is expanded to your home directory (`$HOME`, `%USERPROFILE%` on Windows) before the path check; `~user` and `~` alone are left as is. If there is no home directory the entry is reported as an error. Other paths must be absolute. |
| `match` | no | All keys optional, all given keys must match. `path`: the project path starts with this prefix. `remote`: the fetch URL of the default remote contains this substring. No `match` = applies to every project. |
| `triggers` | no | Default all four: `activate` (tab became active or project changed), `turn_end` (streaming went true→false), `branch` (branch chip changed), `manual` (refresh button). `manual` is always allowed even if omitted. |
| `timeoutMs` | no | Default 10000, clamped 1000..60000. |
| `intervalMs` | no | Absent or 0 = no periodic refresh. Clamped 15000..3600000 (15 s to 1 h). Only fires for the active tab (for `scope: "global"`: app-wide); paused while the window is hidden. |
| `enabled` | no | Default true. Disabled plugins are listed but never run. |
| `scope` | no | `"tab"` (default) or `"global"`. A `tab` plugin keeps one result per tab and refreshes when the tab becomes active. A `global` plugin keeps one result shared by all tabs: its interval timer is not restarted by a tab switch, and `activate` re-runs it only when it has no result yet or the last one is older than `intervalMs` (without `intervalMs` it runs on every `activate`, as a tab plugin does). `match` still applies, evaluated against the active tab when a run launches (an interval run or queued follow-up is skipped while it does not match); global widgets usually omit it. An unknown value rejects the entry. |

At most 32 entries are read; the rest are ignored with one error card. A malformed plugin entry is skipped with an error card for that entry; the other plugins still run. A missing `widgets.json` means no plugins and no error.

## Input

Environment variables (always set, possibly empty):

| Variable | Value |
|---|---|
| `OMP_WIDGET_VERSION` | `1` |
| `OMP_WIDGET_PROJECT` | absolute project path of the active tab |
| `OMP_WIDGET_BRANCH` | current branch or empty (detached / not a repo) |
| `OMP_WIDGET_REMOTE` | fetch URL of the default remote or empty |
| `OMP_WIDGET_SESSION` | desktop session id of the active tab |
| `OMP_WIDGET_TRIGGER` | `activate` \| `turn_end` \| `branch` \| `manual` |

The same values are written to stdin as one JSON object and stdin is closed. Plugins may ignore stdin.

```json
{"version":1,"project":"/Users/me/GIT/app","branch":"main","remote":"git@host:me/app.git","session":"s1","trigger":"manual"}
```

`branch`, `remote`, `session` and `trigger` are each bounded to 2048 bytes (cut on a character boundary), in both the environment and the stdin JSON. `project` is passed as is.

Working directory = project path (falls back to the user's home when the tab has no project). Plugins run with your environment (so `glab`, `gh`, `git` logins work) and the app's extended `PATH`.

## Output schema v1

stdout, UTF-8, one JSON document:

```json
{
  "version": 1,
  "status": "ok",
  "blocks": [
    { "type": "status",   "label": "pipeline", "value": "passed", "tone": "ok" },
    { "type": "number",   "label": "unpushed", "value": 3, "unit": "commits" },
    { "type": "progress", "label": "review",   "value": 2, "max": 5 },
    { "type": "text",     "text": "Branch is 3 ahead of origin/main." },
    { "type": "kv",       "rows": [ { "k": "branch", "v": "feat/x" } ] },
    { "type": "list",     "items": [
        { "label": "!42 Login fix", "detail": "2 approvals", "tone": "ok",
          "url": "https://…/-/merge_requests/42", "insert": "MR !42: Login fix https://…" }
    ] },
    { "type": "link",     "label": "Open pipeline", "url": "https://…" }
  ]
}
```

Top level:

| Field | Required | Meaning |
|---|---|---|
| `version` | yes | Must be the number `1`; another number → error card "plugin reports version N, this desktop supports 1"; a non-number → "plugin reports an invalid version, …". |
| `status` | no | `ok` \| `warn` \| `error` \| `info` \| `neutral` (default `neutral`); tints the card head dot. |
| `blocks` | yes | Array, max 32. |

Blocks:

| `type` | Fields |
|---|---|
| `status` | `label`, `value` (string \| number \| boolean, coerced to string), `tone` |
| `number` | `label`, `value` (number), `unit` (optional) |
| `progress` | `label`, `value`, `max` |
| `text` | `text`, `insert` (optional, ≤ 2000; renders a + button to insert the text into the composer) |
| `kv` | `title` (optional, ≤ 200), `rows`: `[{ "k" (string), "v" (string \| number \| boolean, coerced) }]` |
| `list` | `title` (optional, ≤ 200; shown with the item count), `limit` (optional, 1–50; show only the first `limit` items plus a „+N more“ button), `items`: `[{ "label" (string \| number \| boolean), "detail" (same), "tone", "url", "insert", "time" }]`; only `label` is required |
| `link` | `label`, `url` |

- An unknown block `type` is skipped; the rest renders and the card shows a small "1 unsupported block" note.
- `tone` (on `status` and list items): `ok` | `warn` | `error` | `info` | `neutral`.
- A `list` scrolls inside a 220 px area, so a long list never makes the card taller than the rail. `limit` collapses it further: only the first `limit` items show, with a "+N more" button that expands the rest until the next run.
- `url`: only `http:`/`https:` are opened (via the system browser); anything else is rendered as plain text.
- `insert`: text placed into the composer draft on click (appended to the current draft). Without `insert` the click inserts `label` plus `url` (space-separated, if present). Max 2000 chars; truncated.
- `time` (list items): ISO 8601 string (`YYYY-MM-DD`, optionally `THH:MM[:SS[.fff]]` and a `Z`/`±HH:MM` zone) or epoch seconds; anything else (e.g. `"March 7"`) is dropped. Shown right of the label as a compact relative label (`now` under a minute, `5m`, `3h`, `2d`, future `in 12m`, a `YYYY-MM-DD` UTC date beyond 30 days); the tooltip is the ISO timestamp.
- `label`/`value`/`detail`/`v` accept a string, finite number or boolean and coerce to string; `null` and objects stay invalid and are dropped.
- Strings are rendered as text, never as HTML or markdown.
- Limits: `label`/`value`/`detail`/`k`/`v`/`title` ≤ 200 chars (truncated), `text` ≤ 2000, list ≤ 50 items, kv ≤ 50 rows. Caps are enforced in the JS parser, not in Rust.
- Process limits: stdout is capped at 256 KiB; the timeout is `timeoutMs`. On timeout or cap the plugin's whole process group is killed.

### Conventions

- A list item is the clickable status row — use it for MRs, CI jobs, branches or any row the user can act on.
- Empty state: use a `text` block ("No open MRs.", "Pipeline never run.") rather than an empty `list` or `kv`.
- One config entry per card: each plugin produces one card with up to 32 blocks.

## Errors

| Failure | Card shows |
|---|---|
| Exit ≠ 0 | last 300 chars of stderr (stdout if stderr is empty); common credential shapes (`Bearer …`, `token=…`, `ghp_…`, URL userinfo) are masked as `[REDACTED]`, other diagnostics are shown as printed |
| Timeout | `timed out after N ms` |
| stdout over the cap | `stdout exceeds 256 KiB` |
| Non-JSON stdout | `stdout is not JSON: <first 120 chars>` |
| Wrong output `version` | `plugin reports version N, this desktop supports 1` |
| Executable not found | `command not found: <argv0>` |
| Bad `widgets.json` version / not JSON | one error card titled `widgets.json`, no plugin runs |
| Malformed plugin entry | an error card for that entry; other plugins run |

The previous successful result stays visible (dimmed) while a run is in flight. An error replaces it with the error card but keeps a "last ok HH:MM" note.

## Triggers

- One run per (plugin, trigger event). A new trigger while a run is in flight for the same plugin is coalesced: at most one queued follow-up run.
- Results are per tab (global plugins: one shared result); switching tabs shows that tab's cached results and fires `activate`.
- `interval`: periodic refresh, active tab only (global plugins: one timer for the whole app). The plugin's `intervalMs` (clamped 15 s to 1 h) determines the cadence. Paused while the window is hidden; fires one run per interval-plugin on return to visible if the last run is older than the interval (a run still in flight counts as fresh). A queued interval follow-up is dropped when its tab is no longer active or the window is hidden; a queued global follow-up is dropped when the active tab no longer matches the plugin's `match`. Reloading widgets recreates the timers, so a changed `intervalMs` takes effect at once.

## Security

- Commands come only from the user-wide `widgets.json`. Projects cannot define commands, and the frontend never sends a command line to Rust, only a plugin `id`.
- No shell: `command` is an argv array. Use `["sh", "-c", "…"]` yourself if you want one.
- Timeout (default 10 s, max 60 s) kills the process group; stdout is capped at 256 KiB.
- Plugin stderr is never shown except as the error tail of a failed run.
- Only `http:`/`https:` URLs are opened. Plugin output is rendered as text only, never HTML.
- The working directory is the project, which is untrusted (a cloned repo can contain any file). Run interpreters in isolated mode so project files are never imported: `python3 -I` (ignores the cwd, `PYTHON*` variables and user site-packages; `python3 -c` and `python3 -` otherwise put the cwd on `sys.path`, so a repo's `json.py` would run). Node only loads project-local code if your script requires relative paths. Give script *arguments* as absolute paths too. The bundled `git.sh` uses `python3 -I`; `hello.py` and `mcp-tool.py` are started with an absolute script path (`python3 -I <abs path>`, `uv run … <abs path>`), so the interpreter puts the script's own directory, not the cwd, on the path.
- A plugin runs with your user's permissions and environment: only list programs you trust.

## Where to keep your widgets

Put them in `~/.omp-desktop/widgets/`, one executable per widget, and reference them as `~/.omp-desktop/widgets/<name>` in `widgets.json`.

- Keep them out of project repos: the project is untrusted and widgets run with your permissions.
- `chmod +x` each script and give it a shebang (or start it through an interpreter, e.g. `["python3", "-I", "~/.omp-desktop/widgets/x.py"]`).

## Examples

`examples/widgets/` (`widgets.example.json` wires all four with `~/.omp-desktop/widgets/...` paths; copy the example scripts there first):

- `git.sh`: branch, upstream, changed files, ahead/behind, unpushed commits (click `+` to insert `<sha> <subject>`). Needs `bash`, `git` and `python3` (for JSON escaping).
- `hello.py`: Python 3, standard library only; echoes the context as key/value rows.
- `hello.mjs`: the same in Node ≥ 18.
- `mcp-tool.py`: calls one MCP tool over stdio and shows its text result; run with `uv run --with mcp`.

## MCP

The desktop knows nothing about MCP. A plugin that wants an MCP tool is itself the MCP client: it starts the server, calls the tool and prints the result as widget JSON. `mcp-tool.py` shows the pattern.
