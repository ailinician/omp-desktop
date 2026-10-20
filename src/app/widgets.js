// app/widgets.js — rail widget plugin output contract (pure, no DOM):
//   parseOutput(stdout)        plugin stdout → validated, capped widget v1 or error
//   insertText(item) / safeUrl(url) / appendToDraft(draft, insert)
//                              what a click may do
//   attentionStep({key, ack, seen, hasWidget})
//                              the attention signal: active / seen / writeAck / bounce
//   insertAction(cfg, altKey) / insertTitle(cfg) / consumePromptInsert(...)
//                              where an insert click goes (widgets.json `insert`)
//   runKey / mergeRun / cardRun / parsedLast
//                              the hook's run bookkeeping (events → card props)
//   hiddenPluginIds(tweaks, ids) / argvDisplay(arg)
//                              Settings-hidden plugins; approval-card argv text
// User guide and schema: docs/widgets.md. Which plugin runs when (triggers,
// matching, approval, scheduling) lives in Rust (src-tauri/src/widgets/);
// every output-schema rule lives here so tests/test-widgets.mjs covers it.
(function () {
  const VERSION = 1;
  const LIMITS = { blocks: 32, items: 50, rows: 50, short: 200, long: 2000 };
  const TONES = new Set(["ok", "warn", "error", "neutral", "info"]);

  const clip = (v, max) => (typeof v === "string" ? v.replace(/\u0000/g, "").slice(0, max) : "");
  const isScalar = (v) => typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v));
  const scalar = (v, max) => (isScalar(v) ? String(v).replace(/\u0000/g, "").slice(0, max) : "");
  const tone = (t) => (TONES.has(t) ? t : "neutral");
  // Numbers, or decimal numeric strings only: `Number("")` is 0 and
  // `Number("0x1F")` is 31, neither of which a plugin meant.
  const DECIMAL = /^\s*[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?\s*$/;
  const num = (v) => {
    const n = typeof v === "number" ? v : typeof v === "string" && DECIMAL.test(v) ? Number(v) : NaN;
    return Number.isFinite(n) ? n : null;
  };

  // http(s) only, no control characters, never truncated: a clipped url
  // would open a different address, so an over-long one is refused.
  function safeUrl(url) {
    if (typeof url !== "string" || url.length > LIMITS.long) return null;
    return /^https?:\/\//i.test(url) && !/[\u0000-\u001f\u007f]/.test(url) ? url : null;
  }

  // A list item's non-http(s) url is dropped (no open action, not part of
  // the insert fallback); the item itself stays.
  function item(raw) {
    if (!raw || typeof raw !== "object") return null;
    const label = scalar(raw.label, LIMITS.short);
    if (!label) return null;
    const out = { label, tone: tone(raw.tone) };
    const detail = scalar(raw.detail, LIMITS.short);
    if (detail) out.detail = detail;
    const url = safeUrl(raw.url);
    if (url) out.url = url;
    const insert = clip(raw.insert, LIMITS.long);
    if (insert) out.insert = insert;
    const time = parseTime(raw.time);
    if (time != null) out.time = time;
    return out;
  }

  // parseTime: ISO 8601 date-time with an explicit zone (`Z` or `±HH:MM`),
  // or epoch seconds (number) → epoch ms. Parsed by hand so the result does
  // not depend on the engine's lenient Date.parse; impossible calendar
  // dates (2026-02-30) and anything outside the Date range → null.
  const ISO_TIME = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(\.\d+)?)?(?:Z|([+-])(\d{2}):(\d{2}))$/;
  function parseIso(s) {
    const m = ISO_TIME.exec(s);
    if (!m) return NaN;
    const [y, mo, d, h, mi, sec] = m.slice(1, 7).map(v => Number(v ?? 0));
    const offH = Number(m[9] ?? 0), offM = Number(m[10] ?? 0);
    if (mo < 1 || mo > 12 || d < 1 || h > 23 || mi > 59 || sec > 59 || offH > 23 || offM > 59) return NaN;
    // setUTCFullYear, not Date.UTC: the latter maps years 0–99 to 19xx.
    const date = new Date(0);
    date.setUTCFullYear(y, mo - 1, d);
    if (date.getUTCDate() !== d) return NaN;
    const day = date.getTime();
    const frac = m[7] ? Math.floor(Number(m[7]) * 1000) : 0;
    const off = (m[8] === "-" ? -1 : 1) * (offH * 60 + offM) * 60_000;
    return day + ((h * 60 + mi) * 60 + sec) * 1000 + frac - off;
  }
  function parseTime(v) {
    const ms = typeof v === "number" ? v * 1000 : typeof v === "string" ? parseIso(v) : NaN;
    return Number.isFinite(ms) && Math.abs(ms) <= 8.64e15 ? ms : null;
  }

  // One normaliser per block type; `null` = skip the block. Unknown fields
  // are never copied.
  const BLOCKS = Object.assign(Object.create(null), {
    status: (b) => { const label = scalar(b.label, LIMITS.short), value = scalar(b.value, LIMITS.short); return label || value ? { type: "status", label, value, tone: tone(b.tone) } : null; },
    number: (b) => { const value = num(b.value); if (value === null) return null; const out = { type: "number", label: scalar(b.label, LIMITS.short), value }; const unit = clip(b.unit, LIMITS.short); if (unit) out.unit = unit; return out; },
    progress: (b) => { const value = num(b.value); if (value === null) return null; const max = Math.max(1, num(b.max) ?? 1); return { type: "progress", label: scalar(b.label, LIMITS.short), value: Math.min(Math.max(0, value), max), max }; },
    text: (b) => { const text = clip(b.text, LIMITS.long); if (!text) return null; const out = { type: "text", text }; const insert = clip(b.insert, LIMITS.long); if (insert) out.insert = insert; return out; },
    kv: (b) => { const rows = (Array.isArray(b.rows) ? b.rows : []).filter(r => r && isScalar(r.k) && isScalar(r.v)).slice(0, LIMITS.rows).map(r => ({ k: scalar(r.k, LIMITS.short), v: scalar(r.v, LIMITS.short) })); if (!rows.length) return null; const out = { type: "kv", rows }; const title = clip(b.title, LIMITS.short); if (title) out.title = title; return out; },
    list: (b) => { const items = (Array.isArray(b.items) ? b.items : []).map(item).filter(Boolean).slice(0, LIMITS.items); if (!items.length) return null; const out = { type: "list", items }; const title = clip(b.title, LIMITS.short); if (title) out.title = title; const limit = Math.min(Math.floor(num(b.limit) ?? 0), LIMITS.items); if (limit >= 1) out.limit = limit; return out; },
    // A non-http(s) url keeps the block as plain text (no `url`, no button).
    link: (b) => {
      const url = safeUrl(b.url);
      const label = scalar(b.label, LIMITS.short) || clip(url ?? b.url, LIMITS.short);
      if (!label) return null;
      return url ? { type: "link", label, url } : { type: "link", label };
    },
  });

  // Rust hands over stdout only once it parsed as JSON (otherwise the run is
  // an error carrying a redacted preview), so a parse failure here names no
  // plugin text.
  function parseOutput(stdout) {
    let doc;
    try { doc = JSON.parse(stdout); } catch {
      return { ok: false, error: "stdout is not JSON" };
    }
    if (!doc || typeof doc !== "object" || Array.isArray(doc)) return { ok: false, error: "stdout is not a JSON object" };
    if (typeof doc.version !== "number") return { ok: false, error: `plugin reports an invalid version, this desktop supports ${VERSION}` };
    if (doc.version !== VERSION) return { ok: false, error: `plugin reports version ${doc.version}, this desktop supports ${VERSION}` };
    if (!Array.isArray(doc.blocks)) return { ok: false, error: "output has no \"blocks\" array" };
    let skipped = 0;
    const blocks = [];
    for (const raw of doc.blocks.slice(0, LIMITS.blocks)) {
      const norm = raw && typeof raw === "object" && typeof raw.type === "string" && Object.hasOwn(BLOCKS, raw.type) ? BLOCKS[raw.type](raw) : null;
      if (norm) blocks.push(norm); else skipped++;
    }
    const attention = clip(doc.attention, LIMITS.short);
    const widget = { status: tone(doc.status), blocks, skipped };
    if (attention) widget.attention = attention;
    return { ok: true, widget };
  }

  function insertText(it) {
    const explicit = clip(it?.insert, LIMITS.long);
    if (explicit) return explicit;
    return [clip(it?.label, LIMITS.short), safeUrl(it?.url)].filter(Boolean).join(" ").slice(0, LIMITS.long);
  }

  // attentionStep: one card's attention for what it shows. `key` is the
  // plugin's `attention` string ("" = none), `ack` the key the user last
  // acknowledged, `seen` the key this card showed on its previous step,
  // `hasWidget` whether the card shows a widget at all (a running rerun keeps
  // the old one; idle and error show none).
  // active: a key the user has not acknowledged. Returns `seen` to remember,
  // `writeAck` (true: store null, the key is gone so the same key alerts
  // again when it returns) and `bounce` (active and new since `seen`: a rerun
  // with the same key does not bounce twice). Without a widget nothing was
  // said about attention either way: no signal, `seen` and the ack stay.
  function attentionStep({ key, ack, seen, hasWidget }) {
    if (!hasWidget) return { active: false, seen: seen ?? "", writeAck: false, bounce: false };
    const k = key || "";
    const active = k !== "" && k !== (ack ?? null);
    return { active, seen: k, writeAck: k === "" && (ack ?? null) !== null, bounce: active && k !== (seen || "") };
  }

  // The acknowledged key lives in the tweak store under its own prefix, not
  // `widget:`: Settings → Rail treats every `widget:` key as a toggle. A tab
  // plugin's ack is per plugin id and project (`scopeId`: the project path),
  // so another tab never overwrites it; the path is hashed (two 32-bit FNV-1a
  // runs, 16 hex chars) to bound the key. A global plugin passes no scopeId.
  const fnv = (str, seed) => {
    let h = seed;
    for (let i = 0; i < str.length; i++) { h ^= str.charCodeAt(i); h = Math.imul(h, 16777619); }
    return (h >>> 0).toString(16).padStart(8, "0");
  };
  const ackKey = (id, scopeId) =>
    scopeId == null ? `widgetAck:${id}` : `widgetAck:${id}:${fnv(scopeId, 2166136261)}${fnv(scopeId, 0x9747b28c)}`;

  // appendToDraft: a widget insert goes after the draft (never in front of
  // it, never replacing it — a `/command` draft included), separated by one
  // space unless the draft already ends in whitespace, and followed by one
  // space so typing continues. A blank insert leaves the draft as it is.
  function appendToDraft(draft, insert) {
    const text = String(insert ?? "").trim();
    const base = String(draft ?? "");
    if (!text) return base;
    if (!base.trim()) return text + " ";
    return (/\s$/.test(base) ? base : base + " ") + text + " ";
  }

  // insertAction: where one insert click puts its text. `cfg` is the plugin's
  // `insert` from widgets.json (Rust-validated: a `new-tab` always carries an
  // existing project) — never from plugin output. Anything but a complete
  // `new-tab`, or ⌥/Alt held, is the active tab's draft.
  function insertAction(cfg, altKey) {
    if (!altKey && cfg?.target === "new-tab" && typeof cfg.project === "string" && cfg.project) {
      return { target: "new-tab", project: cfg.project };
    }
    return { target: "draft" };
  }

  // insertTitle: the insert button's tooltip. A new-tab plugin names the
  // folder and the ⌥-click override.
  function insertTitle(cfg) {
    const a = insertAction(cfg, false);
    if (a.target !== "new-tab") return "insert into prompt";
    const folder = a.project.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || a.project;
    return `Click: new tab in ${folder} · ⌥-click: insert here`;
  }

  // consumePromptInsert: whether `Composer` should apply `promptInsert` now,
  // or wait (when a `new-tab` insert's `sessionId` is not in `sessionIds`
  // yet because the await continuation set `promptInsert` before the bridge
  // snapshot's render committed). Returns `{ kind: "none" }` (already seen,
  // absent, or waiting on `sessionIds`), `{ kind: "targeted", nonce,
  // sessionId }` or `{ kind: "active", nonce }`.
  function consumePromptInsert(promptInsert, seenNonce, sessionIds) {
    if (!promptInsert || promptInsert.nonce === seenNonce) return { kind: "none" };
    if (promptInsert.sessionId) {
      if (!Array.isArray(sessionIds) || !sessionIds.includes(promptInsert.sessionId)) {
        return { kind: "none" };
      }
      return { kind: "targeted", nonce: promptInsert.nonce, sessionId: promptInsert.sessionId };
    }
    return { kind: "active", nonce: promptInsert.nonce };
  }

  // Namespaced per source so #52's extension widgets can use `widget:ext:<key>`.
  const TWEAK_PREFIX = "widget:plugin:";
  const tweakKey = (id) => `${TWEAK_PREFIX}${id}`;
  // hiddenPluginIds: every plugin id whose Settings toggle is off, read from the
  // tweak keys rather than the loaded config — a plugin missing from the
  // config (a broken reload, the first view before the snapshot) must still
  // be hidden once a reload brings it back, before the hook re-sends the view.
  // Ids of `pluginIds` (the current config) come first, in its order, so the
  // backend's cap on hidden ids never drops a loaded plugin for stale keys.
  function hiddenPluginIds(tweaks, pluginIds) {
    const off = new Set();
    for (const [k, v] of Object.entries(tweaks ?? {})) {
      if (v === false && k.startsWith(TWEAK_PREFIX) && k.length > TWEAK_PREFIX.length) off.add(k.slice(TWEAK_PREFIX.length));
    }
    const known = (pluginIds ?? []).filter(id => off.delete(id));
    return known.concat([...off].sort());
  }

  // argvDisplay: one command element as the approval card shows it — quoted,
  // with control, invisible and direction-changing characters escaped, so a
  // newline cannot pose as an argument boundary, an empty argument stays
  // visible and a bidi override cannot reorder what the user reads.
  const INVISIBLE = /[\u007f-\u009f\u00ad\u061c\u180e\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u2069\ufeff\ufff9-\ufffb]/g;
  const argvDisplay = (arg) => JSON.stringify(String(arg ?? ""))
    .replace(INVISIBLE, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);

  // ── run bookkeeping (RunView events → card props) ─────────────────────
  // A RunView's seq grows by one per start; within one seq the phase only
  // moves waiting → running → idle. Ranking on (seq, phase) lets the initial
  // snapshot and live events arrive in any order without an older state
  // overwriting a newer one.
  const PHASE_RANK = { waiting: 0, running: 1, idle: 2 };
  const runKey = (key, pluginId) => `${key}\0${pluginId}`;
  // mergeRun: store `run` in `runs` (Map runKey → RunView) unless the stored
  // one is newer. Mutates `runs`; returns whether it changed.
  function mergeRun(runs, run) {
    if (!run || typeof run.key !== "string" || typeof run.pluginId !== "string" || typeof run.seq !== "number") return false;
    const k = runKey(run.key, run.pluginId);
    const cur = runs.get(k);
    if (cur && (run.seq < cur.seq || (run.seq === cur.seq && (PHASE_RANK[run.phase] ?? 0) < (PHASE_RANK[cur.phase] ?? 0)))) return false;
    runs.set(k, run);
    return true;
  }
  // parsedLast: parseOutput of the run's last stdout, cached per run key by
  // the finish it came from (`last.at`), so neither a re-render nor a phase
  // change of the next run re-parses the same output. Mutates `cache`.
  function parsedLast(cache, k, last) {
    if (!last || last.stdout == null) { cache.delete(k); return null; }
    const hit = cache.get(k);
    if (hit && hit.at === last.at) return hit.parsed;
    const parsed = parseOutput(last.stdout);
    cache.set(k, { at: last.at, parsed });
    return parsed;
  }
  // cardRun: RunView (undefined = never ran) plus its parsed stdout →
  // WidgetCard's `run` prop. A run in flight (waiting or running) keeps the
  // previous good widget so the card shows it dimmed.
  function cardRun(run, parsed) {
    const last = run?.last ?? null;
    const base = { durationMs: last?.durationMs ?? null, at: last?.at ?? null, lastOkAt: run?.lastOkAt ?? null };
    const result = !last ? null : last.error != null ? { ok: false, error: last.error } : parsed ?? { ok: false, error: "plugin printed no output" };
    if (run && (run.phase === "running" || run.phase === "waiting")) {
      return { state: "running", widget: result?.ok ? result.widget : null, error: null, ...base };
    }
    if (!result) return { state: "idle", widget: null, error: null, ...base };
    return result.ok
      ? { state: "ok", widget: result.widget, error: null, ...base }
      : { state: "error", widget: null, error: result.error, ...base };
  }

  // relativeTime: compact label for a past/future instant. Under a minute is
  // "now"; whole units are floored; more than 30 days away is the UTC date.
  function relativeTime(epochMs, nowMs) {
    const abs = Math.abs(epochMs - nowMs);
    if (abs < 60_000) return "now";
    if (abs > 30 * 86_400_000) return new Date(epochMs).toISOString().slice(0, 10);
    const [n, unit] = abs < 3_600_000 ? [abs / 60_000, "m"] : abs < 86_400_000 ? [abs / 3_600_000, "h"] : [abs / 86_400_000, "d"];
    return `${epochMs > nowMs ? "in " : ""}${Math.floor(n)}${unit}`;
  }

  window.OMP_WIDGETS = {
    parseOutput, insertText, safeUrl, appendToDraft, insertAction, insertTitle, consumePromptInsert, tweakKey, hiddenPluginIds, argvDisplay, relativeTime,
    attentionStep, ackKey, runKey, mergeRun, parsedLast, cardRun,
  };
})();
