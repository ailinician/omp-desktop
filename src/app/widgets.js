// app/widgets.js — rail widget plugin contract (pure, no DOM):
//   parseOutput(stdout)  plugin stdout → validated, capped widget v1 or error
//   applicable(plugins, ctx) / wantsTrigger(plugin, trigger)  config matching
//   insertText(item) / safeUrl(url)  what a click may do
// Spec: docs/superpowers/specs/2026-10-12-rail-widgets-design.md.
// Rust (src-tauri/src/widgets/) only enforces process-level limits; every
// schema rule lives here so tests/test-widgets.mjs is the single source.
(function () {
  const VERSION = 1;
  const LIMITS = { blocks: 32, items: 50, rows: 50, short: 200, long: 2000 };
  const TONES = new Set(["ok", "warn", "error", "neutral", "info"]);
  const TRIGGERS = ["activate", "turn_end", "branch", "manual", "interval"];

  const clip = (v, max) => (typeof v === "string" ? v.replace(/\u0000/g, "").slice(0, max) : "");
  const isScalar = (v) => typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v));
  const scalar = (v, max) => (isScalar(v) ? String(v).replace(/\u0000/g, "").slice(0, max) : "");
  const tone = (t) => (TONES.has(t) ? t : "neutral");
  const num = (v) => { if (typeof v !== "number" && typeof v !== "string") return null; const n = Number(v); return Number.isFinite(n) ? n : null; };

  function safeUrl(url) {
    if (typeof url !== "string") return null;
    return /^https?:\/\//i.test(url) ? clip(url, LIMITS.long) : null;
  }

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

  // parseTime: ISO 8601 string (date, optionally with time and zone) or epoch
  // seconds (number) → epoch ms. Free-form strings ("March 7") are not
  // parsed; anything outside the Date range (toISOString would throw at
  // render) → null.
  const ISO_TIME = /^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?)?$/;
  function parseTime(v) {
    const ms = typeof v === "number" ? v * 1000 : typeof v === "string" && ISO_TIME.test(v) ? Date.parse(v) : NaN;
    return Number.isFinite(ms) && Math.abs(ms) <= 8.64e15 ? ms : null;
  }

  // One normaliser per block type; `null` = skip the block.
  const BLOCKS = Object.assign(Object.create(null), {
    status: (b) => { const label = clip(b.label, LIMITS.short), value = scalar(b.value, LIMITS.short); return label || value ? { type: "status", label, value, tone: tone(b.tone) } : null; },
    number: (b) => { const value = num(b.value); if (value === null) return null; const out = { type: "number", label: clip(b.label, LIMITS.short), value }; const unit = clip(b.unit, LIMITS.short); if (unit) out.unit = unit; return out; },
    progress: (b) => { const value = num(b.value); if (value === null) return null; const max = Math.max(1, num(b.max) ?? 1); return { type: "progress", label: clip(b.label, LIMITS.short), value: Math.min(Math.max(0, value), max), max }; },
    text: (b) => { const text = clip(b.text, LIMITS.long); if (!text) return null; const out = { type: "text", text }; const insert = clip(b.insert, LIMITS.long); if (insert) out.insert = insert; return out; },
    kv: (b) => { const rows = (Array.isArray(b.rows) ? b.rows : []).filter(r => r && typeof r.k === "string" && isScalar(r.v)).slice(0, LIMITS.rows).map(r => ({ k: clip(r.k, LIMITS.short), v: clip(String(r.v), LIMITS.short) })); if (!rows.length) return null; const out = { type: "kv", rows }; const title = clip(b.title, LIMITS.short); if (title) out.title = title; return out; },
    list: (b) => { const items = (Array.isArray(b.items) ? b.items : []).map(item).filter(Boolean).slice(0, LIMITS.items); if (!items.length) return null; const out = { type: "list", items }; const title = clip(b.title, LIMITS.short); if (title) out.title = title; return out; },
    link: (b) => { const url = safeUrl(b.url); return url ? { type: "link", label: clip(b.label, LIMITS.short) || clip(url, LIMITS.short), url } : null; },
  });

  function parseOutput(stdout) {
    let doc;
    try { doc = JSON.parse(stdout); } catch {
      return { ok: false, error: `stdout is not JSON: ${clip(String(stdout ?? ""), 120).trim()}` };
    }
    if (!doc || typeof doc !== "object") return { ok: false, error: "stdout is not a JSON object" };
    if (typeof doc.version !== "number") return { ok: false, error: `plugin reports an invalid version, this desktop supports ${VERSION}` };
    if (doc.version !== VERSION) return { ok: false, error: `plugin reports version ${doc.version}, this desktop supports ${VERSION}` };
    if (!Array.isArray(doc.blocks)) return { ok: false, error: "output has no \"blocks\" array" };
    let skipped = 0;
    const blocks = [];
    for (const raw of doc.blocks.slice(0, LIMITS.blocks)) {
      const norm = raw && typeof raw === "object" && typeof raw.type === "string" && Object.hasOwn(BLOCKS, raw.type) ? BLOCKS[raw.type](raw) : null;
      if (norm) blocks.push(norm); else skipped++;
    }
    return { ok: true, widget: { status: tone(doc.status), blocks, skipped } };
  }

  // `matchPath` is a directory prefix: "/a/b" matches "/a/b" and "/a/b/c",
  // never "/a/bc". Backslashes are normalised so a Windows config works.
  const norm = (p) => String(p ?? "").replace(/\\/g, "/").replace(/\/+$/, "");
  function pathMatches(prefix, project) {
    const a = norm(prefix), b = norm(project);
    if (!a) return /^[/\\]+$/.test(String(prefix ?? "")) && /^[/\\]/.test(String(project ?? ""));
    return b === a || b.startsWith(a + "/");
  }
  function applicable(plugins, ctx) {
    return (plugins ?? []).filter(p => p.enabled !== false
      && (!p.matchPath || pathMatches(p.matchPath, ctx?.project))
      && (!p.matchRemote || String(ctx?.remote ?? "").includes(p.matchRemote)));
  }
  function wantsTrigger(plugin, trigger) {
    if (trigger === "interval") return !!plugin?.intervalMs;
    return trigger === "manual" || (Array.isArray(plugin?.triggers) && plugin.triggers.includes(trigger));
  }
  // globalPlugins: the global-scope plugins that may run on a timer, taken
  // from the config (not from the per-tab applicable set) so the set, and
  // the timers keyed by it, do not change on a tab switch. `match` is
  // checked at launch time.
  function globalPlugins(configPlugins, tweaks) {
    return (configPlugins ?? []).filter(p => p.scope === "global" && p.enabled !== false && (tweaks?.[tweakKey(p.id)] ?? true));
  }
  // followUpAllowed: whether a queued follow-up (`trigger`) may still launch.
  // `env`: { visible, sid, activeSid, ctx } — ctx is the active tab's current
  // context, or null while it is still loading. A global plugin must match
  // that context now; a queued interval run also needs a visible window and,
  // for a tab plugin, its tab to still be active.
  function followUpAllowed(plugin, trigger, env) {
    const global = plugin?.scope === "global";
    if (global && applicable([plugin], env.ctx).length === 0) return false;
    if (trigger === "interval" && (!env.visible || (!global && env.sid !== env.activeSid))) return false;
    return true;
  }
  function insertText(it) {
    const explicit = clip(it?.insert, LIMITS.long);
    if (explicit) return explicit;
    return [clip(it?.label, LIMITS.short), safeUrl(it?.url)].filter(Boolean).join(" ").slice(0, LIMITS.long);
  }
  const tweakKey = (id) => `widget:${id}`;

  // relativeTime: compact label for a past/future instant. Under a minute is
  // "now"; whole units are floored; more than 30 days away is the UTC date.
  function relativeTime(epochMs, nowMs) {
    const abs = Math.abs(epochMs - nowMs);
    if (abs < 60_000) return "now";
    if (abs > 30 * 86_400_000) return new Date(epochMs).toISOString().slice(0, 10);
    const [n, unit] = abs < 3_600_000 ? [abs / 60_000, "m"] : abs < 86_400_000 ? [abs / 3_600_000, "h"] : [abs / 86_400_000, "d"];
    return `${epochMs > nowMs ? "in " : ""}${Math.floor(n)}${unit}`;
  }

  window.OMP_WIDGETS = { VERSION, LIMITS, TRIGGERS, parseOutput, applicable, wantsTrigger, globalPlugins, followUpAllowed, insertText, safeUrl, tweakKey, relativeTime };
})();
