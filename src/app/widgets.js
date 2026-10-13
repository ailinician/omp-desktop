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
  const scalar = (v, max) => (typeof v === "string" || typeof v === "number" || typeof v === "boolean" ? String(v).replace(/\u0000/g, "").slice(0, max) : "");
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

  // parseTime: ISO 8601 string or epoch seconds (number) → epoch ms; anything
  // outside the Date range (toISOString would throw at render) → null.
  function parseTime(v) {
    const ms = typeof v === "number" ? v * 1000 : typeof v === "string" ? Date.parse(v) : NaN;
    return Number.isFinite(ms) && Math.abs(ms) <= 8.64e15 ? ms : null;
  }

  // One normaliser per block type; `null` = skip the block.
  const BLOCKS = Object.assign(Object.create(null), {
    status: (b) => { const label = clip(b.label, LIMITS.short), value = scalar(b.value, LIMITS.short); return label || value ? { type: "status", label, value, tone: tone(b.tone) } : null; },
    number: (b) => { const value = num(b.value); if (value === null) return null; const out = { type: "number", label: clip(b.label, LIMITS.short), value }; const unit = clip(b.unit, LIMITS.short); if (unit) out.unit = unit; return out; },
    progress: (b) => { const value = num(b.value); if (value === null) return null; const max = Math.max(1, num(b.max) ?? 1); return { type: "progress", label: clip(b.label, LIMITS.short), value: Math.min(Math.max(0, value), max), max }; },
    text: (b) => { const text = clip(b.text, LIMITS.long); if (!text) return null; const out = { type: "text", text }; const insert = clip(b.insert, LIMITS.long); if (insert) out.insert = insert; return out; },
    kv: (b) => { const rows = (Array.isArray(b.rows) ? b.rows : []).filter(r => r && typeof r.k === "string" && (typeof r.v === "string" || typeof r.v === "number" || typeof r.v === "boolean")).slice(0, LIMITS.rows).map(r => ({ k: clip(r.k, LIMITS.short), v: clip(String(r.v), LIMITS.short) })); if (!rows.length) return null; const out = { type: "kv", rows }; const title = clip(b.title, LIMITS.short); if (title) out.title = title; return out; },
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

  window.OMP_WIDGETS = { VERSION, LIMITS, TRIGGERS, parseOutput, applicable, wantsTrigger, insertText, safeUrl, tweakKey, relativeTime };
})();
