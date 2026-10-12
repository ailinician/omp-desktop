# Rail widgets

## What a widget is

A widget is a small program, in any language, that the desktop runs on demand. It prints one JSON document to stdout; the desktop renders it as a card in the ambient rail using a fixed set of block types. Commands come only from your user-wide `widgets.json`; widgets cannot run code inside the app.

## Five-minute start

1. Copy `examples/widgets/hello.py` somewhere stable (or use it in place).
2. Open Tweaks → Rail. The `plugins` row shows the path of `widgets.json` (on macOS `~/Library/Application Support/dev.ohMyPi.desktop/widgets.json`). Create that file:

   ```json
   { "version": 1, "plugins": [
     { "id": "hello", "title": "Hello", "command": ["python3", "/absolute/path/to/hello.py"] }
   ] }
   ```

3. Click "reload widgets" in the same row. The "Hello" card appears in the rail of the active tab.

A ten-line shell script works the same way:

```sh
#!/bin/sh
printf '{"version":1,"blocks":[{"type":"text","text":"project: %s"}]}\n' "$OMP_WIDGET_PROJECT"
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
| `command` | yes | argv array; element 0 is resolved via PATH. No shell interpretation. `~` is **not** expanded; write absolute paths. |
| `match` | no | All keys optional, all given keys must match. `path`: the project path starts with this prefix. `remote`: the fetch URL of the default remote contains this substring. No `match` = applies to every project. |
| `triggers` | no | Default all four: `activate` (tab became active or project changed), `turn_end` (streaming went true→false), `branch` (branch chip changed), `manual` (refresh button). `manual` is always allowed even if omitted. |
| `timeoutMs` | no | Default 10000, clamped 1000..60000. |
| `enabled` | no | Default true. Disabled plugins are listed but never run. |

A malformed plugin entry is skipped with an error card for that entry; the other plugins still run. A missing `widgets.json` means no plugins and no error.

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
| `version` | yes | Must be `1`; anything else → error card "plugin reports version N, this desktop supports 1". |
| `status` | no | `ok` \| `warn` \| `error` \| `neutral` (default `neutral`); tints the card head dot. |
| `blocks` | yes | Array, max 32. |

Blocks:

| `type` | Fields |
|---|---|
| `status` | `label`, `value`, `tone` |
| `number` | `label`, `value` (number), `unit` (optional) |
| `progress` | `label`, `value`, `max` |
| `text` | `text` |
| `kv` | `rows`: `[{ "k", "v" }]` |
| `list` | `items`: `[{ "label", "detail", "tone", "url", "insert" }]`; only `label` is required |
| `link` | `label`, `url` |

- An unknown block `type` is skipped; the rest renders and the card shows a small "1 unsupported block" note.
- `tone` (on `status` and list items): `ok` | `warn` | `error` | `neutral`.
- `url`: only `http:`/`https:` are opened (via the system browser); anything else is rendered as plain text.
- `insert`: text placed into the composer draft on click (appended to the current draft). Without `insert` the click inserts `label` plus `url` (space-separated, if present). Max 2000 chars; truncated.
- Strings are rendered as text, never as HTML or markdown.
- Limits: `label`/`value`/`detail`/`k`/`v` ≤ 200 chars (truncated), `text` ≤ 2000, list ≤ 50 items, kv ≤ 50 rows. Caps are enforced in the JS parser, not in Rust.
- Process limits: stdout is capped at 256 KiB; the timeout is `timeoutMs`. On timeout or cap the plugin's whole process group is killed.

## Errors

| Failure | Card shows |
|---|---|
| Exit ≠ 0 | last 300 chars of stderr (stdout if stderr is empty) |
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
- Results are per active tab; switching tabs shows that tab's cached results and fires `activate`.
- There is no periodic polling; the refresh button fires `manual`.

## Security

- Commands come only from the user-wide `widgets.json`. Projects cannot define commands, and the frontend never sends a command line to Rust, only a plugin `id`.
- No shell: `command` is an argv array. Use `["sh", "-c", "…"]` yourself if you want one.
- Timeout (default 10 s, max 60 s) kills the process group; stdout is capped at 256 KiB.
- Plugin stderr is never shown except as the error tail of a failed run.
- Only `http:`/`https:` URLs are opened. Plugin output is rendered as text only, never HTML.
- A plugin runs with your user's permissions and environment: only list programs you trust.

## Examples

`examples/widgets/` (`widgets.example.json` wires all four; replace `/ABSOLUTE/PATH` with this repository's path):

- `git.sh`: branch, upstream, changed files, ahead/behind, unpushed commits (click `+` to insert `<sha> <subject>`). Needs `bash`, `git` and `python3` (for JSON escaping).
- `hello.py`: Python 3, standard library only; echoes the context as key/value rows.
- `hello.mjs`: the same in Node ≥ 18.
- `mcp-tool.py`: calls one MCP tool over stdio and shows its text result; run with `uv run --with mcp`.

## MCP

The desktop knows nothing about MCP. A plugin that wants an MCP tool is itself the MCP client: it starts the server, calls the tool and prints the result as widget JSON. `mcp-tool.py` shows the pattern.
